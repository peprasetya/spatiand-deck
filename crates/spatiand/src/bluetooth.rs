//! Bluetooth devices, for the shell's Bluetooth page.
//!
//! Through `bluetoothctl`, BlueZ's own command-line client, rather than a D-Bus binding: it is
//! on every system that has BlueZ at all, the handful of questions asked here are one command
//! each, and it keeps a D-Bus stack out of the compositor for the sake of one menu.
//!
//! Everything runs on a thread of its own. `connect` in particular can take the whole of its
//! timeout when a device is asleep, and the compositor thread draws the glasses.
//!
//! The list is only refreshed while the page is open — every two seconds, which is how a
//! keyboard that wakes up shows up as connected without anyone pressing anything.

use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use spatiand_shell::{BluetoothAction, BluetoothView, DeviceRow, Shell};

const TOOL: &str = "bluetoothctl";
const REFRESH: Duration = Duration::from_secs(2);
/// How long a connect may try. A sleeping device is found within a few seconds of waking;
/// longer than this and the wearer is better told to wake it.
const CONNECT_TIMEOUT_S: u32 = 15;

/// Is there anything here to ask?
pub fn available() -> bool {
    Command::new(TOOL)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

enum Ask {
    Refresh,
    Do(BluetoothAction, String),
}

enum Heard {
    State { powered: Option<bool>, devices: Vec<DeviceRow> },
    Began(String),
    /// A command finished: the address it was about (if any) and what to tell the wearer.
    Done(Option<String>, String),
}

pub struct Devices {
    asks: Sender<Ask>,
    heard: Receiver<Heard>,
    view: BluetoothView,
    last: Option<Instant>,
    changed: bool,
    /// The wizard's command, waiting for `tick` to hand it out.
    wizard: Option<String>,
}

impl Devices {
    pub fn start() -> Devices {
        let (asks, asked) = channel();
        let (tell, heard) = channel();
        let spawned = std::thread::Builder::new()
            .name("bluetooth".into())
            .spawn(move || worker(asked, tell));
        if let Err(e) = spawned {
            log::warn!("bluetooth: could not start its thread: {e}");
        }
        Devices {
            asks,
            heard,
            view: BluetoothView::default(),
            last: None,
            changed: false,
            wizard: None,
        }
    }

    /// Ask now rather than at the next tick; the page has just opened.
    pub fn refresh(&mut self) {
        self.last = Some(Instant::now());
        let _ = self.asks.send(Ask::Refresh);
    }

    /// Do what the page asked for.
    pub fn act(&mut self, action: BluetoothAction) {
        let name = |address: &str| {
            self.view
                .devices
                .iter()
                .find(|d| d.address == address)
                .map(|d| d.label.clone())
                .unwrap_or_else(|| address.to_string())
        };
        let label = match &action {
            BluetoothAction::Connect(a)
            | BluetoothAction::Disconnect(a)
            | BluetoothAction::Forget(a) => name(a),
            _ => String::new(),
        };
        if action == BluetoothAction::Add {
            // The wizard, which is still the right tool for discovery and passkeys.
            return self.open_wizard();
        }
        if let BluetoothAction::Connect(a)
        | BluetoothAction::Disconnect(a)
        | BluetoothAction::Forget(a) = &action
        {
            // At once, so a second press while this one waits in line does nothing.
            if !self.view.busy.contains(a) {
                self.view.busy.push(a.clone());
                self.changed = true;
            }
        }
        let _ = self.asks.send(Ask::Do(action, label));
    }

    fn open_wizard(&mut self) {
        match spatiand_platform::settings_command("bluetooth") {
            Some(command) => {
                log::info!("bluetooth: {command}");
                self.wizard = Some(command);
            }
            None => {
                self.view.note = "There is no pairing wizard on this system.".into();
                self.changed = true;
            }
        }
    }

    /// Feed the shell whatever has been learned, and keep the list fresh while it is showing.
    /// Returns a command to launch as a window, when the wizard was asked for.
    pub fn tick(&mut self, shell: &mut Shell) -> Option<String> {
        let open = shell.mode() == spatiand_shell::Mode::Bluetooth;
        if open && self.last.map_or(true, |t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        if !open {
            self.last = None;
        }
        loop {
            match self.heard.try_recv() {
                Ok(Heard::State { powered, devices }) => {
                    self.view.powered = powered;
                    self.view.devices = devices;
                }
                Ok(Heard::Began(address)) => {
                    if !self.view.busy.contains(&address) {
                        self.view.busy.push(address);
                    }
                }
                Ok(Heard::Done(address, note)) => {
                    if let Some(address) = address {
                        self.view.busy.retain(|b| *b != address);
                    }
                    self.view.note = note;
                    // Say what changed at once rather than two seconds later.
                    let _ = self.asks.send(Ask::Refresh);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
            self.changed = true;
        }
        if std::mem::take(&mut self.changed) {
            shell.set_bluetooth(self.view.clone());
        }
        self.wizard.take()
    }
}

fn worker(asked: Receiver<Ask>, tell: Sender<Heard>) {
    while let Ok(first) = asked.recv() {
        // Everything queued behind a slow connect, taken at once: the actions in order, and
        // however many refreshes piled up as one, afterwards.
        let mut batch = vec![first];
        while let Ok(more) = asked.try_recv() {
            batch.push(more);
        }
        let mut refresh = false;
        for ask in batch {
            let (action, label) = match ask {
                Ask::Refresh => {
                    refresh = true;
                    continue;
                }
                Ask::Do(action, label) => (action, label),
            };
            let address = match &action {
                BluetoothAction::Connect(a)
                | BluetoothAction::Disconnect(a)
                | BluetoothAction::Forget(a) => Some(a.clone()),
                _ => None,
            };
            if let Some(a) = &address {
                let _ = tell.send(Heard::Began(a.clone()));
            }
            let note = perform(&action, &label);
            log::info!("bluetooth: {note}");
            if tell.send(Heard::Done(address, note)).is_err() {
                return;
            }
        }
        if refresh {
            let (powered, devices) = read_state();
            if tell.send(Heard::State { powered, devices }).is_err() {
                return;
            }
        }
    }
}

fn perform(action: &BluetoothAction, label: &str) -> String {
    let timeout = CONNECT_TIMEOUT_S.to_string();
    let (args, did, failed): (Vec<&str>, String, String) = match action {
        BluetoothAction::Connect(a) => (
            vec!["--timeout", &timeout, "connect", a],
            format!("Connected {label}."),
            format!(
                "{label} did not connect. Wake it with a key or its button and try again; if it \
                 still will not, it may have paired again under another address."
            ),
        ),
        BluetoothAction::Disconnect(a) => (
            vec!["disconnect", a],
            format!("Disconnected {label}."),
            format!("{label} would not disconnect."),
        ),
        BluetoothAction::Forget(a) => (
            vec!["remove", a],
            format!("Forgot {label}."),
            format!("Could not forget {label}."),
        ),
        BluetoothAction::Power(on) => (
            vec!["power", if *on { "on" } else { "off" }],
            format!("Bluetooth is {}.", if *on { "on" } else { "off" }),
            "Bluetooth did not switch. Something may be blocking it (rfkill).".into(),
        ),
        BluetoothAction::Add | BluetoothAction::None => return String::new(),
    };
    if let Err(e) = Command::new(TOOL).args(&args).output() {
        return format!("Could not run {TOOL}: {e}");
    }
    // Judged by what BlueZ says afterwards, not by the command: a connect that runs out of
    // time exits 0 and prints nothing about failing.
    let worked = match action {
        BluetoothAction::Connect(a) => connected(a) == Some(true),
        BluetoothAction::Disconnect(a) => connected(a) != Some(true),
        BluetoothAction::Forget(a) => run(&["info", a]).map_or(true, |i| field(&i, "Paired") != Some("yes")),
        BluetoothAction::Power(on) => {
            run(&["show"]).and_then(|s| field(&s, "Powered").map(|v| v == "yes")) == Some(*on)
        }
        _ => true,
    };
    if worked {
        did
    } else {
        failed
    }
}

fn connected(address: &str) -> Option<bool> {
    run(&["info", address]).and_then(|i| field(&i, "Connected").map(|v| v == "yes"))
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new(TOOL).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_state() -> (Option<bool>, Vec<DeviceRow>) {
    let powered = run(&["show"]).and_then(|s| field(&s, "Powered").map(|v| v == "yes"));
    // `devices Paired` is BlueZ 5.65 and later; `paired-devices` is what came before it.
    let listing = run(&["devices", "Paired"])
        .filter(|s| s.lines().any(|l| l.starts_with("Device ")))
        .or_else(|| run(&["paired-devices"]))
        .unwrap_or_default();
    let mut devices: Vec<DeviceRow> = listing
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Device ")?;
            let (address, _) = rest.split_once(' ').unwrap_or((rest, ""));
            if !is_address(address) {
                return None;
            }
            let info = run(&["info", address]).unwrap_or_default();
            Some(device_from_info(address, &info))
        })
        .collect();
    // Connected first, then by name, so the answer to "is it connected?" is at the top.
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.address.cmp(&b.address))
    });
    (powered, devices)
}

fn is_address(s: &str) -> bool {
    s.len() == 17 && s.split(':').count() == 6
}

fn field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.lines().find_map(|l| {
        let (k, v) = l.trim().split_once(':')?;
        (k == name).then(|| v.trim())
    })
}

fn device_from_info(address: &str, info: &str) -> DeviceRow {
    let label = field(info, "Alias")
        .or_else(|| field(info, "Name"))
        .filter(|s| !s.is_empty())
        .unwrap_or(address)
        .to_string();
    // "Battery Percentage: 0x55 (85)"
    let battery = field(info, "Battery Percentage").and_then(|v| {
        let inner = v.split_once('(')?.1.trim_end_matches(')');
        inner.trim().parse().ok()
    });
    DeviceRow {
        label,
        address: address.to_string(),
        kind: field(info, "Icon").unwrap_or("").to_string(),
        connected: field(info, "Connected") == Some("yes"),
        battery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "Device 11:6C:9F:CE:14:2C (public)
\tName: BT5.0 Keyboard
\tAlias: BT5.0 Keyboard
\tIcon: input-keyboard
\tPaired: yes
\tConnected: no
\tUUID: Human Interface Device    (00001812-0000-1000-8000-00805f9b34fb)
\tBattery Percentage: 0x55 (85)
";

    #[test]
    fn a_device_is_read_from_bluetoothctl_info() {
        let d = device_from_info("11:6C:9F:CE:14:2C", INFO);
        assert_eq!(d.label, "BT5.0 Keyboard");
        assert_eq!(d.kind, "input-keyboard");
        assert!(!d.connected);
        assert_eq!(d.battery, Some(85));
    }

    #[test]
    fn a_device_with_nothing_to_say_is_named_by_its_address() {
        let d = device_from_info("AA:BB:CC:DD:EE:FF", "");
        assert_eq!(d.label, "AA:BB:CC:DD:EE:FF");
        assert_eq!(d.battery, None);
    }

    #[test]
    fn only_real_addresses_are_devices() {
        assert!(is_address("11:6C:9F:CE:14:2C"));
        assert!(!is_address("Media"));
        assert!(!is_address("0000110a-0000-1000-8000-00805f9b34fb"));
    }
}
