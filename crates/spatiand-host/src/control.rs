//! Talking to a host that is already running.
//!
//! The host runs as a service, and a service cannot be restarted to change what it is doing:
//! restarting it closes every application it is holding open, which is the one thing it
//! exists not to do. So the commands that act on a running host — pairing, for now — reach it
//! over a Unix socket instead of starting a second copy.
//!
//! The socket lives in `$XDG_RUNTIME_DIR`, which is private to the user and gone at logout, so
//! reaching it already proves you are the account the host runs as. That is the same proof
//! `ssh` gave, which is why nothing here asks for a password.
//!
//! The protocol is lines of text, because the other end is a person at a terminal as often as
//! it is a program:
//!
//! ```text
//! → pair
//! ← open 120                        the gate is open for this many seconds
//! ← code 3f9a1c20 482 913           a session arrived; does the headset show this?
//! → yes                             (or: no)
//! ← paired 3f9a1c20                 written down; the session is being served
//! ← refused | closed                it said no, or nobody came in time
//!
//! → launch chrome                   start a catalogue entry, as the headset would
//! ← launched chrome | failed <why>
//!
//! → list                            what is open
//! ← window 3 chrome <title>         one line each, then
//! ← end
//! → close 3                         ask window 3 to close, as its close button would
//! → kill chrome                     kill the application and everything it started
//! ← done | failed <why>
//! → restart                         exit, for the service manager to start a fresh host
//! ← restarting
//!
//! → key 3 ctrl+a ctrl+c             press these in window 3, as the session's keys would
//! → type 3 clip-4821                type this in window 3, US layout
//! ← done | failed <why>
//! → input 3 move 640 400             one input to window 3 exactly as a session sends it:
//! → input 3 button 272 1             move the pointer, press (1) or release (0) a button or
//! → input 3 scroll 0 15              turn the wheel (surface units, as a session sends)
//! → input 3 keycode 56 1             a key by its evdev code -- held across commands, so a
//! ← done | failed <why>              drag with Alt down can be done a step at a time
//! → clipboard                       paste here, as an application would, and say what came
//! ← held <who holds it, as what>
//! ← text <the bytes, escaped>       see spatiand_stream::keys::escape
//! ```
//!
//! The last three exist for tests that copy in one application and paste in another across
//! machines: nobody can do that by hand forty times, and it only means something done through
//! the compositor's own seat and its own paste path.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Something a command-line client asked for.
#[derive(Debug)]
pub enum Asked {
    /// Open the pairing window, and tell this client what happens.
    Pair { client: u64 },
    /// The answer to a `code` line.
    Answer { client: u64, yes: bool },
    /// The client went away. A pairing it started is cancelled: nobody is left to confirm it.
    Gone { client: u64 },
    /// Start this catalogue entry, exactly as if the headset had asked. What the settings
    /// app's Open button sends.
    Launch { client: u64, app: String },
    /// What is open, one line per window.
    List { client: u64 },
    /// Ask a window to close, the way its own close button would.
    Close { client: u64, window: u32 },
    /// Kill an application and everything it started.
    Kill { client: u64, app: String },
    /// Stop, so the service manager starts a fresh host.
    Restart { client: u64 },
    /// Press these in this window, through the seat.
    Keys { client: u64, window: u32, strokes: Vec<spatiand_stream::keys::Stroke> },
    /// One input to a window, as a session would send it. See `input` above.
    Input { client: u64, window: u32, input: spatiand_stream::Input },
    /// Paste the clipboard into a pipe, as an application would, and say what came out.
    Clipboard { client: u64 },
}

/// Where the socket is.
pub fn path() -> PathBuf {
    spatiand_host_catalog::control_socket_path()
}

/// The listening end, held by the host's main loop.
pub struct Control {
    inbox: Receiver<Asked>,
    clients: Arc<Mutex<HashMap<u64, UnixStream>>>,
}

impl Control {
    /// Start listening. A stale socket from a host that died is removed first; a *live* one
    /// means another host is already running here, which is an error worth stopping for.
    pub fn start() -> Result<Control, String> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(format!(
                    "another spatiand-host is already running (its socket {} answers)",
                    path.display()
                ));
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("could not listen on {}: {e}", path.display()))?;

        let (tx, inbox) = channel();
        let clients: Arc<Mutex<HashMap<u64, UnixStream>>> = Arc::default();
        {
            let clients = clients.clone();
            std::thread::Builder::new()
                .name("spatiand-host-control".into())
                .spawn(move || accept(listener, tx, clients))
                .map_err(|e| format!("could not start the control thread: {e}"))?;
        }
        Ok(Control { inbox, clients })
    }

    pub fn poll(&self) -> Vec<Asked> {
        self.inbox.try_iter().collect()
    }

    /// A second handle on one client's stream, for an answer that arrives on another thread.
    pub fn writer(&self, client: u64) -> Option<UnixStream> {
        self.clients.lock().ok()?.get(&client)?.try_clone().ok()
    }

    /// Say one line to one client. A client that has gone is not an error.
    pub fn tell(&self, client: u64, line: &str) {
        if let Ok(mut clients) = self.clients.lock() {
            if let Some(stream) = clients.get_mut(&client) {
                if writeln!(stream, "{line}").is_err() {
                    clients.remove(&client);
                }
            }
        }
    }
}

fn accept(listener: UnixListener, tx: Sender<Asked>, clients: Arc<Mutex<HashMap<u64, UnixStream>>>) {
    let mut next = 1u64;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let client = next;
        next += 1;
        if let Ok(writer) = stream.try_clone() {
            if let Ok(mut clients) = clients.lock() {
                clients.insert(client, writer);
            }
        }
        let tx = tx.clone();
        let clients = clients.clone();
        let _ = std::thread::Builder::new()
            .name(format!("spatiand-host-control-{client}"))
            .spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
                    let rest = rest.trim().to_string();
                    let asked = match word {
                        "pair" => Asked::Pair { client },
                        "yes" | "y" => Asked::Answer { client, yes: true },
                        "no" | "n" => Asked::Answer { client, yes: false },
                        "launch" if !rest.is_empty() => Asked::Launch { client, app: rest },
                        "list" => Asked::List { client },
                        "close" => match rest.parse() {
                            Ok(window) => Asked::Close { client, window },
                            Err(_) => continue,
                        },
                        "kill" if !rest.is_empty() => Asked::Kill { client, app: rest },
                        "restart" => Asked::Restart { client },
                        "clipboard" => Asked::Clipboard { client },
                        "input" => match parse_input(&rest) {
                            Ok((window, input)) => Asked::Input { client, window, input },
                            Err(why) => {
                                tell_now(&clients, client, &format!("failed {why}"));
                                continue;
                            }
                        },
                        "key" | "type" => {
                            let (window, what) = rest.split_once(' ').unwrap_or((&rest, ""));
                            let strokes = if word == "key" {
                                what.split_whitespace()
                                    .map(spatiand_stream::keys::chord)
                                    .collect::<Result<Vec<_>, _>>()
                            } else {
                                spatiand_stream::keys::typing(what)
                            };
                            match (window.parse(), strokes) {
                                (Ok(window), Ok(strokes)) => Asked::Keys { client, window, strokes },
                                (Err(_), _) => {
                                    tell_now(&clients, client, &format!("failed {window:?} is not a window number"));
                                    continue;
                                }
                                (_, Err(why)) => {
                                    tell_now(&clients, client, &format!("failed {why}"));
                                    continue;
                                }
                            }
                        }
                        _ => continue,
                    };
                    if tx.send(asked).is_err() {
                        break;
                    }
                }
                let _ = tx.send(Asked::Gone { client });
                if let Ok(mut clients) = clients.lock() {
                    clients.remove(&client);
                }
            });
    }
}

/// `3 move 640 400`, `3 button 272 1`, `3 keycode 56 0`.
fn parse_input(rest: &str) -> Result<(u32, spatiand_stream::Input), String> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let number = |i: usize| -> Result<f64, String> {
        words
            .get(i)
            .ok_or_else(|| format!("missing argument {i}"))?
            .parse::<f64>()
            .map_err(|_| format!("{:?} is not a number", words[i]))
    };
    let window = number(0)? as u32;
    let input = match words.get(1).copied() {
        Some("move") => spatiand_stream::Input::Motion { x: number(2)?, y: number(3)? },
        Some("button") => spatiand_stream::Input::Button {
            button: number(2)? as u32,
            pressed: number(3)? != 0.0,
        },
        Some("scroll") => spatiand_stream::Input::Scroll { horizontal: number(2)?, vertical: number(3)? },
        Some("keycode") => spatiand_stream::Input::Key {
            code: number(2)? as u32,
            pressed: number(3)? != 0.0,
        },
        other => return Err(format!("unknown input {other:?}")),
    };
    Ok((window, input))
}

fn tell_now(clients: &Mutex<HashMap<u64, UnixStream>>, client: u64, line: &str) {
    if let Ok(mut clients) = clients.lock() {
        if let Some(stream) = clients.get_mut(&client) {
            let _ = writeln!(stream, "{line}");
        }
    }
}

/// `spatiand-host --pair` against a host that is already running.
///
/// Returns `None` when there is no running host to talk to, so the caller can fall back to
/// starting one itself. Otherwise runs the whole conversation at the terminal and returns
/// whether a session was paired.
pub fn pair_with_running_host() -> Option<bool> {
    let stream = UnixStream::connect(path()).ok()?;
    let mut writer = stream.try_clone().ok()?;
    writeln!(writer, "pair").ok()?;

    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("open") => {
                let seconds = words.next().unwrap_or("120");
                println!("Pairing is open for {seconds} seconds.");
                println!(
                    "In Spatiand: Settings → Remote computers → Add a computer, and type this \
                     machine's address."
                );
            }
            Some("code") => {
                let who = words.next().unwrap_or("?");
                let code: Vec<&str> = words.collect();
                let code = code.join(" ");
                println!();
                println!("A headset ({who}) is asking to pair.");
                print!("Does it show the code  {code}  ? [y/N] ");
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                let yes = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
                let _ = writeln!(writer, "{}", if yes { "yes" } else { "no" });
            }
            Some("paired") => {
                println!("Paired. It can connect from now on without asking.");
                return Some(true);
            }
            Some("refused") => {
                println!("Refused. Nothing was written down.");
                return Some(false);
            }
            Some("closed") => {
                println!("Nobody came. Pairing is closed again.");
                return Some(false);
            }
            Some("busy") => {
                println!("Another pairing is already in progress on this host.");
                return Some(false);
            }
            _ => {}
        }
    }
    println!("The host went away.");
    Some(false)
}
