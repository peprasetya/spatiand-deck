//! A keyboard and a mouse, for when somebody has plugged one in.
//!
//! The Deck's own controller and its touchscreen are read directly, each by the one piece of
//! code that understands that device. Everything else a person might connect — a USB keyboard,
//! a Bluetooth keyboard with a trackpad built into it, a mouse — arrives through libinput,
//! which is the library that knows what all of those are.
//!
//! Until this existed, none of them did anything at all. A Bluetooth keyboard would pair, show
//! as connected, and type into nothing.
//!
//! ## The mouse is a third pointer, not a different kind of thing
//!
//! A pad reports where a thumb is, from -1 to 1 across its surface, and that becomes a ray
//! through the head's orientation. A mouse reports how far it has moved since last time and
//! nothing about where it is, so the position has to be kept here — but once it is, it is the
//! same -1 to 1 and makes the same ray by the same function. The cursor therefore sits in
//! front of the wearer and turns with them, which is what a pointer that lives on a screen
//! does, and matches the two the pads already draw.
//!
//! It fades when it has not moved. A cursor parked in the middle of the view is in the way of
//! everything behind it, and a mouse that has not been touched for a while is one nobody is
//! using.

use std::os::unix::io::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

use libinput::Device as LibinputDevice;
use smithay::backend::libinput::LibinputSessionInterface;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::reexports::input as libinput;

/// How far a mouse must travel to cross the view, in libinput's units.
///
/// Roughly one average mousemat. The pads are absolute and a mouse is not, so this is the one
/// number that decides how a mouse *feels*, and it is deliberately generous: a headset's field
/// of view is narrow, and a pointer that crosses it in a flick cannot be aimed.
const ACROSS_THE_VIEW: f64 = 900.0;

/// How long the cursor stays fully visible after the last movement.
const LINGER: Duration = Duration::from_millis(1800);

/// How long it then takes to fade away.
const FADE: Duration = Duration::from_millis(600);

/// What a key or a button did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeskEvent {
    /// A key went down or came up, as an evdev code.
    Key { code: u32, pressed: bool },
    /// A mouse button went down or came up, as an evdev button code.
    Button { code: u32, pressed: bool },
    /// The wheel turned. Positive is away from the hand.
    Scroll { dx: f64, dy: f64 },
}

/// Every keyboard and mouse on the machine.
pub struct Desk {
    context: libinput::Libinput,
    /// Where the cursor is, in the same -1..1 the pads report.
    x: f64,
    y: f64,
    moved: Option<Instant>,
}

impl Desk {
    /// Open every input device this seat has.
    ///
    /// `None` if libinput cannot be reached at all, which is a session with no keyboard rather
    /// than no session — so it is a warning and nothing more.
    pub fn new(session: &LibSeatSession) -> Option<Desk> {
        let mut context =
            libinput::Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        if context.udev_assign_seat(&session.seat()).is_err() {
            log::warn!("no keyboard or mouse: this seat has no input devices we can open");
            return None;
        }
        log::info!("watching for keyboards and mice on seat {}", session.seat());
        Some(Desk {
            context,
            x: 0.0,
            y: 0.0,
            moved: None,
        })
    }

    /// Where the cursor is, from -1 to 1, or `None` if it has faded away.
    ///
    /// Faded rather than hidden outright, so that a cursor which is about to be wanted again
    /// does not blink back into existence — and one nobody is using stops covering whatever is
    /// behind it.
    pub fn cursor(&self) -> Option<(f32, f32, f32)> {
        let since = self.moved?.elapsed();
        let alpha = if since < LINGER {
            1.0
        } else {
            let fading = (since - LINGER).as_secs_f32() / FADE.as_secs_f32();
            1.0 - fading.clamp(0.0, 1.0)
        };
        (alpha > 0.0).then_some((self.x as f32, self.y as f32, alpha))
    }

    /// Read everything that has happened since the last frame.
    pub fn poll(&mut self) -> Vec<DeskEvent> {
        use libinput::event::keyboard::KeyboardEventTrait;
        use libinput::event::pointer::ButtonState;
        use libinput::event::{Event, EventTrait, PointerEvent};

        let mut out = Vec::new();
        if self.context.dispatch().is_err() {
            return out;
        }
        while let Some(event) = self.context.next() {
            // Devices we read ourselves were never opened -- see `OnlyWhatWeDoNotRead` -- so
            // anything arriving here is something nobody else is listening to. The check stays
            // as a belt to that brace, since the cost is one comparison.
            if let Event::Device(libinput::event::DeviceEvent::Added(added)) = &event {
                log::info!("libinput: {}", added.device().name());
            }
            if ours(&event.device()) {
                continue;
            }
            match event {
                Event::Keyboard(k) => {
                    out.push(DeskEvent::Key {
                        code: k.key(),
                        pressed: matches!(
                            k.key_state(),
                            libinput::event::keyboard::KeyState::Pressed
                        ),
                    });
                }
                Event::Pointer(PointerEvent::Motion(m)) => {
                    // Relative, so it is accumulated here. Clamped rather than wrapped: a
                    // pointer that reappears on the other side of the view is lost.
                    self.x = (self.x + m.dx() / ACROSS_THE_VIEW).clamp(-1.0, 1.0);
                    // Inverted, because the two conventions disagree. A mouse reports the way
                    // a screen is measured -- down is positive -- and the pads report the way
                    // the world is -- up is positive. Passing one straight into the other sent
                    // the cursor the wrong way vertically and the right way horizontally,
                    // which is the signature of exactly this.
                    self.y = (self.y - m.dy() / ACROSS_THE_VIEW).clamp(-1.0, 1.0);
                    self.moved = Some(Instant::now());
                }
                Event::Pointer(PointerEvent::Button(b)) => {
                    self.moved = Some(Instant::now());
                    out.push(DeskEvent::Button {
                        code: b.button(),
                        pressed: matches!(b.button_state(), ButtonState::Pressed),
                    });
                }
                Event::Pointer(PointerEvent::ScrollWheel(s)) => {
                    self.moved = Some(Instant::now());
                    out.push(scroll_of(&s));
                }
                Event::Pointer(PointerEvent::ScrollFinger(s)) => {
                    self.moved = Some(Instant::now());
                    out.push(scroll_of(&s));
                }
                Event::Pointer(PointerEvent::ScrollContinuous(s)) => {
                    self.moved = Some(Instant::now());
                    out.push(scroll_of(&s));
                }
                _ => {}
            }
        }
        out
    }
}

/// Is this a device another part of Spatiand already reads?
fn ours(device: &LibinputDevice) -> bool {
    spatiand_input::already_read_here(device.id_vendor() as u16, device.id_product() as u16)
}

/// Hands libinput every device except the ones Spatiand reads for itself.
///
/// Refusing to *open* them is the only thing that works, and it took two attempts to get here.
/// Dropping their events still left libinput holding the device open. Switching them off with
/// `send_events` was worse: libinput closes a disabled device, and a close goes back through
/// the session, which revokes the file descriptor for that device — the one our own reader was
/// still using. The touchscreen went dead the moment libinput noticed it.
///
/// So the refusal happens at the door. libinput asks the session to open a path, this says no,
/// and libinput never adds the device at all.
struct OnlyWhatWeDoNotRead {
    session: LibinputSessionInterface<LibSeatSession>,
}

impl libinput::LibinputInterface for OnlyWhatWeDoNotRead {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        if let Some((vendor, product)) = identity_of(path) {
            if spatiand_input::already_read_here(vendor, product) {
                log::info!(
                    "libinput: not opening {}; spatiand reads it directly",
                    path.display()
                );
                // Not an error anyone should act on -- it is a decision. libinput treats it as
                // a device it cannot have and moves on.
                return Err(libc::ENODEV);
            }
        }
        self.session.open_restricted(path, flags)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        self.session.close_restricted(fd)
    }
}

/// The vendor and product of the device behind an input node.
///
/// Read from sysfs, because the path is all libinput gives us and the decision has to be made
/// before the device is open. `/dev/input/event8` becomes
/// `/sys/class/input/event8/device/id/{vendor,product}`, which are four hex digits each.
fn identity_of(path: &Path) -> Option<(u16, u16)> {
    let node = path.file_name()?.to_str()?;
    let base = format!("/sys/class/input/{node}/device/id");
    let read = |what: &str| {
        std::fs::read_to_string(format!("{base}/{what}"))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
    };
    Some((read("vendor")?, read("product")?))
}

fn scroll_of<E: libinput::event::pointer::PointerScrollEvent>(event: &E) -> DeskEvent {
    use libinput::event::pointer::Axis;
    DeskEvent::Scroll {
        dx: event.scroll_value(Axis::Horizontal),
        dy: event.scroll_value(Axis::Vertical),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mouse_that_has_never_moved_has_no_cursor() {
        // Nothing should be drawn in front of somebody who has not touched a mouse, and on
        // this machine most sessions never will.
        let desk = Desk {
            context: unsafe { std::mem::zeroed() },
            x: 0.0,
            y: 0.0,
            moved: None,
        };
        assert_eq!(desk.cursor(), None);
        // Never dropped: the zeroed context is not a real one.
        std::mem::forget(desk);
    }

    #[test]
    fn the_cursor_is_solid_then_fades_then_goes() {
        let at = |ago: Duration| {
            let desk = Desk {
                context: unsafe { std::mem::zeroed() },
                x: 0.25,
                y: -0.5,
                moved: Some(Instant::now() - ago),
            };
            let seen = desk.cursor();
            std::mem::forget(desk);
            seen
        };
        assert_eq!(at(Duration::from_millis(0)).map(|c| c.2), Some(1.0));
        let midway = at(LINGER + FADE / 2).expect("still on its way out");
        assert!(midway.2 > 0.2 && midway.2 < 0.8, "alpha was {}", midway.2);
        assert_eq!(at(LINGER + FADE + Duration::from_millis(50)), None);
        // And it is where it was left, whatever it is doing.
        assert_eq!(at(Duration::ZERO).map(|c| (c.0, c.1)), Some((0.25, -0.5)));
    }
}
