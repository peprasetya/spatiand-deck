//! Remote applications, shown as ordinary windows in the room.
//!
//! A host somewhere else runs the application; this receives its pictures and puts them on a
//! surface. What makes that cheap to build is the last step: **the remote side is a Wayland
//! client of this compositor**, connected over a socket pair inside the same process.
//!
//! ```text
//!   host ── QUIC ──▶ net thread ──▶ decode ──▶ convert ──▶ wl_surface ──▶ the room
//!                         (this module, on its own thread)      ▲
//!                                                    the compositor sees only a client
//! ```
//!
//! Everything a window already has — a title bar, being moved and resized, focus, its own
//! sound, its own controller layout, the fade when nobody is looking at it — applies to a
//! remote window without a line of new code, because from the compositor's side it *is* a
//! window. The alternative, teaching the scene about a second kind of window, would have
//! touched the renderer, the pointer, the layout and the audio router at once.
//!
//! It costs one hop through a socket, and no pixels go through it: frames arrive as dmabufs
//! and are attached as dmabufs.
//!
//! ## What runs where
//!
//! One thread per host, holding the network, the decoders and the client connection. The
//! compositor thread is untouched: it discovers remote windows the same way it discovers any
//! other application's, by being asked for a surface.

mod client;
mod microphone;
mod pairing;
mod session;
pub mod sound;

use std::sync::{Arc, Mutex};

pub use session::Config;

use smithay::reexports::wayland_server::DisplayHandle;
use spatiand_shell::{HostRow, HostStatus, HostTab, PairingView, RemoteEntry, Shell};

use crate::state::ClientState;

/// What this session knows about one host right now. Written by the host's thread, read by
/// the compositor to fill in the settings page and the launcher.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostView {
    /// What the host calls itself, once it has said.
    pub name: Option<String>,
    pub link: Link,
    /// What it offers, as of the last time it said.
    pub apps: Vec<RemoteApp>,
    /// A game there asked the pad to rumble, and nothing has played it yet. Taken by the
    /// compositor, which owns the only motors.
    pub rumble: Option<(u16, u16)>,
    /// What this host has said about the clipboard and nothing has acted on yet. A queue
    /// rather than the latest, because an announcement, a paste's question and its answer are
    /// three different things and losing any of them loses a paste.
    pub clipboard: Vec<spatiand_stream::control::Clipboard>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Link {
    #[default]
    Connecting,
    Online,
    Offline(String),
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApp {
    pub id: String,
    pub name: String,
    /// The icon the host sent, saved to a file. Its name changes when the picture does, so a
    /// replaced icon is loaded afresh rather than found in the icon cache under the old name.
    pub icon: Option<String>,
}

/// Where the icons hosts send are kept.
fn icon_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("spatiand")
        .join("remote-icons")
}

/// The app id a remote window is given: `remote.<host>.<app>`. One definition, because the
/// controller layouts, the title bar's icon and the window itself all key on it.
pub fn app_id(host: &str, app: &str) -> String {
    format!("{}{app}", app_id_prefix(host))
}

fn app_id_prefix(host: &str) -> String {
    format!("remote.{host}.")
}

/// A remote window's icon, for its title bar: the latest picture its host sent for it.
pub fn icon_for(app_id: &str) -> Option<std::path::PathBuf> {
    let path = icon_dir().join(format!("{}.png", file_safe(app_id)));
    path.exists().then_some(path)
}

/// Keep an icon a host sent. Returns the path the launcher should load it from.
fn store_icon(app_id: &str, png: &[u8]) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let dir = icon_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    png.hash(&mut hasher);
    let name = file_safe(app_id);
    let versioned = dir.join(format!("{name}-{:016x}.png", hasher.finish()));
    if !versioned.exists() {
        std::fs::write(&versioned, png).ok()?;
    }
    // And under the plain name, for the title bar, which only knows the app id.
    let _ = std::fs::write(dir.join(format!("{name}.png")), png);
    Some(versioned.to_string_lossy().to_string())
}

fn file_safe(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

/// Something asked of a host's thread.
#[derive(Debug)]
pub enum Command {
    Launch(String),
    /// Kill an application on the host, by catalogue id.
    ForceQuit(String),
    /// The whole state of the gamepad, for whatever is being played there.
    Pad(spatiand_stream::Pad),
    /// Something about the clipboard: what has been copied, a paste asking for bytes, or the
    /// bytes themselves. See `crate::clipboard`.
    Clipboard(spatiand_stream::control::Clipboard),
}

/// A host being shown in this session.
pub struct Remote {
    /// Kept so the thread can be asked to stop; dropping it closes the connection and the
    /// windows go with it.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// What it was added as. The key it is known by everywhere.
    pub host: String,
    view: Arc<Mutex<HostView>>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl Remote {
    /// Connect to a host and show whatever it opens.
    ///
    /// Returns as soon as the thread is running: connecting, pairing failures and everything
    /// else are reported in the log rather than here, because a host that is asleep is a normal
    /// thing for a session to start with.
    pub fn start(display: &mut DisplayHandle, config: Config) -> Result<Remote, String> {
        let (server, client) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| format!("could not make a socket pair: {e}"))?;
        display
            .insert_client(server, Arc::new(ClientState::default()))
            .map_err(|e| format!("could not attach the remote client: {e}"))?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let host = config.host.clone();
        let view: Arc<Mutex<HostView>> = Arc::default();
        let (commands, inbox) = tokio::sync::mpsc::unbounded_channel();
        {
            let stop = stop.clone();
            let view = view.clone();
            std::thread::Builder::new()
                .name(format!("remote-{host}"))
                .spawn(move || {
                    // A panic here closes this host's windows and says why. It must never take
                    // the session with it: the wearer is wearing the session.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        session::run(client, config, stop, view, inbox);
                    }));
                    if let Err(panic) = result {
                        let what = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "something unprintable".into());
                        log::error!("the remote session died: {what}");
                    }
                })
                .map_err(|e| format!("could not start the remote thread: {e}"))?;
        }
        Ok(Remote {
            stop,
            host,
            view,
            commands,
        })
    }

    pub fn view(&self) -> HostView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn launch(&self, app: &str) {
        let _ = self.commands.send(Command::Launch(app.to_string()));
    }

    pub fn force_quit(&self, app: &str) {
        let _ = self.commands.send(Command::ForceQuit(app.to_string()));
    }

    fn pad(&self, state: spatiand_stream::Pad) {
        let _ = self.commands.send(Command::Pad(state));
    }

    /// Tell this host something about the clipboard.
    fn clipboard(&self, what: spatiand_stream::control::Clipboard) {
        let _ = self.commands.send(Command::Clipboard(what));
    }

    /// Whatever it has said about the clipboard since the last look.
    fn take_clipboard(&self) -> Vec<spatiand_stream::control::Clipboard> {
        self.view
            .lock()
            .map(|mut v| std::mem::take(&mut v.clipboard))
            .unwrap_or_default()
    }

    /// Whether an application id belongs to this host.
    fn owns(&self, app_id: &str) -> bool {
        app_id.starts_with(&app_id_prefix(&self.host))
    }

    /// A game there asked for rumble, if one has since the last look.
    fn take_rumble(&self) -> Option<(u16, u16)> {
        self.view.lock().ok()?.rumble.take()
    }
}

fn config_for(entry: &crate::prefs::RemoteHost, microphone: bool) -> Result<Config, String> {
    Ok(Config {
        host: entry.host.clone(),
        fingerprint: entry.fingerprint.parse()?,
        identity_dir: spatiand_track::config::config_dir(),
        render_node: std::env::var("SPATIAND_RENDER_NODE")
            .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        launch: entry.launch.clone(),
        microphone,
    })
}

/// Every host this session knows, a pairing if one is under way, and the bookkeeping that
/// keeps the settings page and the launcher in step with them.
///
/// The compositor's whole involvement is [`tick`](Self::tick) once a frame and the handful
/// of calls that answer the shell's events.
#[derive(Default)]
pub struct Remotes {
    hosts: Vec<Remote>,
    /// Which host was last given the pad, and what it was given, so an unchanged pad costs
    /// nothing and a host that loses focus is told to let go exactly once.
    padded: Option<(String, spatiand_stream::Pad)>,
    /// Whether this session sends its microphone to a host that asks. From the preferences,
    /// once, at startup.
    microphone: bool,
    pairing: Option<PairingInProgress>,
    /// What the shell was last given, so it is only given something when it changed.
    shown: Option<(Vec<HostRow>, Vec<HostTab>)>,
    shown_pairing: Option<PairingView>,
}

struct PairingInProgress {
    pairing: pairing::Pairing,
    /// The wearer has said the codes match.
    confirmed: bool,
    /// Over: kept only so the page can say how it ended.
    finished: Option<Result<String, String>>,
}

impl Remotes {
    /// Start every host in the preferences.
    ///
    /// A host that is asleep, unreachable or has not paired with this session costs one line
    /// in the log and nothing else — the session carries on, and keeps trying.
    pub fn start(display: &mut DisplayHandle, prefs: &crate::prefs::Prefs) -> Remotes {
        let mut remotes = Remotes {
            microphone: prefs.remote_microphone,
            ..Remotes::default()
        };
        for entry in &prefs.remotes {
            remotes.add(display, entry);
        }
        remotes
    }

    fn add(&mut self, display: &mut DisplayHandle, entry: &crate::prefs::RemoteHost) {
        let config = match config_for(entry, self.microphone) {
            Ok(config) => config,
            Err(e) => {
                log::error!("remote {}: {e}", entry.host);
                return;
            }
        };
        match Remote::start(display, config) {
            Ok(remote) => {
                log::info!("remote: watching {}", entry.host);
                self.hosts.push(remote);
            }
            Err(e) => log::error!("remote {}: {e}", entry.host),
        }
    }

    /// Start an application the launcher asked for.
    pub fn launch(&self, host: &str, app: &str) {
        match self.hosts.iter().find(|r| r.host == host) {
            Some(remote) => remote.launch(app),
            None => log::warn!("remote: no host called {host} to start {app} on"),
        }
    }

    /// Kill the application behind a remote window, named by the window's app id.
    pub fn force_quit(&self, app_id: &str) {
        for remote in &self.hosts {
            if let Some(app) = app_id.strip_prefix(&app_id_prefix(&remote.host)) {
                remote.force_quit(app);
                return;
            }
        }
        log::warn!("no host for {app_id}");
    }

    pub fn pair(&mut self, address: String) {
        log::info!("pairing with {address}");
        self.pairing = Some(PairingInProgress {
            pairing: pairing::Pairing::start(address, spatiand_track::config::config_dir()),
            confirmed: false,
            finished: None,
        });
    }

    pub fn confirm(&mut self) {
        if let Some(p) = self.pairing.as_mut() {
            p.confirmed = true;
        }
    }

    pub fn cancel(&mut self) {
        if self.pairing.take().is_some() {
            log::info!("pairing cancelled");
        }
    }

    /// Stop knowing a host: stop showing it, and take it out of the preferences.
    pub fn forget(&mut self, address: &str, prefs: &mut crate::prefs::Prefs) {
        // Dropping the handle stops its thread, and its windows go with the connection.
        self.hosts.retain(|r| r.host != address);
        prefs.remotes.retain(|r| r.host != address);
        prefs.save();
        log::info!("forgot {address}");
    }

    /// Hand the gamepad to whichever host owns the window in front of the wearer.
    ///
    /// Called every frame with the focused application's id, or `None` for a local one.
    /// Whatever the mapper made of the wearer's controller — thumbsticks, the head on the
    /// spare axes, a Bluetooth pad — is the same report a local game would be given, so a
    /// remote application is played exactly as a local one is, including its own layout.
    ///
    /// Sent only when it differs from the last one, and a host losing focus is told the pad
    /// is at rest: a stick left pushed over walks an avatar into a wall.
    pub fn pad(&mut self, focused: Option<&str>, state: spatiand_stream::Pad) {
        let now = focused
            .filter(|app_id| self.hosts.iter().any(|h| h.owns(app_id)))
            .map(|app_id| app_id.to_string());
        let was = self.padded.as_ref().map(|(host, _)| host.clone());
        if was != now {
            // Whoever had it, let go.
            if let Some(app_id) = was {
                if let Some(host) = self.hosts.iter().find(|h| h.owns(&app_id)) {
                    host.pad(spatiand_stream::Pad::default());
                }
            }
            self.padded = now.clone().map(|app_id| (app_id, spatiand_stream::Pad::default()));
        }
        let Some(app_id) = now else { return };
        if self.padded.as_ref().map(|(_, last)| *last) == Some(state) {
            return;
        }
        if let Some(host) = self.hosts.iter().find(|h| h.owns(&app_id)) {
            host.pad(state);
        }
        self.padded = Some((app_id, state));
    }

    /// What a game on any host has asked the motors to do since the last look.
    pub fn rumble(&self) -> Option<(u16, u16)> {
        self.hosts.iter().find_map(|host| host.take_rumble())
    }

    /// Everything the hosts have said about the clipboard since the last look, each with the
    /// host that said it.
    pub fn clipboard_said(&self) -> Vec<(String, spatiand_stream::control::Clipboard)> {
        self.hosts
            .iter()
            .flat_map(|host| {
                host.take_clipboard()
                    .into_iter()
                    .map(|what| (host.host.clone(), what))
            })
            .collect()
    }

    /// Pass on what the session's clipboard has to say.
    pub fn clipboard_say(&self, say: crate::clipboard::Say) {
        match say {
            crate::clipboard::Say::Everyone { except, what } => {
                for host in &self.hosts {
                    if Some(&host.host) == except.as_ref() {
                        continue;
                    }
                    host.clipboard(what.clone());
                }
            }
            crate::clipboard::Say::Just { host, what } => {
                if let Some(remote) = self.hosts.iter().find(|h| h.host == host) {
                    remote.clipboard(what);
                }
            }
        }
    }

    /// Which hosts are no longer connected, so the board can drop what they were holding.
    pub fn offline(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|host| !matches!(host.view().link, Link::Online))
            .map(|host| host.host.clone())
            .collect()
    }

    /// Once a frame: finish a pairing that both ends agreed to, and tell the shell anything
    /// that changed.
    pub fn tick(
        &mut self,
        display: &mut DisplayHandle,
        prefs: &mut crate::prefs::Prefs,
        shell: &mut Shell,
    ) {
        self.settle_pairing(display, prefs);

        let (rows, tabs) = self.describe(prefs);
        if self.shown.as_ref() != Some(&(rows.clone(), tabs.clone())) {
            shell.set_hosts(rows.clone(), tabs.clone());
            self.shown = Some((rows, tabs));
        }
        if let Some(view) = self.pairing_view() {
            if self.shown_pairing.as_ref() != Some(&view) {
                shell.set_pairing(view.clone());
                self.shown_pairing = Some(view);
            }
        }
    }

    fn settle_pairing(&mut self, display: &mut DisplayHandle, prefs: &mut crate::prefs::Prefs) {
        let Some(p) = self.pairing.as_mut() else { return };
        if p.finished.is_some() {
            return;
        }
        let paired = match p.pairing.state().outcome {
            None => return,
            Some(Err(e)) => {
                p.finished = Some(Err(e));
                return;
            }
            // The host said yes. Written down only once the wearer has said so too: the host
            // saying yes proves only that *something* said yes.
            Some(Ok(paired)) if p.confirmed => paired,
            Some(Ok(_)) => return,
        };
        let address = p.pairing.address.clone();
        p.finished = Some(Ok(paired.name.clone()));

        let entry = crate::prefs::RemoteHost {
            host: address.clone(),
            fingerprint: paired.fingerprint.to_string(),
            launch: Vec::new(),
        };
        // Pairing again with a host already known replaces it, which is what pairing again
        // after the host forgot this headset needs.
        self.hosts.retain(|r| r.host != address);
        prefs.remotes.retain(|r| r.host != address);
        prefs.remotes.push(entry.clone());
        prefs.save();
        log::info!("paired with {} at {address}", paired.name);
        self.add(display, &entry);
    }

    fn pairing_view(&self) -> Option<PairingView> {
        let p = self.pairing.as_ref()?;
        let state = p.pairing.state();
        let address = p.pairing.address.clone();
        let host_accepted = matches!(state.outcome, Some(Ok(_)));
        let (status, finished, succeeded) = match &p.finished {
            Some(Ok(name)) => (
                format!("Paired with {name}. Its apps are in the launcher."),
                true,
                true,
            ),
            Some(Err(e)) => (e.clone(), true, false),
            None => match (&state.code, p.confirmed, host_accepted) {
                (None, _, _) => (format!("Looking for {address}…"), false, false),
                (Some(_), false, _) => (
                    format!(
                        "Check that {address} shows the same code, and answer yes there. Then \
                         press A here if they match."
                    ),
                    false,
                    false,
                ),
                (Some(_), true, false) => (
                    format!("Waiting for {address} to be told yes…"),
                    false,
                    false,
                ),
                (Some(_), true, true) => ("Saving…".into(), false, false),
            },
        };
        Some(PairingView {
            address,
            code: state.code,
            status,
            host_accepted,
            finished,
            succeeded,
        })
    }

    /// The settings page's rows and the launcher's tabs, in the order the hosts were added.
    fn describe(&self, prefs: &crate::prefs::Prefs) -> (Vec<HostRow>, Vec<HostTab>) {
        let mut rows = Vec::new();
        let mut tabs = Vec::new();
        for entry in &prefs.remotes {
            let view = self
                .hosts
                .iter()
                .find(|r| r.host == entry.host)
                .map(|r| r.view())
                .unwrap_or_default();
            let label = view.name.clone().unwrap_or_else(|| entry.host.clone());
            let status = match view.link {
                Link::Online => HostStatus::Online,
                Link::Connecting => HostStatus::Connecting,
                Link::Offline(_) => HostStatus::Offline,
                Link::Refused(_) => HostStatus::Refused,
            };
            rows.push(HostRow {
                label: label.clone(),
                address: entry.host.clone(),
                status,
            });
            tabs.push(HostTab {
                label,
                address: entry.host.clone(),
                online: status == HostStatus::Online,
                apps: view
                    .apps
                    .iter()
                    .map(|a| RemoteEntry {
                        id: a.id.clone(),
                        name: a.name.clone(),
                        icon: a.icon.clone(),
                    })
                    .collect(),
            });
        }
        (rows, tabs)
    }

    /// For the headless snapshot: whether any host has a window to show yet.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
