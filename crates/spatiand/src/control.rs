//! Asking a running session to do what a wearer would.
//!
//! Copy in one window and paste in another, across two machines, and check what arrived:
//! that is a test nobody can run by hand forty times, and one that has to be run through the
//! real compositor to mean anything. So a session listens on a Unix socket for the few things
//! such a test needs — which windows are open, focus this one, press these keys, what is on
//! the clipboard — and does each exactly as a wearer's own input would.
//!
//! The socket is `$XDG_RUNTIME_DIR/spatiand/control`, private to the user who runs the
//! session and gone at logout; reaching it already proves you are that user, the same proof
//! `ssh` gave. Lines of text, like `spatiand-host`'s own control socket:
//!
//! ```text
//! → list
//! ← window 3 org.kde.kwrite 1280x800 wayland,focused Untitled — KWrite
//! ← window 5 remote.host:47600.chrome 1100x700 wayland New Tab - Google Chrome
//! ← end
//! → focus 3                       ← done | failed <why>
//! → key 3 ctrl+a ctrl+c           focus 3 and press these, in order
//! ← done | failed <why>
//! → type 3 clip-4821              focus 3 and type this, US layout
//! ← done | failed <why>
//! → click 3 20 12 3               three left clicks at 20,12 in window 3's own pixels --
//! ← done | failed <why>           a terminal's line selected, with no key for select-all
//! → clipboard                     paste it here, as a window would
//! ← held <who owns it, and as what>
//! ← text <the bytes, escaped>     (see spatiand_stream::keys::escape)
//! ```
//!
//! Keys go through the seat, so a window on a host receives them over the link like any other
//! key, and the clipboard is read through the same path a pasting window takes. Nothing here
//! is a shortcut around the thing being tested.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spatiand_stream::keys;

use crate::state::Spatiand;

/// Something asked for over the socket.
#[derive(Debug)]
enum Asked {
    List,
    Focus(usize),
    Keys(usize, Vec<keys::Stroke>),
    Click { id: usize, x: f64, y: f64, count: u32 },
    Clipboard,
}

/// How long a clipboard read waits for whoever holds it to finish writing.
const READ_PATIENCE: Duration = Duration::from_secs(6);

fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("spatiand").join("control"))
}

pub struct Control {
    inbox: Receiver<(u64, Asked)>,
    clients: Arc<Mutex<HashMap<u64, UnixStream>>>,
}

impl Control {
    /// Start listening, or say why not and carry on without. A session without its test
    /// socket is still a session.
    pub fn start() -> Option<Control> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                log::warn!("another session answers on {}; this one has no control socket", path.display());
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("no control socket at {}: {e}", path.display());
                return None;
            }
        };
        log::info!("control socket at {}", path.display());
        let (tx, inbox) = channel();
        let clients: Arc<Mutex<HashMap<u64, UnixStream>>> = Arc::default();
        let held = clients.clone();
        std::thread::Builder::new()
            .name("spatiand-control".into())
            .spawn(move || {
                for (client, stream) in (1u64..).zip(listener.incoming()) {
                    let Ok(stream) = stream else { continue };
                    if let (Ok(writer), Ok(mut clients)) = (stream.try_clone(), held.lock()) {
                        clients.insert(client, writer);
                    }
                    let tx = tx.clone();
                    let held = held.clone();
                    let _ = std::thread::Builder::new()
                        .name(format!("spatiand-control-{client}"))
                        .spawn(move || {
                            for line in BufReader::new(stream).lines() {
                                let Ok(line) = line else { break };
                                match parse(line.trim()) {
                                    Ok(asked) => {
                                        if tx.send((client, asked)).is_err() {
                                            break;
                                        }
                                    }
                                    Err(why) => say(&held, client, &format!("failed {why}")),
                                }
                            }
                            if let Ok(mut clients) = held.lock() {
                                clients.remove(&client);
                            }
                        });
                }
            })
            .ok()?;
        Some(Control { inbox, clients })
    }

    /// Do whatever has been asked since the last turn. Called once a frame.
    pub fn serve(&self, state: &mut Spatiand) {
        for (client, asked) in self.inbox.try_iter() {
            let tell = |line: &str| say(&self.clients, client, line);
            match asked {
                Asked::List => {
                    for (id, window) in windows(state) {
                        let mut flags =
                            vec![if window.x11_surface().is_some() { "x11" } else { "wayland" }];
                        if state.layout.is_focused(&window) {
                            flags.push("focused");
                        }
                        if Spatiand::is_environment(&window) {
                            flags.push("room");
                        }
                        let app = state.app_id_of(&window).unwrap_or_else(|| "-".into());
                        let size = window.geometry().size;
                        tell(&format!(
                            "window {id} {} {}x{} {} {}",
                            app.replace(' ', "_"),
                            size.w,
                            size.h,
                            flags.join(","),
                            state.display_title(&window)
                        ));
                    }
                    tell("end");
                }
                Asked::Focus(id) => match find(state, id) {
                    Some(window) => {
                        state.focus_window(&window);
                        tell("done");
                    }
                    None => tell(&format!("failed no window {id}")),
                },
                Asked::Keys(id, strokes) => match find(state, id) {
                    Some(window) => {
                        state.focus_window(&window);
                        let time = monotonic_ms();
                        for stroke in &strokes {
                            for (code, pressed) in keys::transitions(stroke) {
                                press(state, code, pressed, time);
                            }
                        }
                        tell("done");
                    }
                    None => tell(&format!("failed no window {id}")),
                },
                Asked::Click { id, x, y, count } => match find(state, id) {
                    Some(window) => {
                        state.focus_window(&window);
                        click(state, &window, (x, y), count, monotonic_ms());
                        tell("done");
                    }
                    None => tell(&format!("failed no window {id}")),
                },
                Asked::Clipboard => self.read_clipboard(state, client),
            }
        }
    }

    /// Paste the clipboard into a pipe, exactly as a window asking for text would, and answer
    /// with what came out of it.
    fn read_clipboard(&self, state: &mut Spatiand, client: u64) {
        let tell = |line: &str| say(&self.clients, client, line);
        tell(&format!("held {}", state.clipboard.describe()));
        let (mut reader, writer) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(e) => return tell(&format!("failed no pipe: {e}")),
        };
        if let Some(say) = state.clipboard.paste_here(
            "text/plain;charset=utf-8".into(),
            writer.into(),
            &state.seat,
            state.xwm.as_mut(),
            &state.loop_handle,
        ) {
            state.clipboard_out.push(say);
        }
        let clients = self.clients.clone();
        let _ = std::thread::Builder::new()
            .name("spatiand-control-read".into())
            .spawn(move || {
                let (done_tx, done_rx) = channel();
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let result = reader.read_to_end(&mut bytes).map(|_| bytes);
                    let _ = done_tx.send(result);
                });
                let line = match done_rx.recv_timeout(READ_PATIENCE) {
                    Ok(Ok(bytes)) => format!("text {}", keys::escape(&bytes)),
                    Ok(Err(e)) => format!("failed reading: {e}"),
                    Err(_) => format!("failed nothing finished writing in {} s", READ_PATIENCE.as_secs()),
                };
                say(&clients, client, &line);
            });
    }
}

fn say(clients: &Mutex<HashMap<u64, UnixStream>>, client: u64, line: &str) {
    if let Ok(mut clients) = clients.lock() {
        if let Some(stream) = clients.get_mut(&client) {
            if writeln!(stream, "{line}").is_err() {
                clients.remove(&client);
            }
        }
    }
}

fn parse(line: &str) -> Result<Asked, String> {
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    let window = |text: &str| -> Result<(usize, String), String> {
        let (id, rest) = text.split_once(' ').unwrap_or((text, ""));
        let id = id.parse().map_err(|_| format!("{id:?} is not a window number"))?;
        Ok((id, rest.to_string()))
    };
    Ok(match word {
        "list" => Asked::List,
        "clipboard" => Asked::Clipboard,
        "focus" => Asked::Focus(window(rest)?.0),
        "key" => {
            let (id, chords) = window(rest)?;
            let strokes = chords
                .split_whitespace()
                .map(keys::chord)
                .collect::<Result<Vec<_>, _>>()?;
            Asked::Keys(id, strokes)
        }
        "click" => {
            let (id, rest) = window(rest)?;
            let numbers: Vec<&str> = rest.split_whitespace().collect();
            let number = |i: usize| -> Result<f64, String> {
                numbers
                    .get(i)
                    .ok_or("click wants a window, x and y")?
                    .parse()
                    .map_err(|_| format!("{:?} is not a number", numbers[i]))
            };
            Asked::Click {
                id,
                x: number(0)?,
                y: number(1)?,
                count: numbers.get(2).map_or(Ok(1), |c| c.parse().map_err(|_| format!("{c:?} is not a count")))?,
            }
        }
        // Everything after the window number, spaces included.
        "type" => {
            let (id, text) = window(rest)?;
            Asked::Keys(id, keys::typing(&text)?)
        }
        other => return Err(format!("{other:?} is not something this answers")),
    })
}

/// Every window with a number, rooms included: the test decides what to skip.
fn windows(state: &Spatiand) -> Vec<(usize, smithay::desktop::Window)> {
    state
        .space
        .elements()
        .filter_map(|w| state.layout.id_of(w).map(|id| (id, w.clone())))
        .collect()
}

fn find(state: &Spatiand, id: usize) -> Option<smithay::desktop::Window> {
    windows(state).into_iter().find(|(i, _)| *i == id).map(|(_, w)| w)
}

/// Milliseconds on CLOCK_MONOTONIC, the clock the seat's other events are stamped with.
fn monotonic_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid pointer to a timespec, and a clock every Linux has.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

/// Left clicks at a point in a window's own pixels, through the seat's pointer. For a window
/// on a host that is the same motion and buttons the wearer's pointer would send over the link.
fn click(
    state: &mut Spatiand,
    window: &smithay::desktop::Window,
    at: (f64, f64),
    count: u32,
    time_ms: u32,
) {
    use smithay::input::pointer::{ButtonEvent, MotionEvent};
    use smithay::utils::SERIAL_COUNTER;
    use smithay::wayland::seat::WaylandFocus;
    let (Some(pointer), Some(surface)) = (state.seat.get_pointer(), window.wl_surface()) else {
        return;
    };
    // The surface's origin at zero, so the location is in the window's own pixels.
    pointer.motion(
        state,
        Some((surface.into_owned(), smithay::utils::Point::from((0.0, 0.0)))),
        &MotionEvent {
            location: at.into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: time_ms,
        },
    );
    pointer.frame(state);
    const BTN_LEFT: u32 = 0x110;
    for _ in 0..count.clamp(1, 3) {
        for pressed in [true, false] {
            pointer.button(
                state,
                &ButtonEvent {
                    button: BTN_LEFT,
                    state: if pressed {
                        smithay::backend::input::ButtonState::Pressed
                    } else {
                        smithay::backend::input::ButtonState::Released
                    },
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_ms,
                },
            );
            pointer.frame(state);
        }
    }
}

/// One key transition through the seat, as a real keyboard's would go.
fn press(state: &mut Spatiand, evdev_code: u32, pressed: bool, time_ms: u32) {
    let Some(keyboard) = state.seat.get_keyboard() else { return };
    keyboard.input::<(), _>(
        state,
        smithay::input::keyboard::Keycode::new(evdev_code + 8),
        if pressed {
            smithay::backend::input::KeyState::Pressed
        } else {
            smithay::backend::input::KeyState::Released
        },
        smithay::utils::SERIAL_COUNTER.next_serial(),
        time_ms,
        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_a_window_and_chords() {
        match parse("key 3 ctrl+a ctrl+c").unwrap() {
            Asked::Keys(3, strokes) => assert_eq!(strokes.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typing_keeps_its_spaces() {
        match parse("type 7 two words").unwrap() {
            Asked::Keys(7, strokes) => assert_eq!(strokes.len(), 9),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_click_counts_once_unless_told() {
        match parse("click 2 40 400").unwrap() {
            Asked::Click { id: 2, count: 1, .. } => {}
            other => panic!("{other:?}"),
        }
        match parse("click 2 40.5 400 3").unwrap() {
            Asked::Click { id: 2, count: 3, x, .. } => assert_eq!(x, 40.5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nonsense_is_answered_rather_than_ignored() {
        assert!(parse("key x ctrl+c").is_err());
        assert!(parse("key 3 ctrl+nothing").is_err());
        assert!(parse("click 3 40").is_err());
        assert!(parse("dance").is_err());
    }
}
