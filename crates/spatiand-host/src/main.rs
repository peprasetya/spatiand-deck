//! `spatiand-host` — applications for a headset that is somewhere else.
//!
//! It runs applications on a machine with a GPU and sends each window to a Spatiand session,
//! one encoded stream per window. It is a daemon: applications belong to it, not to whoever is
//! currently watching, so a connection can come and go without anything closing.
//!
//! It is meant to be portable and small. Nothing in this crate knows what a headset is, what
//! the Deck is, or how a window is placed in a room — that is all at the other end. What it
//! needs from the machine it runs on is a render node and, for X11 applications, Xwayland.
//!
//! ```text
//! spatiand-host                      # serve
//! spatiand-host --list               # what the catalogue holds
//! spatiand-host --fingerprint        # what this host is
//! spatiand-host --pair               # pair a headset: compare a code, then say yes
//! spatiand-host --paired             # who is trusted
//! spatiand-host --trust <64 hex>     # trust one by name
//! spatiand-host --forget <8 hex>     # and undo it
//! spatiand-host --run <id>           # start an application and report its frames, no network
//! spatiand-host --run <id> --encode /tmp/out.hevc   # ...and encode them to a file
//! ```

mod apps;
mod audio;
mod xwayland;
mod compose;
mod control;
mod encode;
mod input;
mod microphone;
mod voice;
mod net;
mod pace;
mod pad;
mod pair;
mod route;
mod state;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::DeviceFd;

use compose::Composer;
use encode::{Coded, Encoder};
use net::{FromSession, Net, ToSession};
use pace::{Attention, Pacer, Rates};
use spatiand_stream::transport::Trust;
use spatiand_stream::{pairing_code, Fingerprint, Gate};
use spatiand_stream::{Catalog, Codec, HostMessage, Identity, WindowId, WindowInfo};


fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    let config_dir = pair::Paired::path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    let identity = match Identity::load_or_create(&config_dir) {
        Ok(identity) => identity,
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };
    let paired_path = pair::Paired::path();
    let mut paired = pair::Paired::load(&paired_path);

    if flag("--fingerprint") {
        println!("{}", identity.fingerprint());
        return;
    }
    if flag("--paired") {
        if paired.is_empty() {
            println!("nothing paired yet; run --pair with the headset ready");
        }
        for fingerprint in paired.fingerprints() {
            println!("{fingerprint}");
        }
        return;
    }
    if let Some(text) = value("--trust") {
        match text.parse() {
            Ok(fingerprint) => {
                let new = paired.trust(&fingerprint);
                if let Err(e) = paired.save(&paired_path) {
                    log::error!("could not write {}: {e}", paired_path.display());
                    std::process::exit(1);
                }
                println!(
                    "{} {fingerprint}",
                    if new { "trusting" } else { "already trusted" }
                );
            }
            Err(e) => {
                log::error!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(prefix) = value("--forget") {
        match paired.forget(&prefix) {
            Ok(gone) => {
                if let Err(e) = paired.save(&paired_path) {
                    log::error!("could not write {}: {e}", paired_path.display());
                    std::process::exit(1);
                }
                println!("forgot {gone}");
            }
            Err(e) => {
                log::error!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let library = Library::load();
    if flag("--list") {
        println!("{}", library.catalog_path.display());
        for app in &library.served.apps {
            println!(
                "  {:<20} {:<24} {:?}/{:?}",
                app.id, app.name, app.kind, app.eyes
            );
        }
        if library.served.apps.is_empty() {
            println!("  (nothing yet)");
        }
        return;
    }

    let pairing = flag("--pair");
    // A host is normally already running, as a service, and must not be restarted to pair:
    // that would close everything it has open. Ask the running one. Only when there is none
    // does `--pair` start a host itself, the way it always has.
    if pairing {
        if let Some(paired) = control::pair_with_running_host() {
            std::process::exit(if paired { 0 } else { 1 });
        }
    }
    if pairing {
        println!("this host is {}", identity.fingerprint());
        println!(
            "pairing: the next session to connect will be trusted, for the next {} seconds",
            pair::PAIRING_WINDOW.as_secs()
        );
    }

    let options = Options {
        run: value("--run"),
        seconds: value("--seconds").and_then(|v| v.parse().ok()).unwrap_or(20),
        encode_to: value("--encode"),
        port: value("--port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(library.settings.port),
        pairing,
        // `--run` on its own is the offline check: start an application, watch it draw, and
        // involve no network at all.
        serve: !flag("--no-serve"),
    };

    if let Err(e) = run_host(library, options, identity, paired, paired_path) {
        log::error!("{e}");
        std::process::exit(1);
    }
}

/// The catalogue as it is on disk, as it is served, and the host's settings — kept together
/// because they are reloaded together.
struct Library {
    catalog_path: std::path::PathBuf,
    settings_path: std::path::PathBuf,
    /// What is sent: the settings app first, then the file's entries, each with its icon
    /// rendered.
    served: Catalog,
    settings: spatiand_host_catalog::Settings,
    seen: (Option<std::time::SystemTime>, Option<std::time::SystemTime>),
}

impl Library {
    fn load() -> Library {
        let mut library = Library {
            catalog_path: spatiand_host_catalog::catalog_path(),
            settings_path: spatiand_host_catalog::settings_path(),
            served: Catalog::default(),
            settings: Default::default(),
            seen: (None, None),
        };
        library.read_catalog();
        library.read_settings();
        library
    }

    fn read_catalog(&mut self) {
        self.seen.0 = spatiand_host_catalog::modified(&self.catalog_path);
        let catalog = match spatiand_host_catalog::load_catalog(&self.catalog_path) {
            Ok(catalog) => catalog,
            Err(e) => {
                // Keep serving what was there: a file saved half-edited by hand should not
                // empty the launcher.
                log::error!("{e}; keeping the catalogue as it was");
                return;
            }
        };
        let entries = spatiand_platform::scan();
        let mut apps = Vec::new();
        if let Some(settings) = std::env::current_exe()
            .ok()
            .and_then(|exe| spatiand_host_catalog::settings_app(&exe))
        {
            apps.push(settings);
        } else {
            log::warn!("spatiand-host-config is not beside this program; no settings app to offer");
        }
        apps.extend(catalog.apps);
        for app in &mut apps {
            app.icon_png = spatiand_host_catalog::icons::icon_png(app, &entries);
        }
        let with_icons = apps.iter().filter(|a| a.icon_png.is_some()).count();
        log::info!(
            "catalogue: {} application(s), {with_icons} with an icon",
            apps.len()
        );
        self.served = Catalog { apps };
    }

    fn read_settings(&mut self) {
        self.seen.1 = spatiand_host_catalog::modified(&self.settings_path);
        match spatiand_host_catalog::load_settings(&self.settings_path) {
            Ok(settings) => {
                log::info!(
                    "settings: {} kbit/s, {} fps, idle {} fps after {} ms",
                    settings.max_kbit,
                    settings.max_fps,
                    settings.idle_fps,
                    settings.idle_after_ms
                );
                self.settings = settings;
            }
            Err(e) => log::error!("{e}; keeping the settings as they were"),
        }
    }

    /// Re-read whichever file changed. Says which did.
    fn reload_if_changed(&mut self) -> (bool, bool) {
        let apps = spatiand_host_catalog::modified(&self.catalog_path) != self.seen.0;
        let settings = spatiand_host_catalog::modified(&self.settings_path) != self.seen.1;
        if apps {
            self.read_catalog();
        }
        if settings {
            self.read_settings();
        }
        (apps, settings)
    }

    fn rates(&self, current: Rates) -> Rates {
        Rates {
            max_fps: self.settings.max_fps,
            ..current
        }
    }
}

struct Options {
    run: Option<String>,
    seconds: u64,
    encode_to: Option<String>,
    port: u16,
    pairing: bool,
    serve: bool,
}

/// One window's encoder, and what has been done with it.
struct Stream {
    encoder: Encoder,
    /// The commit count last encoded, so an unchanged window costs nothing.
    encoded_at: u64,
    frame: u32,
    /// Set until the session has been sent a frame it can start decoding from.
    wants_keyframe: bool,
    /// When this window was last encoded, on the host's clock.
    sent_at: Option<Duration>,
}

fn run_host(
    mut library: Library,
    options: Options,
    identity: Identity,
    mut paired: pair::Paired,
    paired_path: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<state::Host> = EventLoop::try_new()?;
    let mut display: Display<state::Host> = Display::new()?;
    let mut host = state::Host::new(&mut display, &event_loop.handle());
    // An X server for applications that only speak X11 — Firestorm, and most games.
    host.x11_display = xwayland::start(&display.handle(), &event_loop.handle());

    // A render node is all this needs. Scanout is what requires a DRM master, and there is
    // nothing to scan out to: the pictures leave over the network. That is also what lets the
    // host run on a machine whose desktop is in use, or on one with no desktop at all.
    let node =
        std::env::var("SPATIAND_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)?;
    let gbm = GbmDevice::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)))?;
    let egl_display = unsafe { EGLDisplay::new(gbm)? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    let mut composer = Composer::new();
    log::info!("host renderer ready on {node}");
    host.advertise_dmabuf(&renderer);

    let this_host = identity.fingerprint();
    let gate = std::sync::Arc::new(Gate::new(paired.fingerprints()));
    if options.pairing {
        gate.set_open(true);
    } else if paired.is_empty() {
        log::warn!("nothing is paired yet; run `spatiand-host --pair` with the headset ready");
    }
    // How a running host is asked to pair. Losing it costs pairing and nothing else, so a
    // failure is said and survived.
    let control = if options.serve {
        match control::Control::start() {
            Ok(control) => Some(control),
            Err(e) => {
                log::warn!("{e}; `--pair` will not reach this host");
                None
            }
        }
    } else {
        None
    };
    let net = if options.serve {
        let trust = Trust::Gate(gate.clone());
        let bind = format!("[::]:{}", options.port).parse().or_else(
            |_| -> Result<std::net::SocketAddr, std::net::AddrParseError> {
                format!("0.0.0.0:{}", options.port).parse()
            },
        )?;
        Some(Net::start(bind, identity, trust)?)
    } else {
        None
    };

    host.sounds = net.as_ref().map(|n| audio::Sounds::new(n.sender()));

    if let Some(id) = &options.run {
        match library.served.get(id) {
            Some(app) => {
                let sound = host.sounds.as_mut().map(|s| s.prepare(app)).unwrap_or_default();
                let pid = apps::launch(app, &host.socket_name, host.x11_display, &sound)?;
                host.app_of_pid.insert(pid as i32, app.id.clone());
            }
            None => {
                return Err(format!(
                    "no application called {id} in the catalogue; --list shows what there is"
                )
                .into())
            }
        }
    }

    let started = Instant::now();
    let mut reported = Instant::now();
    let mut pairing: Option<Pairing> = options.pairing.then(|| Pairing {
        client: None,
        until: Instant::now() + pair::PAIRING_WINDOW,
        waiting: None,
    });
    let mut attached = false;
    // Watches the audio graph: it puts an application's sound in the right sink even when the
    // application picked its own device, and notices when one of them wants a microphone.
    let router = route::Router::start();
    // Created before any application is, because a program reads the list of joysticks once.
    let mut pads = pad::Pads::start();
    // And the recording device, for the same reason: a program reads the list of microphones
    // once too, and one that appears when the wearer first speaks appears too late to be
    // chosen. It is quiet until there is something to put in it.
    microphone::start();
    let mut wiring = route::Wiring::default();
    let mut recording = false;
    // When the paired list was last read. `--trust` and `--forget` edit the file from another
    // process; a running host notices within a second rather than needing a restart, which
    // for a service holding applications open is not something to ask for.
    let modified = |path: &std::path::Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut paired_seen = modified(&paired_path);
    let mut paired_checked = Instant::now();
    // Whether the session that is here has been let in. A stranger that came through an open
    // pairing gate is connected but **not** admitted until somebody at this machine says its
    // code matches: until then it is told nothing and whatever it asks for is ignored.
    let mut admitted = false;
    // Set when a session arrives: one frame of every window, whether or not anything changed.
    let mut refresh_all = false;
    let mut pacer = Pacer::new(library.rates(Rates::default()));
    let mut titles: HashMap<u32, String> = HashMap::new();
    let mut library_checked = Instant::now();
    let mut streams: HashMap<u32, Stream> = HashMap::new();
    let mut encoded_file = options
        .encode_to
        .as_ref()
        .map(std::fs::File::create)
        .transpose()?;
    let mut encode_ms: Vec<f32> = Vec::new();
    let mut compose_ms: Vec<f32> = Vec::new();
    let mut sent_bytes: u64 = 0;
    let mut sent_frames: u64 = 0;

    while host.running {
        event_loop.dispatch(Some(Duration::from_millis(4)), &mut host)?;
        display.dispatch_clients(&mut host)?;
        let now = started.elapsed();

        // --- what the session said ---
        if let Some(net) = &net {
            for event in net.poll() {
                match event {
                    FromSession::Joined { who, address } => {
                        attached = true;
                        if let Some(s) = &host.sounds {
                            s.set_attached(true);
                        }
                        admitted = false;
                        let code = pairing_code(&this_host, &who);
                        if paired.contains(&who) {
                            log::info!("{} is a session this host knows", who.short());
                            admitted = true;
                        } else if let Some(p) = pairing.as_mut() {
                            match p.client {
                                // Somebody at a terminal is waiting to compare codes.
                                Some(client) => {
                                    log::info!(
                                        "session {} from {address} wants to pair; code {code}",
                                        who.short()
                                    );
                                    p.waiting = Some(who);
                                    if let Some(control) = &control {
                                        control.tell(client, &format!("code {} {code}", who.short()));
                                    }
                                }
                                // `--pair` started this host itself, with nobody on a socket to
                                // ask. The owner is at the terminal reading this log, so the
                                // code is said here, and the session trusted as it always was.
                                None => {
                                    log::info!("pairing with {} ({address}), code {code}", who.short());
                                    admit_pairing(&mut paired, &paired_path, &gate, &who);
                                    pairing = None;
                                    admitted = true;
                                }
                            }
                        } else {
                            // The gate should not have let it in; say no rather than serve it.
                            net.send(ToSession::Control(HostMessage::Refused {
                                version: spatiand_stream::VERSION,
                                reason: "this host has not been paired with this headset".into(),
                            }));
                        }
                        if admitted {
                            greet(net, &host, &library.served, &mut streams, recording);
                            refresh_all = true;
                        }
                    }
                    FromSession::Left => {
                        attached = false;
                        if let Some(s) = &host.sounds {
                            s.set_attached(false);
                        }
                        // Nobody is holding any of it any more: a stick left pushed over
                        // walks an avatar into a wall, and a button or a key left down is
                        // worse, because an X11 client repeats a key nobody released.
                        pads.rest();
                        input::release_everything(&mut host, 0);
                        admitted = false;
                        // A session that leaves mid-pairing takes the question with it.
                        if let Some(p) = pairing.as_mut() {
                            if p.waiting.take().is_some() {
                                if let (Some(client), Some(control)) = (p.client, &control) {
                                    control.tell(client, "refused");
                                }
                                pairing = None;
                                gate.set_open(false);
                            }
                        }
                    }
                    FromSession::Said(message) => {
                        if admitted {
                            said(&mut host, &library.served, message, &mut streams, &mut pads);
                        }
                    }
                }
            }
        }

        if paired_checked.elapsed() >= Duration::from_secs(1) {
            paired_checked = Instant::now();
            let now = modified(&paired_path);
            if now != paired_seen {
                paired_seen = now;
                paired = pair::Paired::load(&paired_path);
                gate.set_paired(paired.fingerprints());
                log::info!("the paired list changed on disk; {} trusted now", paired.fingerprints().len());
            }
        }

        // **Collect applications that have exited.** Without this every one that quit — or
        // was force-quit — stayed behind as a zombie, and stayed in `app_of_pid`, where a later
        // force-quit would signal whatever process had been given the number since.
        loop {
            let mut status = 0;
            // SAFETY: waitpid with WNOHANG on this process's own children.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if let Some(app) = host.app_of_pid.remove(&pid) {
                log::info!("{app} (pid {pid}) exited");
            }
        }

        // **Every turn, listening or not.** The kernel blocks a game's force-feedback upload
        // until it is answered, so a host that stopped collecting these would hang the first
        // game that rumbled.
        if let Some((strong, weak)) = pads.rumble() {
            if let (Some(net), true) = (&net, attached) {
                net.send(ToSession::Control(HostMessage::Rumble { strong, weak }));
            }
        }

        // What the router needs to know: which sink belongs to which application, and which
        // process belongs to which. Sent whenever it changes, which is rarely.
        let now_wiring = route::Wiring {
            sinks: host.sounds.as_ref().map(|s| s.sinks()).unwrap_or_default(),
            source: Some(microphone::NODE.to_string()),
            apps: host.app_of_pid.clone(),
        };
        if now_wiring != wiring {
            wiring = now_wiring.clone();
            router.wire(now_wiring);
        }
        for word in router.poll() {
            match word {
                route::Word::Recording(wanted) => {
                    recording = wanted;
                    log::info!(
                        "microphone: {}",
                        if wanted {
                            "an application here is listening; asking the session for the wearer's microphone"
                        } else {
                            "nothing here is listening any more"
                        }
                    );
                    if let (Some(net), true) = (&net, attached) {
                        net.send(ToSession::Control(HostMessage::Microphone { wanted }));
                    }
                }
            }
        }

        // The catalogue and the settings, edited by the settings app while this runs.
        if library_checked.elapsed() >= Duration::from_secs(1) {
            library_checked = Instant::now();
            let (apps_changed, settings_changed) = library.reload_if_changed();
            if apps_changed && admitted {
                if let Some(net) = &net {
                    net.send(ToSession::Control(HostMessage::Catalog {
                        apps: library.served.apps.clone(),
                    }));
                }
            }
            if settings_changed {
                pacer.set_rates(library.rates(pacer.rates()));
                // An encoder's bitrate is fixed when it is made, so new settings mean new
                // encoders. Each starts on a keyframe and tells the session what it is.
                streams.clear();
            }
        }

        // --- what a terminal asked for ---
        if let Some(control) = &control {
            for asked in control.poll() {
                match asked {
                    control::Asked::Pair { client } => {
                        if pairing.is_some() {
                            control.tell(client, "busy");
                            continue;
                        }
                        pairing = Some(Pairing {
                            client: Some(client),
                            until: Instant::now() + pair::PAIRING_WINDOW,
                            waiting: None,
                        });
                        gate.set_open(true);
                        log::info!("pairing is open for {} seconds", pair::PAIRING_WINDOW.as_secs());
                        control.tell(client, &format!("open {}", pair::PAIRING_WINDOW.as_secs()));
                    }
                    control::Asked::Answer { client, yes } => {
                        let Some(p) = pairing.as_ref() else { continue };
                        if p.client != Some(client) {
                            continue;
                        }
                        let Some(who) = p.waiting else { continue };
                        if yes {
                            admit_pairing(&mut paired, &paired_path, &gate, &who);
                            control.tell(client, &format!("paired {}", who.short()));
                            admitted = true;
                            if let Some(net) = &net {
                                greet(net, &host, &library.served, &mut streams, recording);
                            }
                            refresh_all = true;
                        } else {
                            log::info!("pairing with {} refused at this machine", who.short());
                            if let Some(net) = &net {
                                net.send(ToSession::Control(HostMessage::Refused {
                                    version: spatiand_stream::VERSION,
                                    reason: "the code was not confirmed on the host".into(),
                                }));
                            }
                            control.tell(client, "refused");
                        }
                        pairing = None;
                        gate.set_open(false);
                    }
                    control::Asked::Launch { client, app } => {
                        match start_app(&mut host, &library.served, &app) {
                            Ok(()) => control.tell(client, &format!("launched {app}")),
                            Err(e) => control.tell(client, &format!("failed {e}")),
                        }
                    }
                    control::Asked::List { client } => {
                        for tracked in &host.windows {
                            control.tell(
                                client,
                                &format!("window {} {} {}", tracked.id.0, tracked.app, title_of(tracked)),
                            );
                        }
                        control.tell(client, "end");
                    }
                    control::Asked::Close { client, window } => {
                        match host.windows.iter().find(|t| t.id.0 == window) {
                            Some(tracked) => {
                                close_window(&tracked.window);
                                control.tell(client, "done");
                            }
                            None => control.tell(client, "failed no such window"),
                        }
                    }
                    control::Asked::Kill { client, app } => {
                        let killed = kill_app(&mut host, &app);
                        control.tell(
                            client,
                            &if killed > 0 {
                                "done".to_string()
                            } else {
                                format!("failed {app} is not running")
                            },
                        );
                    }
                    control::Asked::Restart { client } => {
                        log::info!("asked to restart; exiting for the service manager");
                        control.tell(client, "restarting");
                        host.running = false;
                    }
                    control::Asked::Gone { client } => {
                        if pairing.as_ref().is_some_and(|p| p.client == Some(client)) {
                            log::info!("the terminal that asked to pair went away; pairing closed");
                            pairing = None;
                            gate.set_open(false);
                        }
                    }
                }
            }
        }
        // A window nobody used closes on its own. One somebody is answering stays open for
        // them: running out the clock on a person halfway through reading a code is rude.
        if pairing
            .as_ref()
            .is_some_and(|p| p.waiting.is_none() && Instant::now() > p.until)
        {
            if let (Some(client), Some(control)) = (pairing.as_ref().and_then(|p| p.client), &control) {
                control.tell(client, "closed");
            }
            pairing = None;
            gate.set_open(false);
            log::info!("pairing window closed with nobody connecting");
        }

        // --- tell the session what changed ---
        let arrived: Vec<WindowId> = std::mem::take(&mut host.arrived);
        let departed: Vec<WindowId> = std::mem::take(&mut host.departed);
        if let Some(net) = &net {
            for id in &arrived {
                if let Some(tracked) = host.windows.iter().find(|t| t.id == *id) {
                    net.send(ToSession::Control(HostMessage::Opened(info(tracked))));
                }
            }
            for id in &departed {
                net.send(ToSession::Control(HostMessage::Closed { window: *id }));
            }
        }
        for id in departed {
            pacer.forget(id);
            streams.remove(&id.0);
        }

        // --- titles ---
        if let (Some(net), true) = (&net, admitted) {
            for tracked in &host.windows {
                let title = title_of(tracked);
                if titles.get(&tracked.id.0) != Some(&title) {
                    titles.insert(tracked.id.0, title.clone());
                    net.send(ToSession::Control(HostMessage::Retitled {
                        window: tracked.id,
                        title,
                    }));
                }
            }
        }

        // X11's pointer cannot leave its screen, so the screen has to be big enough.
        host.fit_screen_to_x11();

        // --- let the windows draw ---
        let watching = attached || options.run.is_some();
        let output = host.output.clone();
        for tracked in &host.windows {
            pacer.set_attention(
                tracked.id,
                if watching {
                    Attention::Focused
                } else {
                    Attention::Detached
                },
            );
            if pacer.due(tracked.id, now) {
                tracked
                    .window
                    .send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                        Some(output.clone())
                    });
            }
        }

        // --- encode whatever is new, for whoever is listening ---
        if net.is_some() && attached || encoded_file.is_some() {
            let work: Vec<(u32, smithay::desktop::Window, (u32, u32), u64)> = host
                .windows
                .iter()
                .filter_map(|tracked| {
                    state::surface_of(&tracked.window)?;
                    let size = tracked.window.geometry().size;
                    Some((
                        tracked.id.0,
                        tracked.window.clone(),
                        (size.w.max(0) as u32, size.h.max(0) as u32),
                        tracked.commits,
                    ))
                })
                .collect();
            for (id, window, size, commits) in work {
                if size.0 == 0 || size.1 == 0 {
                    continue;
                }
                // Unchanged, and nobody has asked for a keyframe. A request has to be answered
                // even when nothing moved: a session whose picture broke on a still page waits
                // for exactly this, and without it waited until something on the page changed.
                let unchanged = streams
                    .get(&id)
                    .is_some_and(|stream| stream.encoded_at >= commits && !stream.wants_keyframe);
                if unchanged && !refresh_all {
                    continue;
                }
                // **At most one picture per display frame.** The pacer holds back frame
                // callbacks, but an application is free to commit without waiting for one, and
                // Chrome does: a busy page committed ~220 times a second, every commit was
                // encoded, and the stream ran at 90 Mbit/s against a 25 Mbit ceiling — because
                // the rate control budgets per frame at the display's rate. On a real link that
                // is loss, and every loss is a wait for a keyframe. A commit that arrives early
                // is not lost: it is still newer than what was sent, so the next slot sends it.
                let interval = Duration::from_micros(
                    1_000_000_000 / u64::from(pacer.rates().refresh_mhz.max(1)),
                );
                let too_soon = streams
                    .get(&id)
                    .and_then(|stream| stream.sent_at)
                    .is_some_and(|at| now.saturating_sub(at) < interval);
                if too_soon && !refresh_all {
                    continue;
                }
                // A window's size is not known when it opens — it has committed nothing yet —
                // so the size that matters is the one its stream is opened at, and that is
                // what the session is told. Said again whenever it changes: a resized window
                // gets a new encoder, and a session still holding the old picture size would
                // be sizing its decoder and its quad to a stream that no longer exists.
                let is_new = streams
                    .get(&id)
                    .map(|s| (s.encoder.width, s.encoder.height))
                    != Some(size);
                let began = Instant::now();
                match encode_window(
                    &mut streams,
                    &mut compose_ms,
                    &mut composer,
                    &mut renderer,
                    &node,
                    id,
                    &window,
                    size,
                    now.as_millis() as i64,
                    (pacer.rates().refresh_mhz / 1000).max(1),
                    // The ceiling is for everything together, so each window gets its share.
                    library.settings.max_kbit / host.windows.len().max(1) as u32,
                ) {
                    Ok(packets) => {
                        if let Some(stream) = streams.get_mut(&id) {
                            stream.encoded_at = commits;
                            stream.sent_at = Some(now);
                        }
                        if is_new {
                            if let (Some(net), true) = (&net, attached) {
                                net.send(ToSession::Control(HostMessage::Stream {
                                    window: WindowId(id),
                                    codec: Codec::H265,
                                    width: size.0,
                                    height: size.1,
                                    eyes: spatiand_stream::Eyes::Mono,
                                }));
                            }
                        }
                        encode_ms.push(began.elapsed().as_secs_f32() * 1000.0);
                        for packet in packets {
                            sent_bytes += packet.bytes.len() as u64;
                            sent_frames += 1;
                            if let Some(file) = encoded_file.as_mut() {
                                use std::io::Write;
                                file.write_all(&packet.bytes)?;
                            }
                            if let (Some(net), true) = (&net, attached) {
                                let frame = streams.get_mut(&id).map(|s| {
                                    s.frame = s.frame.wrapping_add(1);
                                    s.frame
                                });
                                net.send(ToSession::Video {
                                    window: id as u16,
                                    frame: frame.unwrap_or(0),
                                    viewport: 0,
                                    keyframe: packet.keyframe,
                                    captured_us: now.as_micros() as u64,
                                    bytes: packet.bytes,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("window {id}: {e}");
                        streams.remove(&id);
                    }
                }
            }
        }

        // One pass of everything, and done. This stayed set once it was first set — a single
        // missing line — so from the moment a session attached, every window was encoded on
        // every turn of this loop, changed or not: ~200 pictures a second and 90 Mbit/s for a
        // page that animates at 60, which on a real link is loss, and every loss is a wait
        // for a keyframe.
        if net.is_some() && attached || encoded_file.is_some() {
            refresh_all = false;
        }

        display.flush_clients()?;

        // A heartbeat, so that "the host has stopped saying anything" can be told apart from
        // "the host has stopped". They look identical in a log and mean opposite things.
        if reported.elapsed() >= Duration::from_secs(2) {
            report(
                &host,
                &mut encode_ms,
                &mut compose_ms,
                &mut sent_bytes,
                sent_frames,
                attached,
                reported.elapsed(),
            );
            reported = Instant::now();
        }
        if options.run.is_some()
            && !options.serve
            && started.elapsed() >= Duration::from_secs(options.seconds)
        {
            host.running = false;
        }
    }
    Ok(())
}

/// Kill an application and everything it started. Says how many process groups it reached.
///
/// Each application is started in its own process group (see `apps::launch`), so one signal
/// to the group reaches its children too — a browser's renderers, a game started by its
/// launcher — and cannot reach the host.
fn kill_app(host: &mut state::Host, app: &str) -> usize {
    let groups: Vec<i32> = host
        .app_of_pid
        .iter()
        .filter(|(_, a)| a.as_str() == app)
        .map(|(pid, _)| *pid)
        .collect();
    for pid in &groups {
        log::warn!("force-quitting {app} (process group {pid})");
        // SAFETY: a signal to a process group this host created.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    groups.len()
}

/// Start a catalogue entry — unless it is already running.
///
/// A session asks for what it wants to see, and on a reconnection it asks again: it cannot
/// know what is already running here. Starting a second copy is the wrong answer, because the
/// point of the host owning the application is that there is one of it.
fn start_app(host: &mut state::Host, catalog: &Catalog, app: &str) -> Result<(), String> {
    if host.windows.iter().any(|w| w.app == app) {
        log::info!("{app} is already running; showing the one that is");
        return Ok(());
    }
    let entry = catalog
        .get(app)
        .ok_or_else(|| format!("{app} is not in the catalogue"))?;
    let mut sound = host.sounds.as_mut().map(|s| s.prepare(entry)).unwrap_or_default();
    // And where to listen, which is the headset's own microphone once a session sends it. The
    // name is in the environment whether or not anything is arriving yet: an application reads
    // it once, at startup, and the wearer may start speaking later. See `microphone`.
    if !sound.is_empty() {
        sound.extend(microphone::environment());
    }
    let pid = apps::launch(entry, &host.socket_name, host.x11_display, &sound)
        .map_err(|e| format!("could not start {app}: {e}"))?;
    host.app_of_pid.insert(pid as i32, entry.id.clone());
    Ok(())
}

/// A pairing in progress.
struct Pairing {
    /// The terminal that asked for it, if one did.
    client: Option<u64>,
    until: Instant,
    /// A session that has arrived and whose code is being compared.
    waiting: Option<Fingerprint>,
}

/// Write a session down as trusted, and shut the gate behind it.
fn admit_pairing(
    paired: &mut pair::Paired,
    path: &std::path::Path,
    gate: &Gate,
    who: &Fingerprint,
) {
    if paired.trust(who) {
        match paired.save(path) {
            Ok(()) => log::info!("paired with {who}"),
            Err(e) => log::error!("could not write the paired list: {e}"),
        }
    }
    // Reading back what was just written is harmless, so there is no need to tell the
    // on-disk check that this write was ours.
    gate.set_paired(paired.fingerprints());
    gate.set_open(false);
}

/// Everything a session is told when it is let in.
fn greet(
    net: &Net,
    host: &state::Host,
    catalog: &Catalog,
    streams: &mut HashMap<u32, Stream>,
    recording: bool,
) {
    net.send(ToSession::Control(HostMessage::Welcome {
        version: spatiand_stream::VERSION,
        host: hostname(),
        reattached: !streams.is_empty(),
    }));
    // A session arriving to find something already listening should start sending straight
    // away, rather than when the next application opens a microphone.
    if recording {
        net.send(ToSession::Control(HostMessage::Microphone { wanted: true }));
    }
    net.send(ToSession::Control(HostMessage::Catalog {
        apps: catalog.apps.clone(),
    }));
    // Whatever is already open: a session arriving second finds the room as it was left.
    for tracked in &host.windows {
        net.send(ToSession::Control(HostMessage::Opened(info(tracked))));
        // And what it is *sending*, which a session that arrived after the stream opened has
        // no other way to learn. Without this a reattached session shows a window that never
        // paints: it has been told the window exists and never told what the pictures are.
        if let Some(stream) = streams.get(&tracked.id.0) {
            net.send(ToSession::Control(HostMessage::Stream {
                window: tracked.id,
                codec: Codec::H265,
                width: stream.encoder.width,
                height: stream.encoder.height,
                eyes: spatiand_stream::Eyes::Mono,
            }));
        }
    }
    for stream in streams.values_mut() {
        // Send the picture as it is now, even though nothing has changed.
        //
        // "Only send what changed" is right until somebody arrives, and then it is exactly
        // wrong: a still window has nothing to send, so a session that attaches to one is
        // shown nothing at all and sits looking at an empty frame for as long as the
        // application stays still. Forgetting what was last encoded forces one fresh frame.
        stream.encoded_at = 0;
        stream.wants_keyframe = true;
    }
}

/// Act on one thing the session said.
fn said(
    host: &mut state::Host,
    catalog: &Catalog,
    message: spatiand_stream::ClientMessage,
    streams: &mut HashMap<u32, Stream>,
    pads: &mut pad::Pads,
) {
    use spatiand_stream::ClientMessage as Says;
    match message {
        Says::Hello {
            version, session, ..
        } => {
            log::info!("session {session} speaks version {version}");
        }
        Says::Launch { app } => {
            if let Err(e) = start_app(host, catalog, &app) {
                log::warn!("a session asked for {app}: {e}");
            }
        }
        Says::Input { window, input } => {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u32)
                .unwrap_or(0);
            input::apply(host, window, input, time);
        }
        Says::Pad(state) => pads.apply(&state),
        Says::WantKeyframe { window } => {
            if let Some(stream) = streams.get_mut(&window.0) {
                // And a frame to put it in: asking for a keyframe when the window is still
                // would otherwise be answered by silence, which is what the asking was about.
                stream.encoded_at = 0;
                stream.wants_keyframe = true;
            }
        }
        Says::Configure {
            window,
            width,
            height,
        } => {
            // The wearer dragged the window's frame. Resizing the *application* is what makes
            // this feel like a window rather than a video of one: the picture stops being
            // stretched, and the application lays itself out for the shape it now has.
            let Some(tracked) = host.windows.iter().find(|t| t.id == window) else {
                return;
            };
            // Even, and within reason: a codec works in pairs of pixels, and a session asking
            // for something absurd should not take the encoder with it.
            let size: smithay::utils::Size<i32, smithay::utils::Logical> = (
                (width.clamp(160, 7680) as i32) & !1,
                (height.clamp(120, 4320) as i32) & !1,
            )
                .into();
            let id = tracked.id.0;
            let window = tracked.window.clone();
            let was = window.geometry().size;
            if was != size {
                log::info!("window {id} resized: {}x{} -> {}x{}", was.w, was.h, size.w, size.h);
            }
            if let Some(x11) = window.x11_surface() {
                if x11.geometry().size != size {
                    if let Err(e) = x11.configure(smithay::utils::Rectangle::new((0, 0).into(), size))
                    {
                        log::warn!("could not resize an X11 window: {e}");
                    }
                }
            } else if let Some(toplevel) = window.toplevel() {
                if toplevel.current_state().size != Some(size) {
                    toplevel.with_pending_state(|s| s.size = Some(size));
                    toplevel.send_pending_configure();
                }
            }
        }
        Says::Close { window } => {
            if let Some(tracked) = host.windows.iter().find(|t| t.id == window) {
                close_window(&tracked.window);
            }
        }
        Says::ForceQuit { app } => {
            if kill_app(host, &app) == 0 {
                log::warn!("asked to force-quit {app}, which is not running");
            }
        }
        other => log::debug!("nothing yet handles {other:?}"),
    }
}

/// Ask a window to close: `xdg_toplevel.close`, or `WM_DELETE_WINDOW` for an X11 one.
fn close_window(window: &smithay::desktop::Window) {
    if let Some(x11) = window.x11_surface() {
        if let Err(e) = x11.close() {
            log::warn!("could not ask an X11 window to close: {e}");
        }
    } else if let Some(toplevel) = window.toplevel() {
        toplevel.send_close();
    }
}

/// What a window calls itself, as its application last said.
fn title_of(tracked: &state::Tracked) -> String {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
    if let Some(x11) = tracked.window.x11_surface() {
        let title = x11.title();
        return if title.is_empty() { x11.class() } else { title };
    }
    let Some(toplevel) = tracked.window.toplevel() else {
        return String::new();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|d| d.title.clone()))
            .unwrap_or_default()
    })
}

fn info(tracked: &state::Tracked) -> WindowInfo {
    let size = tracked.window.geometry().size;
    WindowInfo {
        window: tracked.id,
        app: tracked.app.clone(),
        title: title_of(tracked),
        width: size.w.max(0) as u32,
        height: size.h.max(0) as u32,
        parent: None,
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|_| "host".into())
}

/// Composite one window and encode the result, opening an encoder the first time.
#[allow(clippy::too_many_arguments)]
fn encode_window(
    streams: &mut HashMap<u32, Stream>,
    compose_ms: &mut Vec<f32>,
    composer: &mut Composer,
    renderer: &mut GlesRenderer,
    node: &str,
    id: u32,
    window: &smithay::desktop::Window,
    size: (u32, u32),
    pts_ms: i64,
    fps: u32,
    kbit: u32,
) -> Result<Vec<Coded>, String> {
    // An encoder is fixed to one picture size, so a resized window gets a new one rather than a
    // stretched stream. It also owns the buffer everything is drawn into.
    let stale = streams
        .get(&id)
        .is_some_and(|s| s.encoder.width != size.0 || s.encoder.height != size.1);
    if stale {
        streams.remove(&id);
    }
    if !streams.contains_key(&id) {
        // The rate the session's display runs at, which is what the ceiling is shared out
        // over. Not how often this window will actually draw.
        let encoder = Encoder::new(node, Codec::H265, size.0, size.1, kbit.max(500), fps)
            .map_err(|e| e.to_string())?;
        streams.insert(
            id,
            Stream {
                encoder,
                encoded_at: 0,
                frame: 0,
                wants_keyframe: true,
                sent_at: None,
            },
        );
    }
    let stream = streams.get_mut(&id).expect("just made one");
    let drawing = Instant::now();
    composer.compose_into(renderer, window, stream.encoder.canvas(), size)?;
    compose_ms.push(drawing.elapsed().as_secs_f32() * 1000.0);
    let key = std::mem::take(&mut stream.wants_keyframe);
    stream
        .encoder
        .encode(pts_ms, key)
        .map_err(|e| e.to_string())
}

fn report(
    host: &state::Host,
    encode_ms: &mut Vec<f32>,
    compose_ms: &mut Vec<f32>,
    sent_bytes: &mut u64,
    sent_frames: u64,
    attached: bool,
    over: Duration,
) {
    if host.windows.is_empty() {
        log::info!("no windows yet");
        return;
    }
    log::debug!("loop alive, {} window(s)", host.windows.len());
    let secs = over.as_secs_f32().max(0.001);
    for tracked in &host.windows {
        let size = tracked.window.geometry().size;
        log::debug!(
            "window {} ({}) {}x{}",
            tracked.id.0,
            tracked.app,
            size.w,
            size.h
        );
    }
    if encode_ms.is_empty() {
        log::info!(
            "{} window(s), nothing to send{}",
            host.windows.len(),
            if attached { "" } else { ", nobody attached" }
        );
    } else {
        let n = encode_ms.len();
        let mean: f32 = encode_ms.iter().sum::<f32>() / n as f32;
        let worst = encode_ms.iter().cloned().fold(0.0f32, f32::max);
        let drawing: f32 = compose_ms.iter().sum::<f32>() / compose_ms.len().max(1) as f32;
        log::info!(
            "{windows} window(s), {n} frames in {secs:.1}s, {mean:.1} ms each ({drawing:.1} \
             drawing, worst {worst:.1}), {mbit:.1} Mbit/s, {sent_frames} sent",
            windows = host.windows.len(),
            mbit = *sent_bytes as f32 * 8.0 / secs / 1_000_000.0
        );
    }
    encode_ms.clear();
    compose_ms.clear();
    *sent_bytes = 0;
}
