//! The glasses' MCU, on a thread of its own.
//!
//! Every MCU command is a write followed by a wait for the device to echo the command back,
//! and the wait has to allow for a device that takes its time: [`TIMING`] gives it 1.5 s.
//! Whoever sends a command waits that long in the worst case, and the one caller that sends
//! them often is the sidecar's glasses-brightness slider, from the render thread, once per
//! touch sample. A slow ack there froze the world to the wearer's head for every sample of the
//! drag.
//!
//! So this thread owns the MCU outright. It reads everything the MCU sends: acks, which it
//! matches to the command waiting on them, and the unprompted events -- a button, a display
//! mode toggled from the temple -- which it hands to [`Mcu::next_event`] for
//! `XrealGlasses::poll` to report. Nothing else reads the MCU. Two readers would take each
//! other's packets: an ack lost to `poll` is a command that times out, and an event lost to a
//! command's wait is a button press that never happened.
//!
//! Commands that need an answer ([`Mcu::exchange`]) still block their caller, but that is the
//! mode switch and the brightness read, which happen while the output is being built and need
//! the answer before going on. Brightness is fire and forget ([`Mcu::brightness`]): only the
//! newest of a run that queued up is sent, a step the glasses are already on is not sent at
//! all, and a refusal comes back as [`HmdEvent::BrightnessFailed`].
//!
//! ## Keeping the brightness known
//!
//! The panel's brightness changes without being asked to: the temple buttons step it, and a
//! display mode switch re-lights the panel at a level of the glasses' choosing, some time
//! after the switch is acknowledged -- the stereo mode takes a couple of seconds to appear on
//! the connector. Reading it once at a guessed moment is either too early or needlessly late.
//! So this thread reads it again [`Timing::settle`] after anything that may have moved it,
//! and every [`Timing::read_every`] after that, and passes on each change as
//! [`HmdEvent::Brightness`]. A read is 9 ms on an Air, measured, on this thread and not on
//! anyone else's. Reads are held off while the slider is moving it, whose value is newer than
//! anything a read could say.
//!
//! The thread sleeps in `poll(2)` on the MCU and on one end of a socket pair. The other end
//! is [`Mcu`]'s. A byte written to it says a command is waiting, a byte coming back says an
//! event is, and shutting it down tells the thread to finish.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{XrealGlasses, MCU_DATA_OFFSET, MCU_MSGID_OFFSET, MSG_R_BRIGHTNESS, MSG_W_BRIGHTNESS};
use super::MSG_W_DISP_MODE;
use crate::hid::HidDevice;
use crate::{HmdError, HmdEvent, Result};

/// How long things take, and how often to look. See [`TIMING`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Timing {
    /// How long a command waits for the device to echo it back.
    pub ack: Duration,
    /// How often the brightness is read when nothing is happening.
    pub read_every: Duration,
    /// How soon after something that may have moved the brightness it is read again.
    pub settle: Duration,
}

pub(super) const TIMING: Timing = Timing {
    ack: Duration::from_millis(1500),
    // Often enough that a press of a temple button shows on the slider before the wearer
    // looks down for it; each read is 9 ms of this thread's time.
    read_every: Duration::from_secs(2),
    settle: Duration::from_millis(300),
};

/// The MCU's end of the wire: the hidraw node in use, a socket in the tests.
pub(super) trait Port: Send {
    fn fd(&self) -> RawFd;
    fn write_report(&mut self, payload: &[u8]) -> Result<()>;
    /// One report, waiting at most `timeout`. `Ok(None)` is the timeout expiring.
    fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>>;
}

impl Port for HidDevice {
    fn fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn write_report(&mut self, payload: &[u8]) -> Result<()> {
        HidDevice::write_report(self, payload)
    }

    fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        HidDevice::read_report(self, buf, timeout)
    }
}

/// Something for the thread to do.
enum Command {
    /// Send a command and hand back the reply's payload, or why there was none.
    Exchange {
        msgid: u16,
        data: Vec<u8>,
        what: &'static str,
        reply: mpsc::Sender<Result<Vec<u8>>>,
    },
    /// Go to this raw brightness step. Nobody waits for it.
    Brightness(u8),
}

/// The MCU thread, from the side that owns the glasses. See the module notes.
pub(super) struct Mcu {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<HmdEvent>,
    /// Our end of the socket pair with the thread.
    wake: UnixStream,
    /// Set once the thread's end has closed: it has stopped, and there will be nothing more
    /// to wait for on `wake`.
    gone: bool,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mcu {
    pub(super) fn start<P: Port + 'static>(port: P, timing: Timing) -> Result<Mcu> {
        let io = |source| HmdError::Io {
            path: "glasses MCU thread".into(),
            source,
        };
        let (ours, theirs) = UnixStream::pair().map_err(io)?;
        // Neither end ever waits on the other: a wake-up that finds the socket full has
        // plenty of wake-ups ahead of it already.
        ours.set_nonblocking(true).map_err(io)?;
        theirs.set_nonblocking(true).map_err(io)?;
        let (commands, inbox) = mpsc::channel();
        let (outbox, events) = mpsc::channel();
        let worker = Worker {
            port,
            wake: theirs,
            events: outbox,
            timing,
            step: None,
            // Not straight away: whoever opened the glasses reads it themselves, and needs
            // the answer before going on.
            next_read: Instant::now() + timing.read_every,
            finished: false,
        };
        let thread = std::thread::Builder::new()
            .name("glasses-mcu".into())
            .spawn(move || worker.run(&inbox))
            .map_err(io)?;
        Ok(Mcu {
            commands,
            events,
            wake: ours,
            gone: false,
            thread: Some(thread),
        })
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands.send(command).map_err(|_| stopped())?;
        let _ = (&self.wake).write(&[1]);
        Ok(())
    }

    /// Send a command and wait for its reply's payload.
    pub(super) fn exchange(&self, msgid: u16, data: &[u8], what: &'static str) -> Result<Vec<u8>> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::Exchange {
            msgid,
            data: data.to_vec(),
            what,
            reply,
        })?;
        answer.recv().map_err(|_| stopped())?
    }

    /// Ask for a raw brightness step, and return without waiting for it.
    pub(super) fn brightness(&self, step: u8) -> Result<()> {
        self.send(Command::Brightness(step))
    }

    /// The next event the thread has passed over, if one is waiting.
    pub(super) fn next_event(&mut self) -> Option<HmdEvent> {
        self.events.try_recv().ok()
    }

    /// What to wait on for [`Mcu::next_event`] to have something. `None` once the thread has
    /// stopped, which would otherwise leave it readable forever.
    pub(super) fn ready_fd(&self) -> Option<RawFd> {
        (!self.gone).then(|| self.wake.as_raw_fd())
    }

    /// Swallow the wake-ups that made [`Mcu::ready_fd`] readable.
    pub(super) fn clear_ready(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            match (&self.wake).read(&mut buf) {
                Ok(0) => {
                    self.gone = true;
                    return;
                }
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }
}

impl Drop for Mcu {
    fn drop(&mut self) {
        // Joined rather than left to finish on its own time, because the thread owns the MCU's
        // file descriptor and a new handle may be about to open the same device. See
        // `XrealGlasses::open_any` on what a second live handle does.
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stopped() -> HmdError {
    HmdError::Io {
        path: "glasses MCU thread".into(),
        source: std::io::Error::new(std::io::ErrorKind::NotConnected, "it has stopped"),
    }
}

/// The thread's side: the only reader and writer of the MCU.
struct Worker<P> {
    port: P,
    wake: UnixStream,
    events: mpsc::Sender<HmdEvent>,
    timing: Timing,
    /// The brightness step the glasses are on, as far as is known: the last one they
    /// acknowledged or reported. Forgotten whenever anything else may have moved it.
    step: Option<u8>,
    /// When to read the brightness next.
    next_read: Instant,
    /// The owner has gone, noticed in the middle of a command.
    finished: bool,
}

/// What a [`Worker::wait`] ended on.
enum Waited {
    /// The MCU has a report to read.
    Report,
    /// The MCU hung up: the glasses were unplugged.
    Unplugged,
    /// The owner has gone, and is waiting for this thread to finish.
    Finish,
    /// The time ran out, or the owner sent a command.
    Nothing,
}

impl<P: Port> Worker<P> {
    fn run(mut self, commands: &mpsc::Receiver<Command>) {
        let mut buf = [0u8; 64];
        loop {
            // Until there are none, before sleeping. A command sent while the last batch was
            // being carried out had its wake-up swallowed by the wait for that batch's ack, and
            // sleeping now would leave it queued until something else happened to arrive.
            let batch: Vec<Command> = commands.try_iter().collect();
            if !batch.is_empty() {
                if !self.carry_out(batch) {
                    return;
                }
                continue;
            }
            let now = Instant::now();
            if now >= self.next_read {
                self.read_brightness();
                continue;
            }
            match self.wait(Some(self.next_read - now)) {
                Waited::Nothing => {}
                Waited::Finish => return,
                Waited::Unplugged => {
                    self.emit(HmdEvent::Disconnected);
                    return;
                }
                Waited::Report => match self.port.read_report(&mut buf, Duration::ZERO) {
                    Ok(Some(n)) => self.unprompted(&buf[..n]),
                    Ok(None) => {}
                    // What an unplug looks like from here, when it does not arrive as a hangup.
                    Err(e) => {
                        log::warn!("the glasses' MCU cannot be read ({e})");
                        self.emit(HmdEvent::Disconnected);
                        return;
                    }
                },
            }
        }
    }

    /// Sleep until the MCU or the owner has something, or `timeout` passes -- `None` waits
    /// for as long as it takes.
    ///
    /// The owner is watched even while waiting for an ack. One that has gone is not waiting
    /// for the answer, and it *is* waiting for this thread to finish: `Mcu`'s drop joins it,
    /// and a thread that only noticed once the ack had timed out would hold that up for
    /// the whole ack timeout.
    fn wait(&mut self, timeout: Option<Duration>) -> Waited {
        let mut fds = [
            libc::pollfd {
                fd: self.port.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Rounded up: rounding down turns the last fraction of a millisecond into a busy loop.
        let ms = timeout.map_or(-1, |t| t.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Waited::Nothing;
            }
            log::warn!("the glasses' MCU thread cannot wait ({e}); stopping it");
            return Waited::Finish;
        }
        let hung_up = libc::POLLHUP | libc::POLLERR;
        if fds[1].revents & (libc::POLLIN | hung_up) != 0 && !self.woken() {
            return Waited::Finish;
        }
        if fds[0].revents & hung_up != 0 {
            return Waited::Unplugged;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            return Waited::Report;
        }
        Waited::Nothing
    }

    /// Swallow the owner's wake-ups. `false` if the owner has shut its end.
    fn woken(&mut self) -> bool {
        let mut buf = [0u8; 64];
        loop {
            match self.wake.read(&mut buf) {
                Ok(0) => return false,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }

    /// Do a batch of commands in order. `false` if the owner went while they were being done.
    fn carry_out(&mut self, batch: Vec<Command>) -> bool {
        let mut batch = batch.into_iter().peekable();
        while let Some(command) = batch.next() {
            if self.finished {
                return false;
            }
            match command {
                // A slider drag sends these far faster than the glasses answer them, and
                // every one but the last is already out of date: sending them all would carry
                // on changing the brightness after the finger had stopped.
                Command::Brightness(_) if matches!(batch.peek(), Some(Command::Brightness(_))) => {}
                Command::Brightness(step) => self.brightness(step),
                Command::Exchange {
                    msgid,
                    data,
                    what,
                    reply,
                } => {
                    let _ = reply.send(self.exchange(msgid, &data, what));
                }
            }
        }
        !self.finished
    }

    fn brightness(&mut self, step: u8) {
        // Not read back in the middle of a drag: the finger is ahead of the glasses there, and
        // a reading would pull the handle back to where it was a moment ago.
        self.next_read = Instant::now() + self.timing.read_every;
        // A drag moves the finger far more often than it moves between eight steps.
        if self.step == Some(step) {
            return;
        }
        let outcome = self
            .exchange(MSG_W_BRIGHTNESS, &[step], "set brightness")
            .and_then(|reply| match reply.first() {
                // Seen on an Air: `00` for a step it took.
                None | Some(0) => Ok(()),
                Some(status) => Err(HmdError::Protocol(format!(
                    "brightness {step} refused with status {status:#04x}"
                ))),
            });
        match outcome {
            Ok(()) => self.step = Some(step),
            // Nobody left to tell.
            Err(_) if self.finished => {}
            Err(e) => {
                log::warn!("could not set glasses brightness: {e}");
                self.step = None;
                self.emit(HmdEvent::BrightnessFailed);
            }
        }
    }

    /// Read the brightness, which passes on a change as it arrives. See [`Worker::exchange`].
    fn read_brightness(&mut self) {
        self.next_read = Instant::now() + self.timing.read_every;
        if let Err(e) = self.exchange(MSG_R_BRIGHTNESS, &[], "read brightness") {
            // Not a reason to take the slider away: one unanswered read on a control that has
            // been working is a busy MCU, and the next read is two seconds off.
            if !self.finished {
                log::debug!("could not read glasses brightness ({e})");
            }
        }
    }

    /// Something may have moved the brightness: forget it, and look again shortly.
    fn brightness_moved(&mut self) {
        self.step = None;
        self.next_read = Instant::now() + self.timing.settle;
    }

    /// Send a command and wait for the device to echo its msgid back.
    ///
    /// Anything else arriving meanwhile is handled as it would have been had it arrived while
    /// idle, so a button pressed during a mode change is not lost.
    fn exchange(&mut self, msgid: u16, data: &[u8], what: &'static str) -> Result<Vec<u8>> {
        self.port.write_report(&XrealGlasses::mcu_packet(msgid, data))?;

        let deadline = Instant::now() + self.timing.ack;
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            match self.wait(Some(deadline.saturating_duration_since(Instant::now()))) {
                Waited::Report => {}
                Waited::Nothing => continue,
                Waited::Finish => {
                    self.finished = true;
                    return Err(stopped());
                }
                // Left for the idle wait to report, which it will as soon as this returns.
                Waited::Unplugged => break,
            }
            let Some(n) = self.port.read_report(&mut buf, Duration::ZERO)? else {
                continue;
            };
            if n <= MCU_MSGID_OFFSET + 1 {
                continue;
            }
            let echoed = u16::from_le_bytes([buf[MCU_MSGID_OFFSET], buf[MCU_MSGID_OFFSET + 1]]);
            if echoed != msgid {
                self.unprompted(&buf[..n]);
                continue;
            }
            // The payload of the reply, status byte first, which a read command needs and a
            // write mostly ignores.
            let payload = buf.get(MCU_DATA_OFFSET..n).unwrap_or(&[]).to_vec();
            match msgid {
                // Every read, whoever asked for it, so that a change reaches the event queue in
                // the order it was seen -- a read made by the owner and returned to it directly
                // must not be overtaken by an older one still waiting in the queue.
                MSG_R_BRIGHTNESS => {
                    if let Ok(step) = XrealGlasses::brightness_in(&payload) {
                        if self.step != Some(step) {
                            self.step = Some(step);
                            self.emit(HmdEvent::Brightness(XrealGlasses::brightness_to_unit(step)));
                        }
                    }
                }
                // The glasses re-light the panel on a mode change, and what they come back at
                // is not what they were at.
                MSG_W_DISP_MODE => self.brightness_moved(),
                _ => {}
            }
            return Ok(payload);
        }
        Err(HmdError::NoAck { what, msgid })
    }

    /// A packet nobody asked for: an event to pass on, or noise.
    fn unprompted(&mut self, packet: &[u8]) {
        if packet.len() <= MCU_MSGID_OFFSET + 1 {
            return;
        }
        let msgid = u16::from_le_bytes([packet[MCU_MSGID_OFFSET], packet[MCU_MSGID_OFFSET + 1]]);
        let Some(event) = XrealGlasses::decode_async(msgid, packet) else {
            log::trace!("MCU async push {msgid:#06x}");
            return;
        };
        // The temple buttons step the brightness themselves, and a mode change re-lights the
        // panel, so after either the step last set is no longer known.
        if matches!(
            event,
            HmdEvent::Button { .. } | HmdEvent::DisplayModeChanged(_)
        ) {
            self.brightness_moved();
        }
        self.emit(event);
    }

    fn emit(&mut self, event: HmdEvent) {
        let _ = self.events.send(event);
        // After the send, so that whoever this wakes finds the event already there.
        let _ = self.wake.write(&[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    use super::super::EVT_BUTTON_PRESSED;
    use crate::HmdButton;

    /// A datagram socket stands in for hidraw, which also hands over one report per read.
    impl Port for UnixDatagram {
        fn fd(&self) -> RawFd {
            self.as_raw_fd()
        }

        fn write_report(&mut self, payload: &[u8]) -> Result<()> {
            self.send(payload).map(|_| ()).map_err(|source| HmdError::Io {
                path: "test port".into(),
                source,
            })
        }

        fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
            self.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
                .unwrap();
            match self.recv(buf) {
                Ok(n) => Ok(Some(n)),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    Ok(None)
                }
                Err(source) => Err(HmdError::Io {
                    path: "test port".into(),
                    source,
                }),
            }
        }
    }

    /// The glasses' side of the wire.
    struct Glasses(UnixDatagram);

    impl Glasses {
        /// The next command sent, as (msgid, payload), or `None` if none comes within `wait`.
        fn command(&self, wait: Duration) -> Option<(u16, Vec<u8>)> {
            self.0.set_read_timeout(Some(wait)).unwrap();
            let mut buf = [0u8; 64];
            let n = self.0.recv(&mut buf).ok()?;
            let msgid = u16::from_le_bytes([buf[MCU_MSGID_OFFSET], buf[MCU_MSGID_OFFSET + 1]]);
            Some((msgid, buf[MCU_DATA_OFFSET..n].to_vec()))
        }

        fn say(&self, msgid: u16, data: &[u8]) {
            self.0.send(&XrealGlasses::mcu_packet(msgid, data)).unwrap();
        }
    }

    /// Reads far enough apart not to happen during a test that is not about them.
    fn connect(ack: Duration) -> (Mcu, Glasses) {
        connect_with(Timing {
            ack,
            read_every: Duration::from_secs(3600),
            settle: Duration::from_secs(3600),
        })
    }

    fn connect_with(timing: Timing) -> (Mcu, Glasses) {
        let (ours, theirs) = UnixDatagram::pair().unwrap();
        (Mcu::start(ours, timing).unwrap(), Glasses(theirs))
    }

    const SOON: Duration = Duration::from_secs(5);

    /// The next event from the thread, waiting for it the way `poll` does.
    fn event(mcu: &mut Mcu) -> Option<HmdEvent> {
        let deadline = Instant::now() + SOON;
        while Instant::now() < deadline {
            if let Some(event) = mcu.next_event() {
                return Some(event);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }

    #[test]
    fn a_command_gets_its_reply_and_an_event_in_between_is_not_lost() {
        let (mut mcu, glasses) = connect(SOON);
        let device = std::thread::spawn(move || {
            let (msgid, _) = glasses.command(SOON).expect("no command arrived");
            assert_eq!(msgid, MSG_R_BRIGHTNESS);
            // A button pressed while the read is in flight, then the answer.
            glasses.say(EVT_BUTTON_PRESSED, &[0x01]);
            glasses.say(MSG_R_BRIGHTNESS, &[0, 5]);
            glasses
        });
        let reply = mcu.exchange(MSG_R_BRIGHTNESS, &[], "read brightness").unwrap();
        assert_eq!(reply, [0, 5], "the payload, status byte and all");
        assert_eq!(
            event(&mut mcu),
            Some(HmdEvent::Button {
                button: HmdButton::BrightnessUp,
                pressed: true
            })
        );
        device.join().unwrap();
    }

    #[test]
    fn a_drag_sends_the_first_step_and_the_last_and_nothing_in_between() {
        let (mcu, glasses) = connect(SOON);
        mcu.brightness(2).unwrap();
        let first = glasses.command(SOON).expect("the first step was not sent");
        assert_eq!(first, (MSG_W_BRIGHTNESS, vec![2]));
        // The glasses are slow to answer, and the finger keeps moving meanwhile.
        for step in 3..=6 {
            mcu.brightness(step).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        glasses.say(MSG_W_BRIGHTNESS, &[]);
        assert_eq!(
            glasses.command(SOON),
            Some((MSG_W_BRIGHTNESS, vec![6])),
            "the steps the finger passed through should have been dropped"
        );
        glasses.say(MSG_W_BRIGHTNESS, &[]);
        assert_eq!(glasses.command(Duration::from_millis(200)), None);
    }

    #[test]
    fn a_step_the_glasses_are_already_on_is_not_sent_again() {
        let (mcu, glasses) = connect(SOON);
        mcu.brightness(4).unwrap();
        assert_eq!(glasses.command(SOON), Some((MSG_W_BRIGHTNESS, vec![4])));
        glasses.say(MSG_W_BRIGHTNESS, &[]);
        mcu.brightness(4).unwrap();
        assert_eq!(glasses.command(Duration::from_millis(200)), None);

        // Until the temple buttons have been at it: then nothing is known.
        glasses.say(EVT_BUTTON_PRESSED, &[0x02]);
        std::thread::sleep(Duration::from_millis(50));
        mcu.brightness(4).unwrap();
        assert_eq!(glasses.command(SOON), Some((MSG_W_BRIGHTNESS, vec![4])));
    }

    #[test]
    fn a_refused_brightness_is_reported_rather_than_waited_for() {
        let (mut mcu, glasses) = connect(Duration::from_millis(100));
        mcu.brightness(3).unwrap();
        assert!(glasses.command(SOON).is_some());
        // No answer.
        let mut seen = None;
        while let Some(event) = event(&mut mcu) {
            seen = Some(event);
            if seen == Some(HmdEvent::BrightnessFailed) {
                break;
            }
        }
        assert_eq!(seen, Some(HmdEvent::BrightnessFailed));
    }

    #[test]
    fn the_thread_says_when_there_is_an_event_to_collect() {
        let (mut mcu, glasses) = connect(SOON);
        let fd = mcu.ready_fd().expect("a running thread has something to wait on");
        glasses.say(EVT_BUTTON_PRESSED, &[0x01]);
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, SOON.as_millis() as i32) };
        assert_eq!(rc, 1, "an event arrived and nothing said so");
        mcu.clear_ready();
        assert!(mcu.next_event().is_some());
    }

    fn level(step: u8) -> HmdEvent {
        HmdEvent::Brightness(XrealGlasses::brightness_to_unit(step))
    }

    #[test]
    fn a_change_made_on_the_glasses_reaches_the_slider_when_it_is_next_read() {
        let (mut mcu, glasses) = connect_with(Timing {
            ack: SOON,
            read_every: Duration::from_millis(50),
            settle: Duration::from_millis(50),
        });
        // The temple buttons at work between reads: 6, still 6, then 3.
        for step in [6, 6, 3] {
            let (msgid, _) = glasses.command(SOON).expect("the brightness was not read");
            assert_eq!(msgid, MSG_R_BRIGHTNESS);
            glasses.say(MSG_R_BRIGHTNESS, &[0, step]);
        }
        assert_eq!(event(&mut mcu), Some(level(6)));
        assert_eq!(event(&mut mcu), Some(level(3)), "the same step read twice is not news");
    }

    #[test]
    fn a_mode_switch_is_followed_by_a_fresh_read_nobody_asked_for() {
        // The panel re-lights at a level of its own choosing after a switch, and the owner
        // cannot know when -- so the thread looks, shortly after the ack.
        let (mut mcu, glasses) = connect_with(Timing {
            ack: SOON,
            read_every: Duration::from_secs(3600),
            settle: Duration::from_millis(20),
        });
        let device = std::thread::spawn(move || {
            assert_eq!(glasses.command(SOON).map(|c| c.0), Some(MSG_W_DISP_MODE));
            glasses.say(MSG_W_DISP_MODE, &[0]);
            assert_eq!(glasses.command(SOON).map(|c| c.0), Some(MSG_R_BRIGHTNESS));
            glasses.say(MSG_R_BRIGHTNESS, &[0, 4]);
        });
        mcu.exchange(MSG_W_DISP_MODE, &[0x03], "display mode").unwrap();
        assert_eq!(event(&mut mcu), Some(level(4)));
        device.join().unwrap();
    }

    #[test]
    fn a_read_made_for_the_owner_is_not_overtaken_by_an_older_one() {
        // The owner reads the brightness itself after the stereo switch and shows what it gets
        // back. A read the thread made earlier is still queued as an event, and must arrive
        // before it, not after -- or it would put the older value back on the slider.
        let (mut mcu, glasses) = connect_with(Timing {
            ack: SOON,
            read_every: Duration::from_secs(3600),
            settle: Duration::from_millis(20),
        });
        let device = std::thread::spawn(move || {
            glasses.say(EVT_BUTTON_PRESSED, &[0x01]);
            assert_eq!(glasses.command(SOON).map(|c| c.0), Some(MSG_R_BRIGHTNESS));
            glasses.say(MSG_R_BRIGHTNESS, &[0, 2]);
            assert_eq!(glasses.command(SOON).map(|c| c.0), Some(MSG_R_BRIGHTNESS));
            glasses.say(MSG_R_BRIGHTNESS, &[0, 5]);
        });
        assert!(matches!(event(&mut mcu), Some(HmdEvent::Button { .. })));
        assert_eq!(event(&mut mcu), Some(level(2)));
        let reply = mcu.exchange(MSG_R_BRIGHTNESS, &[], "read brightness").unwrap();
        assert_eq!(reply, [0, 5]);
        assert_eq!(event(&mut mcu), Some(level(5)));
        device.join().unwrap();
    }

    #[test]
    fn nothing_is_read_back_while_the_slider_is_moving() {
        let (mcu, glasses) = connect_with(Timing {
            ack: SOON,
            read_every: Duration::from_millis(300),
            settle: Duration::from_millis(300),
        });
        let device = std::thread::spawn(move || {
            let mut reads = 0;
            while let Some((msgid, _)) = glasses.command(Duration::from_millis(150)) {
                if msgid == MSG_R_BRIGHTNESS {
                    reads += 1;
                    glasses.say(msgid, &[0, 3]);
                } else {
                    glasses.say(msgid, &[0]);
                }
            }
            reads
        });
        for i in 0..30 {
            mcu.brightness(1 + (i % 7) as u8).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(device.join().unwrap(), 0, "a read landed in the middle of the drag");
    }

    #[test]
    fn a_brightness_the_glasses_refuse_by_status_is_reported() {
        let (mut mcu, glasses) = connect(SOON);
        mcu.brightness(3).unwrap();
        assert!(glasses.command(SOON).is_some());
        glasses.say(MSG_W_BRIGHTNESS, &[0x01]);
        assert_eq!(event(&mut mcu), Some(HmdEvent::BrightnessFailed));
    }

    #[test]
    fn dropping_it_stops_the_thread() {
        let (mcu, _glasses) = connect(SOON);
        // Returns at all: `Drop` joins, so a thread that did not notice would hang here.
        drop(mcu);
    }

    #[test]
    fn dropping_it_does_not_wait_out_an_ack_that_is_not_coming() {
        let (mcu, glasses) = connect(SOON);
        mcu.brightness(3).unwrap();
        assert!(glasses.command(SOON).is_some());
        // The glasses never answer. A session rebuilding its output drops the handle here and
        // opens a new one, and should not sit through the whole ack timeout first.
        let started = Instant::now();
        drop(mcu);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?} to stop",
            started.elapsed()
        );
    }
}
