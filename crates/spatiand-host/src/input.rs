//! Delivering what the wearer did to the window they did it to.
//!
//! The host is a compositor, so this is not injection: there is no virtual keyboard typing at
//! whatever happens to be focused, and no guessing which window a click belongs to. Each event
//! arrives naming its window and is delivered to that window's surface, exactly as a local
//! input device's event would be.
//!
//! That is the part a screen-capture streamer cannot do, and it is why the pointer lands in
//! the right place even when the session has moved the window somewhere else in the room: the
//! coordinates are the window's own.

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::utils::{Point, SERIAL_COUNTER};
use spatiand_stream::{Input, WindowId};

use crate::state::Host;

/// Let go of everything a session left held, as it goes.
///
/// A link that drops mid-click leaves the application holding a button, and mid-keystroke
/// holding a key — which for an X11 client means it repeats until something releases it.
/// Nothing else will, so this does.
pub fn release_everything(host: &mut Host, time_ms: u32) {
    let held = std::mem::take(&mut host.held_buttons);
    if let Some(pointer) = host.seat.get_pointer() {
        for button in held {
            pointer.button(
                host,
                &ButtonEvent {
                    button,
                    state: ButtonState::Released,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_ms,
                },
            );
        }
        pointer.frame(host);
    }
    // Keyboard focus going away is what tells a client its keys are no longer held; XWayland
    // turns it into the release an X11 client is waiting for.
    if let Some(keyboard) = host.seat.get_keyboard() {
        keyboard.set_focus(host, None, SERIAL_COUNTER.next_serial());
    }
}

/// The session moved its keyboard: to one of the windows here, or off all of them.
///
/// Acted on when it is said, not when the first key arrives: an application that has not yet
/// become the active window ignores a shortcut, so a first key that was Ctrl+Shift+V pasted
/// nothing. Off all of them is a real state too -- the wearer is typing somewhere else -- and
/// a window here that still believed itself active would draw a caret nobody is typing into.
pub fn focus(host: &mut Host, window: Option<WindowId>) {
    let target = window.and_then(|w| {
        host.windows
            .iter()
            .find(|t| t.id == w)
            .map(|t| t.window.clone())
    });
    match target {
        Some(target) => {
            if let Some(surface) = crate::state::surface_of(&target) {
                focus_keyboard(host, &target, surface);
            }
        }
        None => {
            let Some(keyboard) = host.seat.get_keyboard() else {
                return;
            };
            if let Some(crate::state::KeyboardFocus::X11(old)) = keyboard.current_focus() {
                let _ = old.set_activated(false);
            }
            keyboard.set_focus(host, None, SERIAL_COUNTER.next_serial());
        }
    }
}

/// Apply one event to one window.
pub fn apply(host: &mut Host, window: WindowId, input: Input, time_ms: u32) {
    let Some(target) = host.windows.iter().find(|t| t.id == window).map(|t| t.window.clone())
    else {
        return;
    };
    let Some(surface) = crate::state::surface_of(&target) else {
        return;
    };

    match input {
        Input::Motion { x, y } => {
            let Some(pointer) = host.seat.get_pointer() else {
                return;
            };
            // The session's coordinates are the picture's, which starts at the window's
            // geometry — inside any shadow the client draws. The surface's own coordinates
            // start at the shadow's edge.
            let origin = target.geometry().loc;
            let location =
                Point::<f64, smithay::utils::Logical>::from((x + origin.x as f64, y + origin.y as f64));
            // Whatever is under the point, menus first: a click on an open context menu is
            // for the menu, not for the page underneath it.
            let focus = target
                .surface_under(location, smithay::desktop::WindowSurfaceType::ALL)
                .map(|(s, at)| (s, at.to_f64()))
                .unwrap_or((surface, (0.0, 0.0).into()));
            // How far it moved, for relative motion: the whole of what a locked pointer is
            // told, and alongside every absolute move otherwise, as a real mouse gives both.
            let delta = host
                .pointer_last
                .as_ref()
                .map(|(_, _, last)| location - *last)
                .unwrap_or_default();
            // Locked -- see `PointerConstraintsHandler` in state.rs -- the pointer stays where
            // it is and only the movement is passed on.
            let locked = host.pointer_last.as_ref().is_some_and(|(held, _, _)| {
                pointer.current_focus().as_ref() == Some(held)
                    && smithay::wayland::pointer_constraints::with_pointer_constraint(
                        held,
                        &pointer,
                        |constraint| {
                            constraint.is_some_and(|c| {
                                c.is_active()
                                    && matches!(
                                        &*c,
                                        smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)
                                    )
                            })
                        },
                    )
            });
            {
                use std::sync::atomic::{AtomicBool, Ordering};
                static WAS_LOCKED: AtomicBool = AtomicBool::new(false);
                if WAS_LOCKED.swap(locked, Ordering::Relaxed) != locked {
                    log::info!("pointer: {}", if locked { "locked; passing movement on as relative only" } else { "unlocked" });
                }
            }
            if locked {
                let (held, origin, _) = host.pointer_last.clone().expect("locked implies a last");
                pointer.relative_motion(
                    host,
                    Some((held.clone(), origin)),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        utime: time_ms as u64 * 1000,
                    },
                );
                pointer.frame(host);
                host.pointer_last = Some((held, origin, location));
                return;
            }
            pointer.motion(
                host,
                Some(focus.clone()),
                &MotionEvent {
                    location,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_ms,
                },
            );
            if delta != Point::default() {
                pointer.relative_motion(
                    host,
                    Some(focus.clone()),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        utime: time_ms as u64 * 1000,
                    },
                );
            }
            pointer.frame(host);
            // A lock asked for before the pointer arrived is granted now it has.
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |constraint| {
                    if let Some(constraint) = constraint {
                        if !constraint.is_active() {
                            constraint.activate();
                        }
                    }
                },
            );
            host.pointer_last = Some((focus.0, focus.1, location));
        }
        Input::Leave => {
            // Coming back in is a new position, not a movement from where it left.
            host.pointer_last = None;
            let Some(pointer) = host.seat.get_pointer() else {
                return;
            };
            // Focus of `None` is how a pointer leaves; the location goes with it and is not
            // read by anything once there is nothing under it.
            pointer.motion(
                host,
                None,
                &MotionEvent {
                    location: (0.0, 0.0).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_ms,
                },
            );
            pointer.frame(host);
        }
        Input::Button { button, pressed } => {
            let Some(pointer) = host.seat.get_pointer() else {
                return;
            };
            // A press anywhere but on a menu closes the menus, as it does on a desktop. The
            // host takes no popup grabs, so nothing else would ever tell the client its menu
            // is over.
            log::debug!(
                "window {}: button {button:#x} {}",
                window.0,
                if pressed { "pressed" } else { "released" }
            );
            if pressed {
                // Up to the surface the menu *is*: a menu's items can be drawn on a
                // subsurface of it, and a press on one of those is still a press on the menu.
                let on_menu = pointer.current_focus().is_some_and(|mut focused| {
                    while let Some(parent) = smithay::wayland::compositor::get_parent(&focused) {
                        focused = parent;
                    }
                    host.popups.find_popup(&focused).is_some()
                });
                log::debug!(
                    "window {}: press {} a menu",
                    window.0,
                    if on_menu { "on" } else { "outside" }
                );
                if !on_menu {
                    let open: Vec<_> = smithay::desktop::PopupManager::popups_for_surface(&surface)
                        .map(|(popup, _)| popup)
                        .collect();
                    for popup in open.iter().rev() {
                        let _ = smithay::desktop::PopupManager::dismiss_popup(&surface, popup);
                    }
                }
            }
            if pressed {
                if !host.held_buttons.contains(&button) {
                    host.held_buttons.push(button);
                }
            } else {
                host.held_buttons.retain(|held| *held != button);
            }
            pointer.button(
                host,
                &ButtonEvent {
                    button,
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    serial: SERIAL_COUNTER.next_serial(),
                    time: time_ms,
                },
            );
            pointer.frame(host);
        }
        Input::Scroll {
            horizontal,
            vertical,
        } => {
            let Some(pointer) = host.seat.get_pointer() else {
                return;
            };
            let mut frame = AxisFrame::new(time_ms).source(AxisSource::Wheel);
            if horizontal != 0.0 {
                frame = frame.value(Axis::Horizontal, horizontal);
            }
            if vertical != 0.0 {
                frame = frame.value(Axis::Vertical, vertical);
            }
            pointer.axis(host, frame);
            pointer.frame(host);
        }
        Input::Key { code, pressed } => {
            let Some(keyboard) = host.seat.get_keyboard() else {
                return;
            };
            // Focus follows the keys as well as `ClientMessage::Focus`: a key is never meant
            // for any other window, whatever the session has or has not said.
            focus_keyboard(host, &target, surface);
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(
                host,
                // The wire carries evdev codes; smithay's keyboard takes XKB codes, which are
                // eight above them for reasons inherited from X11 (its own libinput backend
                // adds the same 8). Subtracting instead shifted every key two rows of the
                // table down: P arrived as 8.
                smithay::input::keyboard::Keycode::from(code + 8),
                if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
                serial,
                time_ms,
                |_, _, _| FilterResult::Forward,
            );
        }
    }
}

/// Give a window the keyboard, if it does not have it already.
///
/// The window the session names is the window the wearer is using. An X11 window is focused as
/// itself, raised and activated; see `state::KeyboardFocus`.
fn focus_keyboard(
    host: &mut Host,
    target: &smithay::desktop::Window,
    surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) {
    let Some(keyboard) = host.seat.get_keyboard() else {
        return;
    };
    let wanted = match target.x11_surface() {
        Some(x11) => crate::state::KeyboardFocus::X11(x11.clone()),
        None => crate::state::KeyboardFocus::Wayland(surface),
    };
    if keyboard.current_focus().as_ref() == Some(&wanted) {
        return;
    }
    if let Some(crate::state::KeyboardFocus::X11(old)) = keyboard.current_focus() {
        let _ = old.set_activated(false);
    }
    if let crate::state::KeyboardFocus::X11(x11) = &wanted {
        if let Some(wm) = host.xwm.as_mut() {
            let _ = wm.raise_window(x11);
        }
        let _ = x11.set_activated(true);
    }
    keyboard.set_focus(host, Some(wanted), SERIAL_COUNTER.next_serial());
}
