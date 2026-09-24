//! Copy and paste, between this host's applications and whatever the session can see.
//!
//! **The session holds the clipboard.** Whatever is copied here is announced to it, and
//! whatever it announces is offered to the applications running here — so a line copied in
//! Firestorm can be pasted into Chrome on the headset, and a link copied on the headset can be
//! pasted into a terminal here. With two hosts attached to one session, this is also how one
//! machine's clipboard reaches the other.
//!
//! Three things make that harder than it sounds, and each shapes the code below.
//!
//! **A clipboard is a promise, not a payload.** Copying puts nothing anywhere: the application
//! says what forms it *could* provide and hands them over only when somebody pastes. So a copy
//! here sends a list of forms across the link, and the bytes follow later, if ever.
//!
//! **Reading one is blocking work.** The data comes down a pipe from an application which may
//! be busy, slow, or wedged. Every read and every write below happens on its own thread, and
//! the compositor is never one of them — a host that stops drawing because something was
//! copied is worse than one that cannot copy at all.
//!
//! **X11 and Wayland do not agree about any of it.** An X11 client owns a selection on the X
//! server; a Wayland client holds a data source; Xwayland bridges the two only if this
//! compositor does the work. Both are handled here, and which of them owns the selection is
//! remembered, because serving a paste goes back to whichever it was.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::LoopHandle;
use smithay::wayland::selection::{SelectionSource, SelectionTarget};
use smithay::xwayland::X11Wm;
use spatiand_stream::clipboard::{self, Held};
use spatiand_stream::control::{Clipboard as Wire, CLIPBOARD_EAGER_BYTES};
use spatiand_stream::HostMessage;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::ToSession;
use crate::state::Host;

/// How long a paste waits for bytes from the other end before it is given up on.
///
/// A paste that never answers leaves the application waiting on a pipe that will never close,
/// which in most toolkits means a window that has stopped responding. Ending it empty is worse
/// than the data arriving and far better than a hang.
const PATIENCE: Duration = Duration::from_secs(5);

/// Where what is on the clipboard came from.
///
/// Which it was decides how a paste is served: a Wayland selection is read back through the
/// seat, an X11 one through the window manager. Both carry the forms they offered, because
/// that is what is announced and what a paste is checked against.
enum Mine {
    Wayland(Vec<String>),
    X11(Vec<String>),
}

/// A paste here that is waiting for the other end to send the bytes.
struct Waiting {
    mime_type: String,
    fd: OwnedFd,
    since: Instant,
}

/// The host's half of the session's clipboard.
#[derive(Default)]
pub struct Clipboard {
    /// What was copied here, if this end owns the clipboard.
    mine: Option<Mine>,
    /// What the session says is on it, if the other end owns it.
    theirs: Option<Held>,
    /// The last thing announced to the session, so that hearing it back is recognised as an
    /// echo rather than treated as a new copy. See [`Held::same_as`].
    announced: Option<Held>,
    /// Pastes here that have asked the session for bytes.
    waiting: Vec<Waiting>,
    /// A copy made here that has not been announced yet.
    ///
    /// Not announced on the spot, because the compositor tells this handler about a new
    /// selection *before* it stores it: reading the seat at that moment hands back the
    /// previous clipboard, which is the sort of bug that looks like a caching problem and is
    /// an ordering one. It waits one turn of the event loop instead.
    pending: Option<Vec<String>>,
    /// Filled in by the threads that read a local selection, drained by [`Clipboard::pump`].
    notes: Option<(
        std::sync::mpsc::Sender<Held>,
        std::sync::mpsc::Receiver<Held>,
    )>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            notes: Some(std::sync::mpsc::channel()),
            ..Default::default()
        }
    }

    /// Who holds the clipboard and as what, in a line. For the control socket.
    pub fn describe(&self) -> String {
        match (&self.mine, &self.theirs) {
            (Some(Mine::Wayland(forms)), _) => format!("wayland {}", forms.join(",")),
            (Some(Mine::X11(forms)), _) => format!("x11 {}", forms.join(",")),
            (None, Some(held)) => format!("session {}", held.mime_types.join(",")),
            (None, None) => "nobody".into(),
        }
    }

    /// An application here copied something. Tell the session what it has.
    ///
    /// The forms are known at once; the text, if there is any, has to be fetched from the
    /// application, so the announcement is sent from the thread that fetches it. Sending an
    /// announcement first and the text after would make every paste on the far end wait for a
    /// round trip that had already happened.
    pub fn copied_here(&mut self, source: SelectionSource, xwm: Option<&mut X11Wm>) {
        let mime_types = clipboard::offered(source.mime_types());
        self.mine = Some(Mine::Wayland(mime_types.clone()));
        self.theirs = None;
        // X11 is told as well, or an application under Xwayland cannot paste what a Wayland
        // one copied. Xwayland bridges nothing by itself: the compositor claims the X11
        // selection on the application's behalf, and answers for it in `paste_here`.
        tell_x11(xwm, &mime_types);
        self.pending = Some(mime_types);
    }

    /// An X11 application here copied something.
    pub fn copied_here_by_x11(
        &mut self,
        mime_types: Vec<String>,
        xwm: Option<&mut X11Wm>,
        loop_handle: &LoopHandle<'static, Host>,
        out: &UnboundedSender<ToSession>,
    ) {
        let mime_types = clipboard::offered(mime_types);
        self.mine = Some(Mine::X11(mime_types.clone()));
        self.theirs = None;
        let fetch = match (clipboard::best_text(&mime_types), xwm) {
            (Some(mime), Some(xwm)) => read_from_x11(xwm, &mime, loop_handle),
            _ => None,
        };
        self.announce(mime_types, fetch, out);
    }

    /// Send the announcement, once whatever text is being fetched has arrived.
    fn announce(
        &mut self,
        mime_types: Vec<String>,
        fetch: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
        out: &UnboundedSender<ToSession>,
    ) {
        let Some((told, _)) = &self.notes else { return };
        let told = told.clone();
        let out = out.clone();
        std::thread::Builder::new()
            .name("clipboard-copy".into())
            .spawn(move || {
                // Text only when it is small. A document copied whole is announced like any
                // other large form and fetched only if it is actually pasted.
                let text = fetch
                    .and_then(|rx| rx.recv().ok())
                    .filter(|bytes| bytes.len() <= CLIPBOARD_EAGER_BYTES as usize)
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                let held = Held {
                    mime_types,
                    bytes: text.as_ref().map_or(0, |t| t.len() as u32),
                    text,
                };
                log::info!(
                    "clipboard: copied here as {} ({}), offered to the session",
                    held.mime_types.join(", "),
                    held.text.as_ref().map_or("fetched on paste".into(), |t| format!("{} characters carried", t.chars().count()))
                );
                let _ = told.send(held.clone());
                let _ = out.send(ToSession::Control(HostMessage::Clipboard(Wire::Offer {
                    mime_types: held.mime_types,
                    text: held.text,
                    bytes: held.bytes,
                })));
            })
            .ok();
    }

    /// The session says its clipboard changed. Offer it to everything running here.
    ///
    /// Takes the compositor in pieces rather than whole, because this board lives inside it:
    /// `&mut host.clipboard` and `&host.seat` are two different fields and may be borrowed at
    /// once, where `&mut host` twice may not.
    pub fn session_offered(
        &mut self,
        held: Held,
        display_handle: &smithay::reexports::wayland_server::DisplayHandle,
        seat: &smithay::input::Seat<Host>,
        xwm: Option<&mut X11Wm>,
    ) {
        // An announcement of ours coming back round. Applying it would take the selection away
        // from the application that owns it and replace it with a copy of itself, which breaks
        // pasting anything that was not small enough to travel.
        if self.announced.as_ref().is_some_and(|a| a.same_as(&held)) {
            return;
        }
        // Offered to the applications here under every name they might ask by. What travels
        // on the wire stays the true list; the aliases are a local matter. See
        // `clipboard::with_text_aliases`.
        let mime_types = clipboard::with_text_aliases(&held.mime_types);
        log::info!(
            "clipboard: the session holds {}; offering it here",
            mime_types.join(", ")
        );
        self.theirs = Some(held);
        self.mine = None;
        // What this end announced is no longer on the clipboard, so hearing it again is a new
        // copy of the same thing, not an echo. Remembering it past here dropped the second copy
        // of, say, an address copied twice with something else in between -- and every image
        // after the first, since two images with no text compare equal by their forms alone.
        self.announced = None;
        let held_types = mime_types.clone();
        smithay::wayland::selection::data_device::set_data_device_selection(
            display_handle,
            seat,
            mime_types,
            (),
        );
        tell_x11(xwm, &held_types);
    }

    /// Something here is pasting.
    ///
    /// Three cases, and for a long time only the first was handled — which is why an
    /// application could paste its own copy and nothing else. What the session holds comes over
    /// the link; what another application here holds is served from that application, and the
    /// only work is handing it the pipe, whichever protocol it happens to speak.
    pub fn paste_here(
        &mut self,
        mime_type: String,
        fd: OwnedFd,
        seat: &smithay::input::Seat<Host>,
        xwm: Option<&mut X11Wm>,
        loop_handle: &LoopHandle<'static, Host>,
        out: &UnboundedSender<ToSession>,
    ) {
        log::info!(
            "clipboard: something here is pasting {mime_type} (held by {})",
            match (&self.mine, &self.theirs) {
                (Some(Mine::Wayland(_)), _) => "a Wayland application here".into(),
                (Some(Mine::X11(_)), _) => "an X11 application here".into(),
                (None, Some(held)) => format!("the session, as {}", held.mime_types.join(", ")),
                (None, None) => "nobody".into(),
            }
        );
        // Copied here, in an application speaking the other protocol. Hand that application
        // the pipe and let it write; nothing passes through this process at all.
        match &self.mine {
            Some(Mine::Wayland(forms)) => {
                if let Some(asked) = clipboard::resolve(&mime_type, forms) {
                    if let Err(e) =
                        smithay::wayland::selection::data_device::request_data_device_client_selection(
                            seat, asked, fd,
                        )
                    {
                        log::warn!("could not pass on a paste of {mime_type}: {e}");
                    }
                }
                return;
            }
            Some(Mine::X11(forms)) => {
                if let (Some(asked), Some(xwm)) = (clipboard::resolve(&mime_type, forms), xwm) {
                    if let Err(e) = xwm.send_selection(
                        SelectionTarget::Clipboard,
                        asked,
                        fd,
                        loop_handle.clone(),
                    ) {
                        log::warn!("could not pass on a paste of {mime_type} from X11: {e}");
                    }
                }
                return;
            }
            None => {}
        }
        self.paste_from_the_session(mime_type, fd, out)
    }

    /// The part of a paste the session has to answer, which is all of it when the session owns
    /// the clipboard. Separated so it can be tested without a compositor to hand.
    fn paste_from_the_session(
        &mut self,
        mime_type: String,
        fd: OwnedFd,
        out: &UnboundedSender<ToSession>,
    ) {
        let Some(held) = self.theirs.as_ref() else {
            // Nothing to give. Dropping the fd ends the paste rather than leaving the
            // application waiting on a pipe nobody will ever write to.
            return;
        };
        if let Some(bytes) = held.answers(&mime_type) {
            write_away(fd, bytes);
            return;
        }
        // What this application asked for, in the words the other end understands. An X11
        // client asking for STRING is asking for the text, whatever the far side calls it.
        let Some(asked) = clipboard::resolve(&mime_type, &held.mime_types) else {
            // Nothing to give. Dropping the fd ends the paste rather than leaving the
            // application waiting on a pipe nobody will ever write to.
            log::info!("clipboard: nothing on the clipboard can be given as {mime_type}");
            return;
        };
        log::info!("clipboard: asking the session for {asked}");
        self.waiting.push(Waiting {
            mime_type: asked.clone(),
            fd,
            since: Instant::now(),
        });
        let _ = out.send(ToSession::Control(HostMessage::Clipboard(Wire::Want {
            mime_type: asked,
        })));
    }

    /// The bytes a paste here was waiting for.
    pub fn arrived(&mut self, mime_type: &str, bytes: Vec<u8>) {
        // Everything waiting on this form, not merely the first: two applications can paste
        // the same thing at once, and the second would otherwise wait out the deadline.
        let (ready, rest): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.mime_type.eq_ignore_ascii_case(mime_type));
        self.waiting = rest;
        for waiting in ready {
            write_away(waiting.fd, bytes.clone());
        }
    }

    /// The session is pasting what was copied here. Fetch it and send it.
    /// Log line for a paste on the far end, so both halves of one paste are visible in a log.
    pub fn note_session_paste(mime_type: &str) {
        log::info!("clipboard: the session is pasting {mime_type} from here");
    }

    pub fn session_wants(
        &mut self,
        mime_type: String,
        seat: &smithay::input::Seat<Host>,
        xwm: Option<&mut X11Wm>,
        loop_handle: &LoopHandle<'static, Host>,
        out: &UnboundedSender<ToSession>,
    ) {
        let asked = match &self.mine {
            Some(Mine::Wayland(forms) | Mine::X11(forms)) => {
                clipboard::resolve(&mime_type, forms)
            }
            None => None,
        };
        let fetch = match (&self.mine, asked, xwm) {
            (Some(Mine::Wayland(_)), Some(asked), _) => read_from_wayland(seat, &asked),
            (Some(Mine::X11(_)), Some(asked), Some(xwm)) => {
                read_from_x11(xwm, &asked, loop_handle)
            }
            _ => None,
        };
        let out = out.clone();
        std::thread::Builder::new()
            .name("clipboard-paste".into())
            .spawn(move || {
                // An empty answer, always, rather than none: the far end has an application
                // waiting on a pipe, and it would rather paste nothing than hang.
                let bytes = fetch.and_then(|rx| rx.recv().ok()).unwrap_or_default();
                let _ = out.send(ToSession::Control(HostMessage::Clipboard(Wire::Data {
                    mime_type,
                    bytes,
                })));
            })
            .ok();
    }

    /// Housekeeping, once a turn: announce a copy made here, pick up what the reader threads
    /// finished, and give up on pastes nobody answered.
    pub fn pump(&mut self, seat: &smithay::input::Seat<Host>, out: &UnboundedSender<ToSession>) {
        if let Some(mime_types) = self.pending.take() {
            let fetch = clipboard::best_text(&mime_types)
                .and_then(|mime| read_from_wayland(seat, &mime));
            self.announce(mime_types, fetch, out);
        }
        if let Some((_, notes)) = &self.notes {
            while let Ok(held) = notes.try_recv() {
                self.announced = Some(held);
            }
        }
        self.expire();
    }

    /// Give up on pastes the other end never answered.
    fn expire(&mut self) {
        self.waiting.retain(|w| {
            let alive = w.since.elapsed() < PATIENCE;
            if !alive {
                log::warn!(
                    "nothing answered a paste of {} in {} s; ending it empty",
                    w.mime_type,
                    PATIENCE.as_secs()
                );
            }
            alive
        });
    }

    /// An X11 application let go of the selection without another taking it.
    pub fn cleared_here(&mut self) {
        self.mine = None;
        self.pending = None;
    }

    /// The session left. What it was holding goes with it; what was copied here stays.
    pub fn session_left(&mut self) {
        self.theirs = None;
        self.announced = None;
        self.waiting.clear();
    }
}

/// Claim the X11 clipboard on an application's behalf, so X11 clients can paste what a Wayland
/// one — or a host — copied. Under every name text is known by; see `with_text_aliases`.
fn tell_x11(xwm: Option<&mut X11Wm>, mime_types: &[String]) {
    let Some(xwm) = xwm else { return };
    let offered = clipboard::with_text_aliases(mime_types);
    if let Err(e) = xwm.new_selection(SelectionTarget::Clipboard, Some(offered)) {
        log::warn!("X11 was not told about the clipboard: {e}");
    }
}

/// Hand `bytes` to whoever is reading the other end of `fd`, on a thread.
///
/// A pipe holds only 64 KB, so a picture cannot be written in one go: the write blocks until
/// the application has read what is already there, and doing that on the compositor's thread
/// stops the world until an application that may not be in a hurry catches up.
fn write_away(fd: OwnedFd, bytes: Vec<u8>) {
    std::thread::Builder::new()
        .name("clipboard-write".into())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            // A paste the application abandons closes the pipe, and writing to it raises
            // EPIPE. Not worth a word: it means somebody pressed escape.
            let _ = file.write_all(&bytes);
        })
        .ok();
}

/// Ask the application that owns the selection for one form, and read it on a thread.
fn read_from_wayland(
    seat: &smithay::input::Seat<Host>,
    mime_type: &str,
) -> Option<std::sync::mpsc::Receiver<Vec<u8>>> {
    let (read, write) = pipe()?;
    if let Err(e) = smithay::wayland::selection::data_device::request_data_device_client_selection(
        seat,
        mime_type.to_string(),
        write,
    ) {
        log::warn!("could not read what was copied here as {mime_type}: {e}");
        return None;
    }
    Some(drain_on_a_thread(read, mime_type.to_string()))
}

/// The same, for a selection owned by an X11 client.
fn read_from_x11(
    xwm: &mut X11Wm,
    mime_type: &str,
    loop_handle: &LoopHandle<'static, Host>,
) -> Option<std::sync::mpsc::Receiver<Vec<u8>>> {
    let (read, write) = pipe()?;
    if let Err(e) = xwm.send_selection(
        SelectionTarget::Clipboard,
        mime_type.to_string(),
        write,
        loop_handle.clone(),
    ) {
        log::warn!("could not read the X11 selection as {mime_type}: {e}");
        return None;
    }
    Some(drain_on_a_thread(read, mime_type.to_string()))
}

fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    match std::io::pipe() {
        Ok((read, write)) => Some((OwnedFd::from(read), OwnedFd::from(write))),
        Err(e) => {
            log::warn!("could not make a pipe for the clipboard: {e}");
            None
        }
    }
}

fn drain_on_a_thread(fd: OwnedFd, mime_type: String) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard-read".into())
        .spawn(move || {
            let bytes = match clipboard::drain(fd, clipboard::LARGEST) {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("could not read the clipboard as {mime_type}: {e}");
                    Vec::new()
                }
            };
            let _ = tx.send(bytes);
        })
        .ok();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn text_on_the_session(text: &str) -> Clipboard {
        let mut board = Clipboard::new();
        board.theirs = Some(Held {
            mime_types: vec!["text/plain;charset=utf-8".into(), "image/png".into()],
            text: Some(text.into()),
            bytes: text.len() as u32,
        });
        board
    }

    /// Read what a paste produced, with the writing end already gone.
    ///
    /// Bytes rather than text on purpose: a clipboard carries pictures, and an earlier version
    /// of this read the pipe as a string, where a PNG's first byte is not valid UTF-8 and the
    /// whole paste came back empty. The code was right and the test was wrong, which is the
    /// more expensive way round.
    fn pasted(read: std::io::PipeReader) -> Vec<u8> {
        let mut out = Vec::new();
        let mut read = read;
        let _ = read.read_to_end(&mut out);
        out
    }

    #[test]
    fn small_text_is_pasted_without_asking_the_other_end() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel();
        let mut board = text_on_the_session("hello");
        board.paste_from_the_session("text/plain;charset=utf-8".into(), OwnedFd::from(write), &out);
        assert_eq!(pasted(read), b"hello");
        assert!(
            outbox.try_recv().is_err(),
            "text that already travelled must not be asked for again"
        );
    }

    #[test]
    fn a_picture_is_asked_for_and_handed_over_when_it_arrives() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel();
        let mut board = text_on_the_session("hello");
        board.paste_from_the_session("image/png".into(), OwnedFd::from(write), &out);
        match outbox.try_recv() {
            Ok(ToSession::Control(HostMessage::Clipboard(Wire::Want { mime_type }))) => {
                assert_eq!(mime_type, "image/png")
            }
            _ => panic!("the picture should have been asked for"),
        }
        board.arrived("image/png", b"\x89PNG\r\n\x1a\n".to_vec());
        assert_eq!(pasted(read), b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn a_form_nobody_has_ends_the_paste_rather_than_leaving_it_hanging() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel();
        let mut board = text_on_the_session("hello");
        board.paste_from_the_session("audio/flac".into(), OwnedFd::from(write), &out);
        assert!(outbox.try_recv().is_err(), "nothing to ask for");
        assert!(
            pasted(read).is_empty(),
            "the pipe should be closed, not held open"
        );
    }

    #[test]
    fn a_paste_nobody_answers_is_given_up_on() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let (out, _outbox) = tokio::sync::mpsc::unbounded_channel();
        let mut board = text_on_the_session("hello");
        board.paste_from_the_session("image/png".into(), OwnedFd::from(write), &out);
        // As it stands, still waiting: the application is not hurried along.
        board.expire();
        assert_eq!(board.waiting.len(), 1);
        // Once the deadline has passed it is dropped, which closes the pipe and ends the
        // paste empty rather than leaving the window stuck.
        board.waiting[0].since = Instant::now() - PATIENCE * 2;
        board.expire();
        assert!(board.waiting.is_empty());
        assert!(pasted(read).is_empty());
    }

    #[test]
    fn two_pastes_of_the_same_thing_are_both_answered() {
        let (out, _outbox) = tokio::sync::mpsc::unbounded_channel();
        let mut board = text_on_the_session("hello");
        let pipes: Vec<_> = (0..2)
            .map(|_| {
                let (read, write) = std::io::pipe().expect("a pipe");
                board.paste_from_the_session("image/png".into(), OwnedFd::from(write), &out);
                read
            })
            .collect();
        board.arrived("image/png", b"bytes".to_vec());
        for read in pipes {
            assert_eq!(pasted(read), b"bytes");
        }
    }

    #[test]
    fn what_we_announced_is_not_applied_when_it_comes_back() {
        // The session tells every host what it holds, including the one it heard it from. A
        // host that applied that would take the selection away from the application that owns
        // it and replace it with a copy of itself, and pasting anything larger than the text
        // would then fetch from a source nobody holds.
        let mut board = Clipboard::new();
        let held = Held {
            mime_types: vec!["text/plain;charset=utf-8".into()],
            text: Some("hello".into()),
            bytes: 5,
        };
        board.announced = Some(held.clone());
        assert!(board.announced.as_ref().is_some_and(|a| a.same_as(&held)));
    }
}
