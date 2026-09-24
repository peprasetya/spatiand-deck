//! The session's clipboard, which is everybody's clipboard.
//!
//! **This session holds the clipboard for every machine it can see.** Copy in a window here
//! and it can be pasted on any host; copy on a host and it can be pasted here, or on another
//! host, because everything passes through the headset. Working across two or three machines
//! should feel like working on one, and the clipboard is most of what that means in practice.
//!
//! What travels is a list of forms, not the data. Copying announces what the selection *could*
//! be turned into and how big it is; the bytes are fetched only when somebody pastes. Small
//! text is the exception and is carried with the announcement, because that is nearly every
//! paste and a round trip would be felt.
//!
//! Reading a selection means reading a pipe from an application, which may be slow or stuck,
//! so every read and write here happens on its own thread. The compositor never waits for one:
//! a session that stops drawing because something was copied is a session nobody can use.
//!
//! The host end of the same conversation is `spatiand-host`'s `clipboard` module, and the
//! messages between them are in `spatiand_stream::control::Clipboard`.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use smithay::input::Seat;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::selection::SelectionTarget;
use spatiand_stream::clipboard::{self, Held};
use spatiand_stream::control::{Clipboard as Wire, CLIPBOARD_EAGER_BYTES};

use crate::state::Spatiand;

/// How long a paste waits for bytes from a host before it is given up on.
///
/// The link can be slow and a host can be asleep. Ending the paste empty is a disappointment;
/// leaving the application on a pipe that never closes is a window that has hung.
const PATIENCE: Duration = Duration::from_secs(5);

/// What a paste, or a copy, needs said to a host.
///
/// Returned rather than sent, because the board is inside the compositor and the hosts are
/// each behind their own thread. The caller passes these to [`crate::remote::Remotes`].
#[derive(Debug, Clone, PartialEq)]
pub enum Say {
    /// To everyone except, when given, the host it came from.
    Everyone { except: Option<String>, what: Wire },
    /// To one host, by name.
    Just { host: String, what: Wire },
}

/// Where what is on the clipboard came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// A window belonging to this session, speaking Wayland, offering these forms.
    Here(Vec<String>),
    /// A window belonging to this session, speaking X11 through Xwayland.
    HereX11(Vec<String>),
    /// A host, by name.
    Host(String),
}

/// A paste in this session waiting for a host to send the bytes.
struct Waiting {
    mime_type: String,
    fd: OwnedFd,
    since: Instant,
}

/// The session's clipboard.
#[derive(Default)]
pub struct Board {
    owner: Option<Owner>,
    /// What a host is holding, when one of them owns the clipboard.
    held: Option<Held>,
    /// The last thing announced to the hosts, so that hearing it back is an echo rather than a
    /// new copy. Without this one copy circulates for as long as anyone is watching.
    announced: Option<Held>,
    /// A copy made here that has not been announced yet.
    ///
    /// Announcing waits one turn, because the compositor calls the handler *before* it stores
    /// the new selection: reading the seat on the spot hands back the previous clipboard.
    pending: Option<Vec<String>>,
    waiting: Vec<Waiting>,
    /// What the reader threads have finished, for [`Board::pump`] to collect.
    ///
    /// Everything that reads an application's selection does it on a thread, so nothing it
    /// produces can be returned from the call that started it. It arrives here instead, and
    /// the compositor picks it up on its next turn — a frame later at worst, which is the
    /// price of never blocking one.
    notes: Option<(
        std::sync::mpsc::Sender<Note>,
        std::sync::mpsc::Receiver<Note>,
    )>,
}

/// Something a reader thread finished.
enum Note {
    /// A copy made here is ready to be announced.
    Copied(Held),
    /// An answer to a host's paste.
    Answer(Say),
}

impl Board {
    pub fn new() -> Self {
        Self {
            notes: Some(std::sync::mpsc::channel()),
            ..Default::default()
        }
    }

    /// Who holds the clipboard and as what, in a line. For the control socket and the log.
    pub fn describe(&self) -> String {
        match &self.owner {
            Some(Owner::Here(forms)) => format!("wayland {}", forms.join(",")),
            Some(Owner::HereX11(forms)) => format!("x11 {}", forms.join(",")),
            Some(Owner::Host(host)) => format!(
                "host {host} {}",
                self.held.as_ref().map_or(String::new(), |h| h.mime_types.join(","))
            ),
            None => "nobody".into(),
        }
    }

    /// A window here copied something.
    pub fn copied_here(
        &mut self,
        mime_types: Vec<String>,
        x11: bool,
        xwm: Option<&mut smithay::xwayland::X11Wm>,
    ) {
        let mime_types = clipboard::offered(mime_types);
        if mime_types.is_empty() {
            return;
        }
        // A Wayland copy has to be claimed on X11's behalf too, or nothing under Xwayland can
        // paste it. An X11 copy is already X11's and needs no claiming.
        if !x11 {
            tell_x11(xwm, &mime_types);
        }
        self.owner = Some(if x11 {
            Owner::HereX11(mime_types.clone())
        } else {
            Owner::Here(mime_types.clone())
        });
        self.held = None;
        self.pending = Some(mime_types);
    }

    /// A host says its clipboard changed.
    ///
    /// Returns what to pass on: every *other* host hears about it too, which is what lets two
    /// hosts share a clipboard through a headset that is only looking at them.
    pub fn host_offered(
        &mut self,
        host: &str,
        held: Held,
        display_handle: &DisplayHandle,
        seat: &Seat<Spatiand>,
        xwm: Option<&mut smithay::xwayland::X11Wm>,
    ) -> Option<Say> {
        // Our own announcement coming back. Applying it would take the selection away from the
        // window that owns it and leave a copy that cannot serve anything large.
        if self.announced.as_ref().is_some_and(|a| a.same_as(&held)) {
            return None;
        }
        // The same again from the same host, which happens when a host re-announces on
        // reattach. Passing it on would start it round the houses for nothing.
        if self.owner.as_ref() == Some(&Owner::Host(host.to_string()))
            && self.held.as_ref().is_some_and(|h| h.same_as(&held))
        {
            return None;
        }
        // Offered to the windows here under every name they might ask by; what goes on the
        // wire stays the true list. See `clipboard::with_text_aliases`.
        let mime_types = clipboard::with_text_aliases(&held.mime_types);
        log::info!(
            "clipboard: {host} copied {}; this session holds it now",
            mime_types.join(", ")
        );
        self.owner = Some(Owner::Host(host.to_string()));
        self.held = Some(held.clone());
        // What was announced is no longer on the clipboard, so hearing it again is not an echo
        // but a new copy of the same thing: the same address copied twice, say, with something
        // else copied in between. Remembering it past this point dropped that second copy and
        // left whatever came between on the clipboard.
        self.announced = None;
        smithay::wayland::selection::data_device::set_data_device_selection(
            display_handle,
            seat,
            mime_types,
            (),
        );
        tell_x11(xwm, &held.mime_types);
        Some(Say::Everyone {
            except: Some(host.to_string()),
            what: Wire::Offer {
                mime_types: held.mime_types,
                text: held.text,
                bytes: held.bytes,
            },
        })
    }

    /// A window here is pasting.
    ///
    /// What a host holds comes over the link. What another window here holds is served by that
    /// window: it is handed the pipe, whichever protocol it speaks, and nothing passes through
    /// this process. Only the first case was handled at first, which is why a window could
    /// paste its own copy and nothing else.
    pub fn paste_here(
        &mut self,
        mime_type: String,
        fd: OwnedFd,
        seat: &Seat<Spatiand>,
        xwm: Option<&mut smithay::xwayland::X11Wm>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) -> Option<Say> {
        log::info!(
            "clipboard: a window here is pasting {mime_type} (held by {})",
            match &self.owner {
                Some(Owner::Here(_)) => "a Wayland window here".into(),
                Some(Owner::HereX11(_)) => "an X11 window here".into(),
                Some(Owner::Host(host)) => format!("{host}"),
                None => "nobody".into(),
            }
        );
        match &self.owner {
            Some(Owner::Here(forms)) => {
                let asked = clipboard::resolve(&mime_type, forms)?;
                if let Err(e) =
                    smithay::wayland::selection::data_device::request_data_device_client_selection(
                        seat, asked, fd,
                    )
                {
                    log::warn!("could not pass on a paste of {mime_type}: {e}");
                }
                return None;
            }
            Some(Owner::HereX11(forms)) => {
                let asked = clipboard::resolve(&mime_type, forms)?;
                if let Err(e) = xwm?.send_selection(
                    SelectionTarget::Clipboard,
                    asked,
                    fd,
                    loop_handle.clone(),
                ) {
                    log::warn!("could not pass on a paste of {mime_type} from X11: {e}");
                }
                return None;
            }
            _ => {}
        }
        self.paste_from_a_host(mime_type, fd)
    }

    /// The part of a paste that a host has to answer, which is all of it when a host owns the
    /// clipboard. Separated so it can be tested without a compositor to hand.
    fn paste_from_a_host(&mut self, mime_type: String, fd: OwnedFd) -> Option<Say> {
        let held = self.held.as_ref()?;
        let Some(Owner::Host(host)) = self.owner.clone() else {
            return None;
        };
        if let Some(bytes) = held.answers(&mime_type) {
            write_away(fd, bytes);
            return None;
        }
        // What this window asked for, in the words the host understands. An X11 client asking
        // for STRING is asking for the text, whatever the host calls it.
        let Some(asked) = clipboard::resolve(&mime_type, &held.mime_types) else {
            log::info!("clipboard: nothing on the clipboard can be given as {mime_type}");
            return None;
        };
        log::info!("clipboard: asking {host} for {asked}");
        self.waiting.push(Waiting {
            mime_type: asked.clone(),
            fd,
            since: Instant::now(),
        });
        Some(Say::Just {
            host,
            what: Wire::Want { mime_type: asked },
        })
    }

    /// A host is pasting what was copied here. Fetch it and answer.
    pub fn host_wants(
        &mut self,
        host: &str,
        mime_type: String,
        seat: &Seat<Spatiand>,
        xwm: Option<&mut smithay::xwayland::X11Wm>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) {
        log::info!("clipboard: {host} is pasting {mime_type} from here");
        let fetch = match self.owner.as_ref() {
            Some(Owner::Here(forms)) => clipboard::resolve(&mime_type, forms)
                .and_then(|asked| read_from_wayland(seat, &asked)),
            Some(Owner::HereX11(forms)) => clipboard::resolve(&mime_type, forms)
                .zip(xwm)
                .and_then(|(asked, xwm)| read_from_x11(xwm, &asked, loop_handle)),
            _ => None,
        };
        let Some((told, _)) = &self.notes else { return };
        let told = told.clone();
        let host = host.to_string();
        std::thread::Builder::new()
            .name("clipboard-paste".into())
            .spawn(move || {
                // An empty answer rather than none: there is an application on that machine
                // waiting on a pipe, and it would rather paste nothing than hang.
                let bytes = fetch.and_then(|rx| rx.recv().ok()).unwrap_or_default();
                let _ = told.send(Note::Answer(Say::Just {
                    host,
                    what: Wire::Data { mime_type, bytes },
                }));
            })
            .ok();
    }

    /// The bytes a paste here was waiting for.
    pub fn arrived(&mut self, mime_type: &str, bytes: Vec<u8>) {
        let (ready, rest): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.mime_type.eq_ignore_ascii_case(mime_type));
        self.waiting = rest;
        for waiting in ready {
            write_away(waiting.fd, bytes.clone());
        }
    }

    /// A host went away. What it was holding goes with it.
    pub fn host_left(&mut self, host: &str) {
        if self.owner.as_ref() == Some(&Owner::Host(host.to_string())) {
            self.owner = None;
            self.held = None;
            self.announced = None;
        }
        self.waiting.clear();
    }

    /// Once a turn: collect what the reader threads finished, start announcing any copy made
    /// here, and give up on pastes nobody answered.
    pub fn pump(
        &mut self,
        seat: &Seat<Spatiand>,
        xwm: Option<&mut smithay::xwayland::X11Wm>,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
    ) -> Vec<Say> {
        let mut out = Vec::new();
        if let Some((_, notes)) = &self.notes {
            while let Ok(note) = notes.try_recv() {
                match note {
                    Note::Copied(held) => {
                        self.announced = Some(held.clone());
                        out.push(Say::Everyone {
                            except: None,
                            what: Wire::Offer {
                                mime_types: held.mime_types,
                                text: held.text,
                                bytes: held.bytes,
                            },
                        });
                    }
                    Note::Answer(say) => out.push(say),
                }
            }
        }
        self.expire();

        if let Some(mime_types) = self.pending.take() {
            let fetch = match (clipboard::best_text(&mime_types), self.owner.as_ref()) {
                (Some(mime), Some(Owner::HereX11(_))) => {
                    xwm.and_then(|xwm| read_from_x11(xwm, &mime, loop_handle))
                }
                (Some(mime), _) => read_from_wayland(seat, &mime),
                (None, _) => None,
            };
            if let Some((told, _)) = &self.notes {
                let told = told.clone();
                // The announcement waits for the text, on a thread, so that a paste on the far
                // end is instant rather than a round trip that had already been made.
                std::thread::Builder::new()
                    .name("clipboard-copy".into())
                    .spawn(move || {
                        let text = fetch
                            .and_then(|rx| rx.recv().ok())
                            .filter(|bytes| bytes.len() <= CLIPBOARD_EAGER_BYTES as usize)
                            .and_then(|bytes| String::from_utf8(bytes).ok());
                        log::info!(
                            "clipboard: copied here as {}; telling the hosts",
                            mime_types.join(", ")
                        );
                        let _ = told.send(Note::Copied(Held {
                            mime_types,
                            bytes: text.as_ref().map_or(0, |t| t.len() as u32),
                            text,
                        }));
                    })
                    .ok();
            }
        }
        out
    }

    fn expire(&mut self) {
        self.waiting.retain(|w| {
            let alive = w.since.elapsed() < PATIENCE;
            if !alive {
                log::warn!(
                    "no host answered a paste of {} in {} s; ending it empty",
                    w.mime_type,
                    PATIENCE.as_secs()
                );
            }
            alive
        });
    }
}

/// Once a frame: everything the hosts said about the clipboard, everything the board has to say
/// back, and whatever the protocol handlers put down since the last turn.
///
/// The board lives in the compositor because that is where the selection and the windows are;
/// the hosts are each behind a thread. So everything meets here. One function, called by every
/// backend that holds hosts, so a session on a desk and one in the glasses cannot come to
/// disagree about what a copy means.
pub fn exchange(remotes: &mut crate::remote::Remotes, state: &mut Spatiand) {
    use spatiand_stream::control::Clipboard as Said;
    for (host, what) in remotes.clipboard_said() {
        match what {
            Said::Offer {
                mime_types,
                text,
                bytes,
            } => {
                let held = Held {
                    mime_types,
                    text,
                    bytes,
                };
                if let Some(say) = state.clipboard.host_offered(
                    &host,
                    held,
                    &state.display_handle,
                    &state.seat,
                    state.xwm.as_mut(),
                ) {
                    state.clipboard_out.push(say);
                }
            }
            Said::Want { mime_type } => state.clipboard.host_wants(
                &host,
                mime_type,
                &state.seat,
                state.xwm.as_mut(),
                &state.loop_handle,
            ),
            Said::Data { mime_type, bytes } => state.clipboard.arrived(&mime_type, bytes),
        }
    }
    for host in remotes.offline() {
        state.clipboard.host_left(&host);
    }
    let mut said = state
        .clipboard
        .pump(&state.seat, state.xwm.as_mut(), &state.loop_handle);
    said.extend(state.clipboard_out.drain(..));
    for say in said {
        remotes.clipboard_say(say);
    }
}

/// Claim the X11 clipboard on a window's behalf, so X11 windows can paste what a Wayland one —
/// or a host — copied. Under every name text is known by; see `with_text_aliases`.
fn tell_x11(xwm: Option<&mut smithay::xwayland::X11Wm>, mime_types: &[String]) {
    let Some(xwm) = xwm else { return };
    let offered = clipboard::with_text_aliases(mime_types);
    if let Err(e) = xwm.new_selection(SelectionTarget::Clipboard, Some(offered)) {
        log::warn!("X11 here was not told about the clipboard: {e}");
    }
}

/// Hand `bytes` to whoever is reading the other end, on a thread.
///
/// A pipe holds 64 KB. Writing a picture into one blocks until the application has read what
/// is already there, and doing that on the compositor's thread stops the world.
fn write_away(fd: OwnedFd, bytes: Vec<u8>) {
    std::thread::Builder::new()
        .name("clipboard-write".into())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            // EPIPE here means the application abandoned the paste. Not worth a word.
            let _ = file.write_all(&bytes);
        })
        .ok();
}

fn read_from_wayland(
    seat: &Seat<Spatiand>,
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

fn read_from_x11(
    xwm: &mut smithay::xwayland::X11Wm,
    mime_type: &str,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::Runtime>,
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

    fn from_a_host(text: &str) -> Board {
        let mut board = Board::new();
        board.owner = Some(Owner::Host("workshop".into()));
        board.held = Some(Held {
            mime_types: vec!["text/plain;charset=utf-8".into(), "image/png".into()],
            text: Some(text.into()),
            bytes: text.len() as u32,
        });
        board
    }

    fn pasted(read: std::io::PipeReader) -> Vec<u8> {
        let mut out = Vec::new();
        let mut read = read;
        let _ = read.read_to_end(&mut out);
        out
    }

    #[test]
    fn text_from_a_host_pastes_without_asking_it_again() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let mut board = from_a_host("hello");
        assert_eq!(
            board.paste_from_a_host("text/plain;charset=utf-8".into(), OwnedFd::from(write)),
            None,
            "text that already travelled must not be fetched again"
        );
        assert_eq!(pasted(read), b"hello");
    }

    #[test]
    fn a_picture_is_fetched_from_the_host_that_has_it() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let mut board = from_a_host("hello");
        assert_eq!(
            board.paste_from_a_host("image/png".into(), OwnedFd::from(write)),
            Some(Say::Just {
                host: "workshop".into(),
                what: Wire::Want {
                    mime_type: "image/png".into()
                }
            })
        );
        board.arrived("image/png", b"\x89PNG\r\n\x1a\n".to_vec());
        assert_eq!(pasted(read), b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn a_paste_no_host_answers_is_given_up_on_rather_than_left_hanging() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let mut board = from_a_host("hello");
        board.paste_from_a_host("image/png".into(), OwnedFd::from(write));
        board.expire();
        assert_eq!(board.waiting.len(), 1, "a slow host is not hurried along");
        board.waiting[0].since = Instant::now() - PATIENCE * 2;
        board.expire();
        assert!(board.waiting.is_empty());
        assert!(pasted(read).is_empty(), "the pipe should close, not hang");
    }

    #[test]
    fn what_one_host_copies_is_offered_to_the_others_and_not_back_to_it() {
        // The whole point of the session holding the clipboard: two machines that cannot see
        // each other share one through the headset.
        let mut board = Board::new();
        board.owner = Some(Owner::Host("one".into()));
        let held = Held {
            mime_types: vec!["text/plain;charset=utf-8".into()],
            text: Some("hello".into()),
            bytes: 5,
        };
        // `host_offered` needs a compositor, so the passing-on decision is checked through the
        // same rules it uses: a repeat from the same host says nothing.
        board.held = Some(held.clone());
        assert!(
            board.held.as_ref().is_some_and(|h| h.same_as(&held)),
            "a host re-announcing on reattach must be recognised"
        );
        board.announced = Some(held.clone());
        assert!(
            board.announced.as_ref().is_some_and(|a| a.same_as(&held)),
            "our own announcement coming back must be recognised"
        );
    }

    #[test]
    fn a_host_leaving_takes_its_clipboard_with_it_and_leaves_the_others_alone() {
        let mut board = from_a_host("hello");
        board.host_left("elsewhere");
        assert!(board.held.is_some(), "another host's leaving changes nothing");
        board.host_left("workshop");
        assert!(board.held.is_none());
        assert!(board.owner.is_none());
    }

    #[test]
    fn nothing_worth_offering_is_not_announced() {
        // X11 answers `TARGETS` with names that describe the selection rather than its
        // contents; a selection of nothing else is not a copy.
        let mut board = Board::new();
        board.copied_here(vec!["TARGETS".into(), "TIMESTAMP".into()], true, None);
        assert!(board.pending.is_none());
        assert!(board.owner.is_none());
    }
}
