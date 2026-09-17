//! The one gamepad a game sees.
//!
//! A uinput device with the identity of a wired Xbox 360 pad, which every input stack — SDL,
//! Wine's XInput, a Unity game's own — already knows how to read. Whatever the wearer is
//! actually holding, the layout drives this, and the physical devices are hidden from the game
//! (see `docs/input-mapper.md`), so a game can never see two pads and pick the wrong one.
//!
//! It also takes rumble. A game uploads a force-feedback effect and plays it; the kernel hands
//! both to us as requests on the same file descriptor, and [`VirtualPad::poll_rumble`] turns
//! them into two motor strengths for the Deck. Uinput blocks the game's upload until we answer
//! it, so this must be polled every frame or a game that rumbles stalls.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// What the device calls itself, and the identity it wears.
///
/// **Steam's own virtual gamepad, deliberately.** The obvious identity is a wired Xbox 360 pad
/// — every game knows it, SDL has a mapping for it, Wine turns it into XInput — and it was
/// `045e:028e` here until a game proved it wrong. Steam gives every game it launches a list of
/// several hundred controller ids that Steam Input handles, `045e:028e` among them, and Proton
/// honours that list: a device wearing a real controller's identity is not a controller as far
/// as the game is concerned, it is something Steam has promised to deal with.
///
/// Measured on the Deck with Stumble Guys running: a pad wearing `045e:028e` was never opened
/// by anything in the Wine prefix, while a second pad created at the same moment wearing this
/// identity was opened by `winedevice.exe` within four seconds. The wire format either way is
/// an Xbox pad's — the buttons and axes below are unchanged — so this is what it is called,
/// not what it is.
///
/// `SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1`, which Steam sets in every game's
/// environment, is the visible half of the same arrangement.
pub const NAME: &str = "Steam Virtual Gamepad";
pub const VENDOR: u16 = 0x28DE;
pub const PRODUCT: u16 = 0x11FF;
const VERSION: u16 = 0x0001;
const BUS_USB: u16 = 0x03;

/// What a launched application's environment needs so that it sees this pad and no other.
///
/// Two variables, and both were measured rather than assumed — see [`NAME`] for the identity
/// they name:
///
/// * `SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT` hides every other controller. Checked on the
///   Deck: under Proton 11 with this set, the only device Wine created was this pad.
/// * `SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD` makes SDL show a Steam virtual gamepad
///   at all. Without it SDL hides one, on the understanding that Steam will offer the same
///   controller through its own API — which is true for a game Steam launched and false for
///   anything Spatiand launched itself. Measured with the same pad up and one variable
///   changed: **one** joystick with it, **none** without.
pub fn hide_other_controllers() -> Vec<(String, String)> {
    vec![
        (
            "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT".into(),
            format!("0x{VENDOR:04x}/0x{PRODUCT:04x}"),
        ),
        (
            "SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD".into(),
            "1".into(),
        ),
    ]
}

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_FF: u16 = 0x15;
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
const FF_RUMBLE: u16 = 0x50;
const FF_GAIN: u16 = 0x60;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
/// The four spare axes — see [`Report::extra`].
///
/// Chosen for being axes nothing reinterprets. `ABS_GAS` and `ABS_BRAKE` are the obvious spare
/// pair and are exactly the wrong ones: Wine turns them into triggers, and a game would find
/// itself with its accelerator held down by a head that was merely facing forwards. A rudder,
/// a wheel and two tilts are read by a flight stick and by nothing a gamepad game does.
const ABS_RUDDER: u16 = 0x07;
const ABS_WHEEL: u16 = 0x08;
const ABS_TILT_X: u16 = 0x1A;
const ABS_TILT_Y: u16 = 0x1B;

/// The spare axes, in the order [`Report::extra`] carries them.
pub const EXTRA_AXES: [u16; 4] = [ABS_RUDDER, ABS_WHEEL, ABS_TILT_X, ABS_TILT_Y];

const UI_SET_EVBIT: u64 = 0x4004_5564;
const UI_SET_KEYBIT: u64 = 0x4004_5565;
const UI_SET_ABSBIT: u64 = 0x4004_5567;
const UI_SET_FFBIT: u64 = 0x4004_556B;
const UI_DEV_SETUP: u64 = 0x405C_5503;
const UI_ABS_SETUP: u64 = 0x401C_5504;
const UI_DEV_CREATE: u64 = 0x5501;
const UI_DEV_DESTROY: u64 = 0x5502;
/// `_IOWR('U', 200, struct uinput_ff_upload)`, the struct being 104 bytes on 64-bit.
const UI_BEGIN_FF_UPLOAD: u64 = 0xC068_55C8;
const UI_END_FF_UPLOAD: u64 = 0x4068_55C9;
/// `_IOWR('U', 202, struct uinput_ff_erase)`, 12 bytes.
const UI_BEGIN_FF_ERASE: u64 = 0xC00C_55CA;
const UI_END_FF_ERASE: u64 = 0x400C_55CB;

const FF_UPLOAD_LEN: usize = 104;
const INPUT_EVENT_LEN: usize = 24;

/// What the pad calls itself to the kernel, and so to everything that enumerates devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            name: NAME.into(),
            vendor: VENDOR,
            product: PRODUCT,
            version: VERSION,
        }
    }
}

impl Identity {
    /// A wired Xbox 360 pad, which is what this used to be.
    ///
    /// Kept so the experiment that moved it can be run again: create one of each while a game
    /// is running and watch which of the two `winedevice.exe` opens. See [`NAME`] for what
    /// that measurement said.
    pub fn xbox_360() -> Self {
        Identity {
            name: "Spatiand Gamepad (Xbox 360)".into(),
            vendor: 0x045E,
            product: 0x028E,
            version: 0x0114,
        }
    }
}

/// The buttons, in the order [`Report::buttons`] packs them.
pub const BUTTON_CODES: [u16; 11] = [
    0x130, // A
    0x131, // B
    0x133, // X
    0x134, // Y
    0x136, // left bumper
    0x137, // right bumper
    0x13D, // left stick click
    0x13E, // right stick click
    0x13B, // start
    0x13A, // select
    0x13C, // guide
];

/// The pad's whole state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Report {
    /// Bit `i` is [`BUTTON_CODES`]`[i]`.
    pub buttons: u16,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    /// −1..1, +y up.
    pub left: (f32, f32),
    pub right: (f32, f32),
    /// 0..1.
    pub left_trigger: f32,
    pub right_trigger: f32,
    /// Four axes beyond the six a gamepad has, −1..1, published as [`EXTRA_AXES`].
    ///
    /// Reserved rather than used: nothing writes them yet and they rest at centre, which is
    /// what a game reading them sees. They are here because the shape of a uinput device is
    /// fixed when it is created — a game enumerates it once, at startup — so an axis that
    /// might ever be wanted has to exist from the beginning or every application has to be
    /// restarted to find it.
    ///
    /// What they are for is a head: a gamepad has six axes, two sticks and two triggers, and a
    /// head has three of its own before anything is held in a hand. An application that wants
    /// to be driven by where the wearer is looking — the reason this is being reserved is
    /// Second Life through Firestorm — can be given yaw, pitch and roll as axes it already
    /// knows how to bind, with one spare.
    pub extra: [f32; 4],
}

#[derive(Debug, Clone, Copy)]
struct Effect {
    strong: u16,
    weak: u16,
    length: Duration,
}

pub struct VirtualPad {
    fd: OwnedFd,
    /// Every axis and button as last written, so only changes are sent.
    written: HashMap<(u16, u16), i32>,
    effects: HashMap<i16, Effect>,
    playing: HashMap<i16, Instant>,
    rumble: (u16, u16),
}

fn ioctl_ptr(fd: i32, request: u64, arg: *mut libc::c_void) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, arg) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_int(fd: i32, request: u64, value: libc::c_ulong) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, value) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl VirtualPad {
    pub fn create() -> io::Result<Self> {
        Self::create_as(&Identity::default())
    }

    /// Create the pad under a different name and identity.
    ///
    /// Exists because which identity a game's runtime accepts is not something that can be
    /// reasoned about: Steam hands every game a list of controller ids for Steam Input to
    /// handle, Proton reads the same list, and a device is either on the right side of it or
    /// invisible. Finding out which side takes a device, a game, and a `/proc` listing — see
    /// `examples/virtual-pad.rs`, which is how that experiment is run.
    pub fn create_as(identity: &Identity) -> io::Result<Self> {
        let path = std::ffi::CString::new("/dev/uinput").unwrap();
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let f = fd.as_raw_fd();

        for ev in [EV_KEY, EV_ABS, EV_FF] {
            ioctl_int(f, UI_SET_EVBIT, ev as _)?;
        }
        for code in BUTTON_CODES {
            ioctl_int(f, UI_SET_KEYBIT, code as _)?;
        }
        ioctl_int(f, UI_SET_FFBIT, FF_RUMBLE as _)?;
        ioctl_int(f, UI_SET_FFBIT, FF_GAIN as _)?;
        let axes: [(u16, i32, i32, i32, i32); 12] = [
            (ABS_X, -32768, 32767, 16, 128),
            (ABS_Y, -32768, 32767, 16, 128),
            (ABS_RX, -32768, 32767, 16, 128),
            (ABS_RY, -32768, 32767, 16, 128),
            (ABS_Z, 0, 255, 0, 0),
            (ABS_RZ, 0, 255, 0, 0),
            (ABS_HAT0X, -1, 1, 0, 0),
            (ABS_HAT0Y, -1, 1, 0, 0),
            (EXTRA_AXES[0], -32768, 32767, 16, 128),
            (EXTRA_AXES[1], -32768, 32767, 16, 128),
            (EXTRA_AXES[2], -32768, 32767, 16, 128),
            (EXTRA_AXES[3], -32768, 32767, 16, 128),
        ];
        for (code, min, max, fuzz, flat) in axes {
            ioctl_int(f, UI_SET_ABSBIT, code as _)?;
            // struct uinput_abs_setup { u16 code; (pad) struct input_absinfo { s32 value, min,
            // max, fuzz, flat, resolution } }
            let mut setup = [0u8; 28];
            setup[0..2].copy_from_slice(&code.to_le_bytes());
            setup[8..12].copy_from_slice(&min.to_le_bytes());
            setup[12..16].copy_from_slice(&max.to_le_bytes());
            setup[16..20].copy_from_slice(&fuzz.to_le_bytes());
            setup[20..24].copy_from_slice(&flat.to_le_bytes());
            ioctl_ptr(f, UI_ABS_SETUP, setup.as_mut_ptr().cast())?;
        }
        // struct uinput_setup { struct input_id { u16 bustype, vendor, product, version };
        // char name[80]; u32 ff_effects_max; }
        let mut setup = [0u8; 92];
        setup[0..2].copy_from_slice(&BUS_USB.to_le_bytes());
        setup[2..4].copy_from_slice(&identity.vendor.to_le_bytes());
        setup[4..6].copy_from_slice(&identity.product.to_le_bytes());
        setup[6..8].copy_from_slice(&identity.version.to_le_bytes());
        // 80 bytes with room for a terminator, and a name is not allowed to overrun it.
        let name = identity.name.as_bytes();
        let name = &name[..name.len().min(79)];
        setup[8..8 + name.len()].copy_from_slice(name);
        setup[88..92].copy_from_slice(&16u32.to_le_bytes());
        ioctl_ptr(f, UI_DEV_SETUP, setup.as_mut_ptr().cast())?;
        ioctl_int(f, UI_DEV_CREATE, 0)?;
        log::info!(
            "virtual gamepad created: {} ({:04x}:{:04x})",
            identity.name,
            identity.vendor,
            identity.product
        );

        Ok(Self {
            fd,
            written: HashMap::new(),
            effects: HashMap::new(),
            playing: HashMap::new(),
            rumble: (0, 0),
        })
    }

    /// Send whatever changed since the last report.
    pub fn send(&mut self, report: &Report) -> io::Result<()> {
        let stick = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i32;
        let trigger = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as i32;
        let hat = |negative: bool, positive: bool| positive as i32 - negative as i32;
        let mut wanted: Vec<(u16, u16, i32)> = BUTTON_CODES
            .iter()
            .enumerate()
            .map(|(i, code)| (EV_KEY, *code, (report.buttons >> i & 1) as i32))
            .collect();
        wanted.extend([
            (EV_ABS, ABS_X, stick(report.left.0)),
            // evdev's Y grows downward.
            (EV_ABS, ABS_Y, -stick(report.left.1)),
            (EV_ABS, ABS_RX, stick(report.right.0)),
            (EV_ABS, ABS_RY, -stick(report.right.1)),
            (EV_ABS, ABS_Z, trigger(report.left_trigger)),
            (EV_ABS, ABS_RZ, trigger(report.right_trigger)),
            (EV_ABS, ABS_HAT0X, hat(report.dpad_left, report.dpad_right)),
            (EV_ABS, ABS_HAT0Y, hat(report.dpad_up, report.dpad_down)),
            (EV_ABS, EXTRA_AXES[0], stick(report.extra[0])),
            (EV_ABS, EXTRA_AXES[1], stick(report.extra[1])),
            (EV_ABS, EXTRA_AXES[2], stick(report.extra[2])),
            (EV_ABS, EXTRA_AXES[3], stick(report.extra[3])),
        ]);
        let mut bytes = Vec::new();
        for (kind, code, value) in wanted {
            if self.written.get(&(kind, code)) != Some(&value) {
                self.written.insert((kind, code), value);
                bytes.extend_from_slice(&event(kind, code, value));
            }
        }
        if bytes.is_empty() {
            return Ok(());
        }
        bytes.extend_from_slice(&event(EV_SYN, 0, 0));
        let n = unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            // Forget what was written, so the next frame sends everything again rather than
            // leaving the game holding a button that was released during the failure.
            self.written.clear();
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Answer the game's force-feedback requests, and say how hard the motors should run now.
    ///
    /// `None` when that has not changed since the last call.
    pub fn poll_rumble(&mut self) -> Option<(u16, u16)> {
        let f = self.fd.as_raw_fd();
        let mut buf = [0u8; INPUT_EVENT_LEN * 16];
        loop {
            let n = unsafe { libc::read(f, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            for chunk in buf[..n as usize].chunks_exact(INPUT_EVENT_LEN) {
                let kind = u16::from_le_bytes([chunk[16], chunk[17]]);
                let code = u16::from_le_bytes([chunk[18], chunk[19]]);
                let value = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
                match (kind, code) {
                    (EV_UINPUT, UI_FF_UPLOAD) => self.upload(value as u32),
                    (EV_UINPUT, UI_FF_ERASE) => self.erase(value as u32),
                    (EV_FF, FF_GAIN) => {}
                    (EV_FF, id) => {
                        if value > 0 {
                            self.playing.insert(id as i16, Instant::now());
                        } else {
                            self.playing.remove(&(id as i16));
                        }
                    }
                    _ => {}
                }
            }
        }

        let now = Instant::now();
        let effects = &self.effects;
        self.playing.retain(|id, started| {
            effects.get(id).is_some_and(|e| {
                e.length.is_zero() || now.duration_since(*started) < e.length
            })
        });
        let mut strong = 0u16;
        let mut weak = 0u16;
        for id in self.playing.keys() {
            if let Some(e) = self.effects.get(id) {
                strong = strong.max(e.strong);
                weak = weak.max(e.weak);
            }
        }
        if (strong, weak) != self.rumble {
            self.rumble = (strong, weak);
            Some(self.rumble)
        } else {
            None
        }
    }

    fn upload(&mut self, request: u32) {
        let f = self.fd.as_raw_fd();
        let mut up = [0u8; FF_UPLOAD_LEN];
        up[0..4].copy_from_slice(&request.to_le_bytes());
        if ioctl_ptr(f, UI_BEGIN_FF_UPLOAD, up.as_mut_ptr().cast()).is_err() {
            return;
        }
        // struct ff_effect starts at 8: type u16, id s16, direction u16, trigger (4), replay
        // { length u16, delay u16 }, then the union at 16 — for rumble, strong and weak u16.
        let effect = &up[8..8 + 48];
        let kind = u16::from_le_bytes([effect[0], effect[1]]);
        let id = i16::from_le_bytes([effect[2], effect[3]]);
        let length = u16::from_le_bytes([effect[10], effect[11]]);
        let result: i32 = if kind == FF_RUMBLE {
            self.effects.insert(
                id,
                Effect {
                    strong: u16::from_le_bytes([effect[16], effect[17]]),
                    weak: u16::from_le_bytes([effect[18], effect[19]]),
                    length: Duration::from_millis(length as u64),
                },
            );
            0
        } else {
            -libc::EINVAL
        };
        up[4..8].copy_from_slice(&result.to_le_bytes());
        let _ = ioctl_ptr(f, UI_END_FF_UPLOAD, up.as_mut_ptr().cast());
    }

    fn erase(&mut self, request: u32) {
        let f = self.fd.as_raw_fd();
        let mut erase = [0u8; 12];
        erase[0..4].copy_from_slice(&request.to_le_bytes());
        if ioctl_ptr(f, UI_BEGIN_FF_ERASE, erase.as_mut_ptr().cast()).is_err() {
            return;
        }
        let id = u32::from_le_bytes([erase[8], erase[9], erase[10], erase[11]]) as i16;
        self.effects.remove(&id);
        self.playing.remove(&id);
        erase[4..8].copy_from_slice(&0i32.to_le_bytes());
        let _ = ioctl_ptr(f, UI_END_FF_ERASE, erase.as_mut_ptr().cast());
    }
}

impl Drop for VirtualPad {
    fn drop(&mut self) {
        let _ = ioctl_int(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0);
    }
}

fn event(kind: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_LEN] {
    // The kernel stamps uinput events itself; a zero time is fine.
    let mut e = [0u8; INPUT_EVENT_LEN];
    e[16..18].copy_from_slice(&kind.to_le_bytes());
    e[18..20].copy_from_slice(&code.to_le_bytes());
    e[20..24].copy_from_slice(&value.to_le_bytes());
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upload_ioctls_encode_the_structure_size() {
        let size = |request: u64| (request >> 16) & 0x3FFF;
        assert_eq!(size(UI_BEGIN_FF_UPLOAD), FF_UPLOAD_LEN as u64);
        assert_eq!(size(UI_END_FF_UPLOAD), FF_UPLOAD_LEN as u64);
        assert_eq!(size(UI_BEGIN_FF_ERASE), 12);
        assert_eq!(size(UI_DEV_SETUP), 92);
        assert_eq!(size(UI_ABS_SETUP), 28);
    }

    #[test]
    fn the_hiding_variables_name_this_pad_and_let_it_be_seen() {
        let env = hide_other_controllers();
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{key} is not set"))
        };
        // Steam's virtual gamepad, which is what this pad now is -- see `NAME`.
        assert_eq!(get("SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT"), "0x28de/0x11ff");
        // And without this SDL hides exactly that, which would leave a game with nothing.
        assert_eq!(get("SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD"), "1");
    }

    /// The one thing the unit tests cannot answer: whether the kernel accepts all of this and
    /// publishes a device a game would find.
    ///
    /// Needs `/dev/uinput`, which the desktop user has by ACL on SteamOS. Run it on the Deck
    /// itself -- the build container has no seat and no device nodes:
    ///
    /// ```text
    /// target/release/deps/spatiand_input-* --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs /dev/uinput and a real kernel"]
    fn the_kernel_publishes_it_as_a_gamepad() {
        let mut pad = VirtualPad::create().expect("create the pad");
        pad.send(&Report {
            buttons: 1,
            left: (1.0, 0.0),
            right_trigger: 1.0,
            ..Default::default()
        })
        .expect("send a report");
        let devices = std::fs::read_to_string("/proc/bus/input/devices").expect("read devices");
        let block = devices
            .split("\n\n")
            .find(|b| b.contains(NAME))
            .unwrap_or_else(|| panic!("{NAME} is not in the device list"));
        assert!(
            block.contains(&format!("Vendor={VENDOR:04x} Product={PRODUCT:04x}")),
            "wrong identity:\n{block}"
        );
        assert!(block.contains(" js"), "not a joystick:\n{block}");
        // And an ordinary process can open it. This is not a formality: a device node the
        // kernel publishes is no use to a game unless the game's own user is allowed to read
        // it, and that permission comes from a udev rule rather than from anything here. The
        // test runs as the user a game runs as, so opening it is the same question.
        let node = block
            .lines()
            .find_map(|line| line.strip_prefix("H: Handlers="))
            .and_then(|handlers| {
                handlers
                    .split_whitespace()
                    .find(|h| h.starts_with("event"))
                    .map(|h| format!("/dev/input/{h}"))
            })
            .unwrap_or_else(|| panic!("no event node:\n{block}"));
        // Retried, because the permission is applied by udev a moment after the kernel
        // publishes the device, and how long that takes is the interesting number: a game
        // starting seconds later never notices, and this test opening it in the same
        // microsecond always would.
        let began = std::time::Instant::now();
        loop {
            match std::fs::File::open(&node) {
                Ok(_) => {
                    println!("{node} became readable after {:?}", began.elapsed());
                    break;
                }
                Err(e) if began.elapsed() < std::time::Duration::from_secs(2) => {
                    let _ = e;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("a game could not open {node}: {e}"),
            }
        }
        // And it goes when the pad does, rather than leaving a controller behind.
        drop(pad);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let after = std::fs::read_to_string("/proc/bus/input/devices").expect("read devices");
        assert!(!after.contains(NAME), "the pad outlived the session");
    }

    #[test]
    fn the_spare_axes_are_four_distinct_ones_a_game_does_not_reinterpret() {
        use std::collections::HashSet;
        let spare: HashSet<u16> = EXTRA_AXES.into_iter().collect();
        assert_eq!(spare.len(), EXTRA_AXES.len(), "an axis is listed twice");
        for taken in [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y] {
            assert!(!spare.contains(&taken), "{taken:#x} is already the pad's");
        }
        // ABS_GAS and ABS_BRAKE, which Wine reads as an accelerator and a brake.
        for pedal in [0x09u16, 0x0A] {
            assert!(!spare.contains(&pedal), "{pedal:#x} is a pedal to Wine");
        }
    }

    #[test]
    fn a_report_that_says_nothing_about_the_spare_axes_leaves_them_centred() {
        // What every report is today: they are reserved, and reserved has to mean "at rest"
        // rather than "at one end", or a game bound to one would be held hard over by a
        // compositor that never intended to say anything at all.
        let report = Report::default();
        assert_eq!(report.extra, [0.0; 4]);
    }

    #[test]
    fn an_event_is_laid_out_as_the_kernel_reads_it() {
        let e = event(EV_ABS, ABS_RY, -5);
        assert_eq!(&e[16..18], &3u16.to_le_bytes());
        assert_eq!(&e[18..20], &4u16.to_le_bytes());
        assert_eq!(&e[20..24], &(-5i32).to_le_bytes());
    }
}
