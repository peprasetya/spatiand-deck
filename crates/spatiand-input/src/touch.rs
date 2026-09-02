//! A touchscreen, read straight from evdev.
//!
//! This is the one place in the project where evdev is the right layer rather than a
//! compromise. The controller is read as raw hidraw because `hid-steam` throws away exactly
//! the fields a 3D pointer needs; a touch panel has the opposite problem, in that the kernel
//! has already done the hard part. Multitouch protocol B is a *stateful* wire format — the
//! driver sends only what changed — and the kernel's input layer is what turns a stream of
//! partial updates into "finger 2 is at (x, y)". Reading the raw HID descriptor would mean
//! reimplementing that, per vendor, for nothing.
//!
//! Nothing here names a particular machine. A touchscreen is found by asking which device
//! claims to be one, which is why this sits in the input crate rather than behind a device
//! quirk.
//!
//! ## The shape of protocol B
//!
//! Events arrive in packets terminated by `SYN_REPORT`, and **nothing may be acted on until
//! that arrives**. A packet that sets a new X but has not yet delivered the matching Y is a
//! position that never existed; using it puts a finger briefly on the wrong row, which looks
//! like jitter and is really a framing bug.
//!
//! Within a packet, `ABS_MT_SLOT` selects which finger the following events describe, and that
//! selection persists — including across packets, which is why `slot` lives in the struct and
//! not in the loop. `ABS_MT_TRACKING_ID` is the finger's identity: a non-negative number when
//! it arrives, and −1 when it lifts. The slot is a channel; the tracking id is who is talking
//! on it.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

/// Fixed ceiling on simultaneous contacts.
///
/// The panel this was written against reports ten. Sixteen costs nothing and means a device
/// with more never writes past the end of the array — a slot number out of range is clamped,
/// not trusted.
pub const MAX_SLOTS: usize = 16;

// Event types and codes, from `linux/input-event-codes.h`. Spelled out rather than pulled from
// a crate: there are six of them and they have not changed in twenty years.
const EV_SYN: u16 = 0x00;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

/// Bit positions in the `ABS` capability mask, same numbering as the codes above.
const BIT_MT_SLOT: usize = ABS_MT_SLOT as usize;
const BIT_MT_POSITION_X: usize = ABS_MT_POSITION_X as usize;
const BIT_MT_POSITION_Y: usize = ABS_MT_POSITION_Y as usize;
/// `INPUT_PROP_DIRECT` — the contact surface is the display. This is what separates a
/// touchscreen from a touchpad, both of which report the same axes.
const INPUT_PROP_DIRECT: usize = 1;

/// One finger, in the digitiser's own normalised space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub slot: usize,
    /// The kernel's tracking id. Stable for as long as the finger is down.
    pub id: i32,
    /// 0..1 across the digitiser's reported range — *not* pixels, and not necessarily in the
    /// framebuffer's orientation. See [`Touchscreen::poll`].
    pub x: f32,
    pub y: f32,
}

/// What a completed packet did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchEvent {
    Down(Contact),
    Motion(Contact),
    Up { slot: usize },
}

/// A candidate device, found by what it says it can do.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchNode {
    pub name: String,
    pub path: PathBuf,
}

/// Every direct-touch multitouch device the kernel is advertising.
///
/// `/proc/bus/input/devices` is readable by anyone, which matters: the event node itself is
/// `root:input` with no ACL, so opening it needs the session's help. Shortlisting from a file
/// that needs no permission means exactly one privileged open, rather than a failed one per
/// device.
pub fn find_touchscreens() -> Vec<TouchNode> {
    match std::fs::read_to_string("/proc/bus/input/devices") {
        Ok(text) => parse_devices(&text),
        Err(e) => {
            log::warn!("cannot enumerate input devices: {e}");
            Vec::new()
        }
    }
}

/// Pick out the direct-touch multitouch devices from `/proc/bus/input/devices`.
///
/// Selection is by capability, never by name. The panel here happens to be called
/// `FTS3528:00 2808:1015`, and matching that string would be a device quirk hiding in the
/// wrong crate — as well as being wrong on the next revision of the same hardware.
pub fn parse_devices(text: &str) -> Vec<TouchNode> {
    let mut found = Vec::new();
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut handlers = "";
        let mut props = "";
        let mut abs = "";
        for line in block.lines() {
            let Some(rest) = line.strip_prefix("N: Name=") else {
                if let Some(rest) = line.strip_prefix("H: Handlers=") {
                    handlers = rest;
                } else if let Some(rest) = line.strip_prefix("B: PROP=") {
                    props = rest;
                } else if let Some(rest) = line.strip_prefix("B: ABS=") {
                    abs = rest;
                }
                continue;
            };
            name = rest.trim().trim_matches('"').to_string();
        }
        // Protocol B specifically: a device with positions but no slots is protocol A, which
        // is a different and much older wire format. Requiring the slot axis means the state
        // machine below never has to guess which format it is reading.
        let touchscreen = bitmask_has(props, INPUT_PROP_DIRECT)
            && bitmask_has(abs, BIT_MT_SLOT)
            && bitmask_has(abs, BIT_MT_POSITION_X)
            && bitmask_has(abs, BIT_MT_POSITION_Y);
        if !touchscreen {
            continue;
        }
        let Some(event) = handlers.split_whitespace().find(|h| h.starts_with("event")) else {
            continue;
        };
        found.push(TouchNode {
            name,
            path: PathBuf::from("/dev/input").join(event),
        });
    }
    found
}

/// Test a bit in one of the space-separated hex masks `/proc/bus/input/devices` prints.
///
/// The words are 64 bits each and printed **most significant first**, so the last word holds
/// bits 0..63. Reading them left to right — the obvious way — puts every bit in the wrong
/// place on any device whose mask needs more than one word, which is every touchscreen.
fn bitmask_has(mask: &str, bit: usize) -> bool {
    let words: Vec<&str> = mask.split_whitespace().collect();
    let word_from_end = bit / 64;
    if word_from_end >= words.len() {
        return false;
    }
    let word = words[words.len() - 1 - word_from_end];
    u64::from_str_radix(word, 16)
        .map(|w| w & (1u64 << (bit % 64)) != 0)
        .unwrap_or(false)
}

/// The range one axis reports, as `EVIOCGABS` gives it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub min: i32,
    pub max: i32,
}

impl Range {
    /// Where a raw reading sits in 0..1.
    ///
    /// A zero-width range would divide by zero, and a panel that reports one is broken in a way
    /// we cannot fix; returning the middle keeps a finger on screen instead of at infinity.
    pub fn normalise(&self, value: i32) -> f32 {
        let span = self.max - self.min;
        if span <= 0 {
            return 0.5;
        }
        ((value - self.min) as f32 / span as f32).clamp(0.0, 1.0)
    }
}

/// What one slot is currently doing, mid-packet.
///
/// Position is kept separately from the tracking id rather than inside an `Option`, because
/// the two are set by different events that can arrive in either order. Storing a position
/// only for live contacts loses the coordinates of a finger whose X arrived before its id.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SlotState {
    id: Option<i32>,
    x: i32,
    y: i32,
}

/// Protocol B decoded into contacts. No I/O, so the whole thing is testable from a byte log.
pub struct Multitouch {
    range_x: Range,
    range_y: Range,
    /// The slot subsequent events apply to. Persists across packets — the driver only sends
    /// `ABS_MT_SLOT` when the slot *changes*.
    slot: usize,
    /// Being edited by the packet in flight.
    pending: [SlotState; MAX_SLOTS],
    /// Committed at the last `SYN_REPORT`; what the outside world has been told.
    live: [SlotState; MAX_SLOTS],
}

impl Multitouch {
    pub fn new(range_x: Range, range_y: Range) -> Self {
        Self {
            range_x,
            range_y,
            slot: 0,
            pending: [SlotState::default(); MAX_SLOTS],
            live: [SlotState::default(); MAX_SLOTS],
        }
    }

    /// Feed one kernel event. Returns the events completed, which is empty except on
    /// `SYN_REPORT`.
    pub fn feed(&mut self, kind: u16, code: u16, value: i32) -> Vec<TouchEvent> {
        match kind {
            EV_ABS => {
                match code {
                    ABS_MT_SLOT => {
                        // Clamping rather than ignoring: a slot beyond our array still means
                        // "the following events are not about the previous finger", and
                        // leaving `slot` alone would misattribute them to it.
                        self.slot = (value.max(0) as usize).min(MAX_SLOTS - 1);
                    }
                    ABS_MT_TRACKING_ID => {
                        self.pending[self.slot].id = (value >= 0).then_some(value);
                    }
                    ABS_MT_POSITION_X => self.pending[self.slot].x = value,
                    ABS_MT_POSITION_Y => self.pending[self.slot].y = value,
                    _ => {}
                }
                Vec::new()
            }
            EV_SYN if code == SYN_REPORT => self.commit(),
            EV_SYN if code == SYN_DROPPED => {
                // The kernel's buffer overflowed and events were thrown away, so our idea of
                // where the fingers are is now fiction. The correct recovery is to re-read the
                // whole slot table; the honest cheap one is to lift everything, because a
                // drag that ends early is recoverable and a finger stuck down forever is not.
                let mut events = Vec::new();
                for slot in 0..MAX_SLOTS {
                    if self.live[slot].id.is_some() {
                        events.push(TouchEvent::Up { slot });
                    }
                    self.live[slot] = SlotState::default();
                    self.pending[slot] = SlotState::default();
                }
                log::debug!("touch: SYN_DROPPED, released {} contact(s)", events.len());
                events
            }
            _ => Vec::new(),
        }
    }

    /// Turn the difference between `pending` and `live` into events.
    fn commit(&mut self) -> Vec<TouchEvent> {
        let mut events = Vec::new();
        for slot in 0..MAX_SLOTS {
            let was = self.live[slot];
            let now = self.pending[slot];
            let contact = |s: SlotState| Contact {
                slot,
                id: s.id.unwrap_or(-1),
                x: self.range_x.normalise(s.x),
                y: self.range_y.normalise(s.y),
            };
            match (was.id, now.id) {
                (None, Some(_)) => events.push(TouchEvent::Down(contact(now))),
                (Some(_), None) => events.push(TouchEvent::Up { slot }),
                // A slot can be reused by a new finger without an intervening −1 if both
                // happen inside one packet. Treating that as motion would drag the old
                // contact across the screen to wherever the new finger landed.
                (Some(before), Some(after)) if before != after => {
                    events.push(TouchEvent::Up { slot });
                    events.push(TouchEvent::Down(contact(now)));
                }
                (Some(_), Some(_)) if (was.x, was.y) != (now.x, now.y) => {
                    events.push(TouchEvent::Motion(contact(now)))
                }
                _ => {}
            }
            self.live[slot] = now;
        }
        events
    }

    /// Contacts currently down, for anything that wants state rather than events.
    pub fn contacts(&self) -> impl Iterator<Item = Contact> + '_ {
        self.live.iter().enumerate().filter_map(|(slot, s)| {
            s.id.map(|id| Contact {
                slot,
                id,
                x: self.range_x.normalise(s.x),
                y: self.range_y.normalise(s.y),
            })
        })
    }
}

/// An open touchscreen.
pub struct Touchscreen {
    fd: OwnedFd,
    decoder: Multitouch,
    buf: [u8; std::mem::size_of::<InputEvent>() * 64],
}

/// `struct input_event`.
///
/// 24 bytes on 64-bit and 16 on 32-bit, which is why the timestamp is a `libc::timeval` rather
/// than a pair of `i64`s — hard-coding the 64-bit layout works on the machine this was written
/// on and silently misparses everywhere else.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

impl Touchscreen {
    /// Take an already-open descriptor.
    ///
    /// The caller opens it, because *how* differs by context and this crate should not care:
    /// the compositor goes through libseat, since the node is `root:input` with no ACL and
    /// only the session can hand it over, while a test or a probe opens it directly.
    pub fn from_fd(fd: OwnedFd) -> std::io::Result<Self> {
        // Non-blocking here rather than as an open flag the caller has to remember. A blocking
        // read from the render loop stalls the whole compositor until someone touches the
        // screen, which is a hang that looks exactly like a GPU fault.
        let raw = fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let range_x = axis_range(raw, ABS_MT_POSITION_X)?;
        let range_y = axis_range(raw, ABS_MT_POSITION_Y)?;
        log::info!(
            "touchscreen: x {}..{}, y {}..{}",
            range_x.min,
            range_x.max,
            range_y.min,
            range_y.max
        );
        Ok(Self {
            fd,
            decoder: Multitouch::new(range_x, range_y),
            buf: [0; std::mem::size_of::<InputEvent>() * 64],
        })
    }

    /// Drain whatever has arrived.
    ///
    /// Coordinates come out 0..1 in the *digitiser's* frame, which is its own business and not
    /// necessarily the framebuffer's — same origin corner if you are lucky, and a panel mounted
    /// rotated is not. Mapping that onto a screen belongs to whoever knows how the screen is
    /// mounted; see `spatiand::sidecar`.
    pub fn poll(&mut self) -> Vec<TouchEvent> {
        let mut events = Vec::new();
        loop {
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    self.buf.as_mut_ptr() as *mut libc::c_void,
                    self.buf.len(),
                )
            };
            if n <= 0 {
                // EAGAIN is the normal exit: nothing left to read.
                break;
            }
            let n = n as usize;
            let size = std::mem::size_of::<InputEvent>();
            for chunk in self.buf[..n].chunks_exact(size) {
                // The kernel writes whole events, and `chunks_exact` guarantees the length, so
                // this read is aligned and in range.
                let event: InputEvent = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                events.extend(self.decoder.feed(event.kind, event.code, event.value));
            }
            if n < self.buf.len() {
                break;
            }
        }
        events
    }

    pub fn contacts(&self) -> impl Iterator<Item = Contact> + '_ {
        self.decoder.contacts()
    }
}

/// `EVIOCGABS(axis)` — `_IOR('E', 0x40 + axis, struct input_absinfo)`.
fn axis_range(fd: std::os::fd::RawFd, axis: u16) -> std::io::Result<Range> {
    // `struct input_absinfo` is six `__s32`: value, minimum, maximum, fuzz, flat, resolution.
    let mut info = [0i32; 6];
    const READ: u32 = 2;
    let request = (READ << 30)
        | ((std::mem::size_of::<[i32; 6]>() as u32) << 16)
        | ((b'E' as u32) << 8)
        | (0x40 + axis as u32);
    let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, info.as_mut_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Range {
        min: info[1],
        max: info[2],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from the Steam Deck, trimmed to three devices: the touchscreen, the
    /// second collection on the same chip that has no touch axes at all, and the controller.
    const DEVICES: &str = "\
I: Bus=0003 Vendor=28de Product=1205 Version=0110
N: Name=\"Valve Software Steam Controller\"
P: Phys=usb-0000:04:00.4-3/input0
H: Handlers=event4 mouse0
B: PROP=0
B: EV=17
B: KEY=30000 0 0 0 0
B: REL=1943

I: Bus=0018 Vendor=2808 Product=1015 Version=0100
N: Name=\"FTS3528:00 2808:1015\"
P: Phys=i2c-FTS3528:00
H: Handlers=event5 mouse1
B: PROP=2
B: EV=1b
B: KEY=400 0 0 0 0 0
B: ABS=3273800000000003
B: MSC=20

I: Bus=0018 Vendor=2808 Product=1015 Version=0100
N: Name=\"FTS3528:00 2808:1015 UNKNOWN\"
P: Phys=i2c-FTS3528:00
H: Handlers=event6 mouse2
B: PROP=0
B: EV=1b
B: KEY=c03 0 0 0 0 0
B: ABS=1000003
B: MSC=10
";

    #[test]
    fn the_touchscreen_is_picked_out_of_a_real_device_list() {
        let found = parse_devices(DEVICES);
        assert_eq!(found.len(), 1, "found {found:?}");
        assert_eq!(found[0].path, PathBuf::from("/dev/input/event5"));
        assert_eq!(found[0].name, "FTS3528:00 2808:1015");
    }

    #[test]
    fn the_second_collection_on_the_same_chip_is_rejected() {
        // Same vendor, same product, same phys, adjacent event node — and no MT axes and no
        // DIRECT property. Anything matching on name or on vid/pid would take this one half
        // the time, and it never reports a touch.
        let sibling = DEVICES.split("\n\n").nth(2).expect("third block");
        assert!(parse_devices(sibling).is_empty());
    }

    #[test]
    fn a_mask_is_read_from_its_last_word_first() {
        // `B: ABS=3273800000000003` is one word, so bits 0..63 — ABS_X and ABS_Y at 0 and 1,
        // and the MT axes up at 47..61.
        assert!(bitmask_has("3273800000000003", 0));
        assert!(bitmask_has("3273800000000003", BIT_MT_SLOT));
        assert!(bitmask_has("3273800000000003", BIT_MT_POSITION_X));
        assert!(bitmask_has("3273800000000003", BIT_MT_POSITION_Y));
        // Two words: the *right-hand* one holds the low bits. Read left to right instead and
        // every bit above 63 lands 64 places from where it belongs.
        assert!(bitmask_has("1 0", 64));
        assert!(!bitmask_has("1 0", 0));
        assert!(!bitmask_has("0 1", 64));
        assert!(bitmask_has("0 1", 0));
    }

    #[test]
    fn a_mask_shorter_than_the_bit_asked_for_is_not_a_panic() {
        assert!(!bitmask_has("3", 200));
        assert!(!bitmask_has("", 0));
        assert!(!bitmask_has("nonsense", 0));
    }

    fn decoder() -> Multitouch {
        // The ranges the Deck's panel actually reports.
        Multitouch::new(Range { min: 0, max: 800 }, Range { min: 0, max: 1280 })
    }

    /// Feed a packet and return what it produced.
    fn packet(mt: &mut Multitouch, events: &[(u16, u16, i32)]) -> Vec<TouchEvent> {
        let mut out = Vec::new();
        for &(kind, code, value) in events {
            out.extend(mt.feed(kind, code, value));
        }
        out.extend(mt.feed(EV_SYN, SYN_REPORT, 0));
        out
    }

    #[test]
    fn nothing_is_reported_before_the_packet_ends() {
        // The whole reason SYN_REPORT exists. Acting on a half-delivered position puts the
        // finger somewhere it never was, for one frame.
        let mut mt = decoder();
        assert!(mt.feed(EV_ABS, ABS_MT_TRACKING_ID, 7).is_empty());
        assert!(mt.feed(EV_ABS, ABS_MT_POSITION_X, 400).is_empty());
        assert!(mt.feed(EV_ABS, ABS_MT_POSITION_Y, 640).is_empty());
        let events = mt.feed(EV_SYN, SYN_REPORT, 0);
        assert_eq!(
            events,
            vec![TouchEvent::Down(Contact {
                slot: 0,
                id: 7,
                x: 0.5,
                y: 0.5
            })]
        );
    }

    #[test]
    fn a_finger_goes_down_moves_and_lifts() {
        let mut mt = decoder();
        packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 0),
            ],
        );
        let moved = packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_POSITION_X, 800),
                (EV_ABS, ABS_MT_POSITION_Y, 1280),
            ],
        );
        assert_eq!(
            moved,
            vec![TouchEvent::Motion(Contact {
                slot: 0,
                id: 1,
                x: 1.0,
                y: 1.0
            })]
        );
        let lifted = packet(&mut mt, &[(EV_ABS, ABS_MT_TRACKING_ID, -1)]);
        assert_eq!(lifted, vec![TouchEvent::Up { slot: 0 }]);
        assert_eq!(mt.contacts().count(), 0);
    }

    #[test]
    fn a_packet_that_changes_nothing_reports_nothing() {
        // Panels resend the same coordinates while a finger rests. Reporting motion for those
        // would make a still finger look like a drag, and a slider under it would jitter.
        let mut mt = decoder();
        packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 100),
            ],
        );
        assert!(packet(&mut mt, &[(EV_ABS, ABS_MT_POSITION_X, 100)]).is_empty());
    }

    #[test]
    fn the_slot_selection_persists_across_packets() {
        // The driver sends ABS_MT_SLOT only when the slot changes, so a decoder that resets it
        // per packet attributes the second finger's whole drag to the first.
        let mut mt = decoder();
        packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 9),
                (EV_ABS, ABS_MT_POSITION_X, 200),
            ],
        );
        let moved = packet(&mut mt, &[(EV_ABS, ABS_MT_POSITION_X, 400)]);
        assert_eq!(moved.len(), 1);
        assert!(matches!(moved[0], TouchEvent::Motion(c) if c.slot == 1 && c.id == 9));
    }

    #[test]
    fn two_fingers_are_tracked_separately() {
        let mut mt = decoder();
        let down = packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 4),
                (EV_ABS, ABS_MT_POSITION_X, 200),
                (EV_ABS, ABS_MT_POSITION_Y, 320),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 5),
                (EV_ABS, ABS_MT_POSITION_X, 600),
                (EV_ABS, ABS_MT_POSITION_Y, 960),
            ],
        );
        assert_eq!(down.len(), 2);
        assert_eq!(mt.contacts().count(), 2);
        // Lifting one leaves the other exactly where it was.
        let lift = packet(
            &mut mt,
            &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
        );
        assert_eq!(lift, vec![TouchEvent::Up { slot: 0 }]);
        let left: Vec<Contact> = mt.contacts().collect();
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].slot, left[0].id), (1, 5));
    }

    #[test]
    fn a_slot_reused_inside_one_packet_is_a_lift_and_a_new_press() {
        // Otherwise the old contact appears to travel to wherever the new finger landed, and
        // anything holding a drag follows it there.
        let mut mt = decoder();
        packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 0),
            ],
        );
        let swapped = packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, 800),
            ],
        );
        assert_eq!(swapped.len(), 2);
        assert_eq!(swapped[0], TouchEvent::Up { slot: 0 });
        assert!(matches!(swapped[1], TouchEvent::Down(c) if c.id == 2));
    }

    #[test]
    fn a_dropped_packet_releases_every_finger() {
        // After SYN_DROPPED our slot table is fiction. A finger left stuck down holds a
        // slider forever; a drag cut short is merely annoying.
        let mut mt = decoder();
        packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
            ],
        );
        let dropped = mt.feed(EV_SYN, SYN_DROPPED, 0);
        assert_eq!(dropped.len(), 2, "both contacts should be released");
        assert!(dropped.iter().all(|e| matches!(e, TouchEvent::Up { .. })));
        assert_eq!(mt.contacts().count(), 0);
    }

    #[test]
    fn a_slot_beyond_the_array_cannot_write_past_the_end() {
        let mut mt = decoder();
        let events = packet(
            &mut mt,
            &[
                (EV_ABS, ABS_MT_SLOT, 9999),
                (EV_ABS, ABS_MT_TRACKING_ID, 3),
                (EV_ABS, ABS_MT_POSITION_X, 400),
            ],
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TouchEvent::Down(c) if c.slot == MAX_SLOTS - 1));
    }

    #[test]
    fn readings_are_normalised_against_the_reported_range() {
        // The digitiser's coordinate space is its own; assuming it matches the panel's pixels
        // is a coincidence on this hardware and wrong on the next.
        let r = Range { min: 100, max: 300 };
        assert_eq!(r.normalise(100), 0.0);
        assert_eq!(r.normalise(200), 0.5);
        assert_eq!(r.normalise(300), 1.0);
        // Panels do report outside their own advertised range at the very edge.
        assert_eq!(r.normalise(50), 0.0);
        assert_eq!(r.normalise(1000), 1.0);
    }

    #[test]
    fn a_degenerate_range_does_not_divide_by_zero() {
        let r = Range { min: 5, max: 5 };
        assert_eq!(r.normalise(5), 0.5);
    }

    #[test]
    fn an_input_event_is_the_size_the_kernel_writes() {
        // 24 bytes on 64-bit. If this is ever 20 or 32 the read loop silently misparses every
        // event after the first, which looks like a mad touchscreen rather than a struct.
        let expected = std::mem::size_of::<libc::timeval>() + 8;
        assert_eq!(std::mem::size_of::<InputEvent>(), expected);
    }
}
