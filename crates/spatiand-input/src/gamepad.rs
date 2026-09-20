//! Other controllers — a Bluetooth or USB pad — read through evdev and merged with the Deck.
//!
//! The kernel's own drivers already decode these (xpad, hid-playstation, hid-nintendo,
//! xpadneo), so unlike the Deck there is nothing to take over: open the event node and read.
//! Each is grabbed while open, so its presses reach only the layout — the game is shown the
//! virtual pad instead, and a pad that also reached it directly would be a second controller.
//!
//! Found by rescanning `/proc/bus/input/devices` every couple of seconds, which is how a pad
//! paired mid-session joins without anything having to listen for hotplug.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const RESCAN: Duration = Duration::from_secs(2);

/// What one pad currently reads, in the Deck's terms.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GamepadState {
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub l1: bool,
    pub r1: bool,
    pub l2: bool,
    pub r2: bool,
    pub select: bool,
    pub start: bool,
    /// The Xbox, PlayStation or Home button: what STEAM is on the Deck.
    pub guide: bool,
    pub left_stick_click: bool,
    pub right_stick_click: bool,
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// −1..1, +y up.
    pub left_stick: (f32, f32),
    pub right_stick: (f32, f32),
    /// 0..1.
    pub left_trigger: f32,
    pub right_trigger: f32,
}

/// A joystick in the kernel's device list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoystickNode {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    /// `eventN`.
    pub event: String,
}

/// Every joystick worth merging, from the text of `/proc/bus/input/devices`.
///
/// Leaves out Valve's own devices — the Deck is read over hidraw, and `28de:11ff` is Steam's
/// virtual pad — the virtual pad Spatiand itself creates, and the separate motion-sensor
/// devices some drivers add beside a pad, which carry no buttons.
pub fn parse_joysticks(text: &str) -> Vec<JoystickNode> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut vendor = 0u16;
        let mut product = 0u16;
        let mut event = None;
        let mut joystick = false;
        // `None` when the block does not say, which only a test fixture does not.
        let mut pad_buttons = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("N: Name=") {
                name = rest.trim_matches('"').to_string();
            } else if let Some(rest) = line.strip_prefix("I: ") {
                for field in rest.split_whitespace() {
                    if let Some(v) = field.strip_prefix("Vendor=") {
                        vendor = u16::from_str_radix(v, 16).unwrap_or(0);
                    } else if let Some(p) = field.strip_prefix("Product=") {
                        product = u16::from_str_radix(p, 16).unwrap_or(0);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("B: KEY=") {
                pad_buttons = Some(has_pad_buttons(rest));
            } else if let Some(rest) = line.strip_prefix("H: Handlers=") {
                for handler in rest.split_whitespace() {
                    if handler.starts_with("js") {
                        joystick = true;
                    } else if handler.starts_with("event") {
                        event = Some(handler.to_string());
                    }
                }
            }
        }
        let ours = name == crate::virtual_pad::NAME;
        let valve = vendor == 0x28DE;
        let sensors = name.contains("Motion Sensors") || name.contains("IMU");
        // A joystick node is not a joystick. The kernel gives one to anything with an odd
        // axis: a Bluetooth keyboard with a built-in trackpad reported `js0` for a single
        // ABS_MISC, was grabbed here as a controller, and from then on typed nothing and moved
        // no pointer. A pad has pad buttons; a keyboard does not.
        let buttons = pad_buttons.unwrap_or(true);
        if let (true, Some(event)) = (joystick && buttons && !ours && !valve && !sensors, event) {
            out.push(JoystickNode {
                name,
                vendor,
                product,
                event,
            });
        }
    }
    out
}

/// Whether a `B: KEY=` bitmap has any joystick or gamepad button: `BTN_TRIGGER` (0x120) to
/// `BTN_THUMBR` (0x13E). Keyboards and trackpads use the codes either side of that range.
fn has_pad_buttons(bitmap: &str) -> bool {
    // Words of a `long`, highest first; the last word holds bits 0..64.
    let words: Vec<u64> = bitmap
        .split_whitespace()
        .rev()
        .map(|w| u64::from_str_radix(w, 16).unwrap_or(0))
        .collect();
    (0x120u16..=0x13E).any(|bit| {
        let (word, offset) = (bit as usize / 64, bit as usize % 64);
        words.get(word).is_some_and(|w| w & (1 << offset) != 0)
    })
}

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

const BTN_SOUTH: u16 = 0x130;
const BTN_EAST: u16 = 0x131;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const BTN_SELECT: u16 = 0x13A;
const BTN_START: u16 = 0x13B;
const BTN_MODE: u16 = 0x13C;
const BTN_THUMBL: u16 = 0x13D;
const BTN_THUMBR: u16 = 0x13E;
const BTN_DPAD_UP: u16 = 0x220;
const BTN_DPAD_DOWN: u16 = 0x221;
const BTN_DPAD_LEFT: u16 = 0x222;
const BTN_DPAD_RIGHT: u16 = 0x223;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_GAS: u16 = 0x09;
const ABS_BRAKE: u16 = 0x0A;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

/// `EVIOCGRAB`.
const EVIOCGRAB: u64 = 0x4004_4590;

fn eviocgabs(axis: u16) -> u64 {
    (2 << 30) | (24 << 16) | ((b'E' as u64) << 8) | (0x40 + axis as u64)
}

struct Gamepad {
    node: JoystickNode,
    fd: OwnedFd,
    ranges: HashMap<u16, (i32, i32)>,
    /// Which axes the right stick and the triggers are on. Drivers differ: xpad and
    /// hid-playstation put the right stick on RX/RY and the triggers on Z/RZ, while plain HID
    /// pads put the right stick on Z/RZ and the triggers on GAS/BRAKE.
    right_stick: (u16, u16),
    triggers: (u16, u16),
    /// xpad reports the Xbox X and Y under the codes the kernel's layout document assigns to
    /// the top and left buttons the other way round, so Microsoft's pads are read by label.
    by_label: bool,
    state: GamepadState,
}

impl Gamepad {
    fn open(node: JoystickNode) -> io::Result<Self> {
        let path = std::ffi::CString::new(format!("/dev/input/{}", node.event)).unwrap();
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB as _, 1 as libc::c_ulong) } < 0 {
            log::warn!(
                "{}: could not take it for ourselves ({}); a game may also see it directly",
                node.name,
                io::Error::last_os_error()
            );
        }
        let mut ranges = HashMap::new();
        for axis in [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_GAS, ABS_BRAKE, ABS_HAT0X, ABS_HAT0Y] {
            let mut info = [0i32; 6];
            let rc = unsafe { libc::ioctl(fd.as_raw_fd(), eviocgabs(axis) as _, info.as_mut_ptr()) };
            if rc >= 0 && info[2] > info[1] {
                ranges.insert(axis, (info[1], info[2]));
            }
        }
        let has = |a| ranges.contains_key(&a);
        let (right_stick, triggers) = if has(ABS_RX) {
            ((ABS_RX, ABS_RY), (ABS_Z, ABS_RZ))
        } else {
            ((ABS_Z, ABS_RZ), (ABS_BRAKE, ABS_GAS))
        };
        log::info!(
            "controller joined: {} ({:04x}:{:04x}) on {}",
            node.name,
            node.vendor,
            node.product,
            node.event
        );
        Ok(Self {
            by_label: node.vendor == 0x045E,
            node,
            fd,
            ranges,
            right_stick,
            triggers,
            state: GamepadState::default(),
        })
    }

    fn normalised(&self, axis: u16, value: i32) -> f32 {
        let (min, max) = self.ranges.get(&axis).copied().unwrap_or((-32768, 32767));
        let span = (max - min).max(1) as f32;
        (value - min) as f32 / span
    }

    /// Drain pending events. `false` once the device has gone.
    fn read(&mut self) -> bool {
        let mut buf = [0u8; 24 * 32];
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return e.kind() == io::ErrorKind::WouldBlock;
            }
            if n == 0 {
                return false;
            }
            for chunk in buf[..n as usize].chunks_exact(24) {
                let kind = u16::from_le_bytes([chunk[16], chunk[17]]);
                let code = u16::from_le_bytes([chunk[18], chunk[19]]);
                let value = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
                match kind {
                    EV_KEY => self.key(code, value != 0),
                    EV_ABS => self.axis(code, value),
                    _ => {}
                }
            }
        }
    }

    fn key(&mut self, code: u16, down: bool) {
        let s = &mut self.state;
        match code {
            BTN_SOUTH => s.a = down,
            BTN_EAST => s.b = down,
            BTN_NORTH if self.by_label => s.x = down,
            BTN_WEST if self.by_label => s.y = down,
            BTN_NORTH => s.y = down,
            BTN_WEST => s.x = down,
            BTN_TL => s.l1 = down,
            BTN_TR => s.r1 = down,
            BTN_TL2 => s.l2 = down,
            BTN_TR2 => s.r2 = down,
            BTN_SELECT => s.select = down,
            BTN_START => s.start = down,
            BTN_MODE => s.guide = down,
            BTN_THUMBL => s.left_stick_click = down,
            BTN_THUMBR => s.right_stick_click = down,
            BTN_DPAD_UP => s.up = down,
            BTN_DPAD_DOWN => s.down = down,
            BTN_DPAD_LEFT => s.left = down,
            BTN_DPAD_RIGHT => s.right = down,
            _ => {}
        }
    }

    fn axis(&mut self, code: u16, value: i32) {
        let centred = |this: &Self| this.normalised(code, value) * 2.0 - 1.0;
        if code == ABS_X {
            self.state.left_stick.0 = centred(self);
        } else if code == ABS_Y {
            self.state.left_stick.1 = -centred(self);
        } else if code == self.right_stick.0 {
            self.state.right_stick.0 = centred(self);
        } else if code == self.right_stick.1 {
            self.state.right_stick.1 = -centred(self);
        } else if code == self.triggers.0 {
            self.state.left_trigger = self.normalised(code, value);
            self.state.l2 |= self.state.left_trigger > 0.95;
            if self.state.left_trigger < 0.9 {
                self.state.l2 = false;
            }
        } else if code == self.triggers.1 {
            self.state.right_trigger = self.normalised(code, value);
            self.state.r2 |= self.state.right_trigger > 0.95;
            if self.state.right_trigger < 0.9 {
                self.state.r2 = false;
            }
        } else if code == ABS_HAT0X {
            self.state.left = value < 0;
            self.state.right = value > 0;
        } else if code == ABS_HAT0Y {
            self.state.up = value < 0;
            self.state.down = value > 0;
        }
    }
}

/// Every extra controller that is plugged in or paired.
pub struct Gamepads {
    pads: Vec<Gamepad>,
    scanned: Option<Instant>,
    /// Nodes that would not open, so a pad this user cannot read is reported once, not every
    /// two seconds.
    refused: Vec<String>,
}

impl Default for Gamepads {
    fn default() -> Self {
        Self::new()
    }
}

impl Gamepads {
    pub fn new() -> Self {
        Self {
            pads: Vec::new(),
            scanned: None,
            refused: Vec::new(),
        }
    }

    /// Read every pad, looking for new ones every couple of seconds.
    pub fn poll(&mut self) -> Vec<GamepadState> {
        if self.scanned.is_none_or(|t| t.elapsed() >= RESCAN) {
            self.scanned = Some(Instant::now());
            self.rescan();
        }
        self.pads.retain_mut(|pad| {
            let alive = pad.read();
            if !alive {
                log::info!("controller left: {}", pad.node.name);
            }
            alive
        });
        self.pads.iter().map(|p| p.state).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.pads.iter().map(|p| p.node.name.clone()).collect()
    }

    fn rescan(&mut self) {
        let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") else {
            return;
        };
        for node in parse_joysticks(&text) {
            if self.pads.iter().any(|p| p.node.event == node.event)
                || self.refused.contains(&node.event)
            {
                continue;
            }
            let event = node.event.clone();
            let name = node.name.clone();
            match Gamepad::open(node) {
                Ok(pad) => self.pads.push(pad),
                Err(e) => {
                    log::warn!("found {name} on {event} but could not open it: {e}");
                    self.refused.push(event);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES: &str = r#"I: Bus=0003 Vendor=28de Product=1205 Version=0110
N: Name="Steam Deck"
H: Handlers=event10 js0

I: Bus=0003 Vendor=28de Product=11ff Version=0001
N: Name="Steam Virtual Gamepad"
H: Handlers=event18 js2

I: Bus=0003 Vendor=045e Product=028e Version=0114
N: Name="Microsoft X-Box 360 pad"
H: Handlers=event19 js6

I: Bus=0005 Vendor=054c Product=0ce6 Version=8100
N: Name="DualSense Wireless Controller"
H: Handlers=event20 js3

I: Bus=0005 Vendor=054c Product=0ce6 Version=8100
N: Name="DualSense Wireless Controller Motion Sensors"
H: Handlers=event21 js4

I: Bus=0005 Vendor=045e Product=0b13 Version=0515
N: Name="Xbox Wireless Controller"
H: Handlers=kbd event22 js5

I: Bus=0003 Vendor=046d Product=c52b Version=0111
N: Name="Logitech USB Receiver"
H: Handlers=sysfs kbd event5

I: Bus=0005 Vendor=04e8 Product=7021 Version=0001
N: Name="BT5.0 Keyboard"
H: Handlers=sysrq kbd leds event11 mouse3 js0 
B: KEY=101f 0 3f00033fff 0 0 483ffff17aff32d bfd5444600000000 ff0001 130ff38b17d007 ffff7bfad9415fff ffbeffdfffefffff fffffffffffffffe

I: Bus=0005 Vendor=054c Product=09cc Version=8100
N: Name="Wireless Controller"
H: Handlers=event23 js7
B: KEY=7fdb000000000000 0 0 0 0"#;

    #[test]
    fn only_real_extra_pads_are_merged() {
        // Our own pad and the Deck's own controller are left out; everything a person actually
        // plugged in is merged -- including a real wired Xbox 360 pad, which is the identity
        // this virtual one used to wear and could not have been told apart from.
        let found = parse_joysticks(DEVICES);
        let names: Vec<&str> = found.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Microsoft X-Box 360 pad",
                "DualSense Wireless Controller",
                "Xbox Wireless Controller",
                // A DualShock 4 as the kernel lists it, with its button bitmap.
                "Wireless Controller"
            ]
        );
        assert_eq!(found[1].event, "event20");
        assert_eq!((found[2].vendor, found[2].product), (0x045E, 0x0B13));
    }

    #[test]
    fn the_absinfo_ioctl_reads_the_right_axis() {
        assert_eq!(eviocgabs(ABS_X), 0x8018_4540);
        assert_eq!(eviocgabs(ABS_HAT0Y), 0x8018_4551);
    }
}
