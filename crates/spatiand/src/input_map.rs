//! Which physical control means what.
//!
//! The one place a Steam Deck button becomes an abstract intent. Keeping it to a single
//! function is what lets `spatiand-shell` stay free of Valve-specific notions, and what makes
//! remapping — or adding a keyboard, or a different headset's temple buttons — a change here
//! rather than a change everywhere.
//!
//! Two bindings are load-bearing and worth stating plainly:
//!
//! * **STEAM opens the HUD, `⋯` opens the launcher.** That is the arrangement the whole design
//!   was asked for, and it matches what both buttons do in Game Mode closely enough that the
//!   muscle memory carries over.
//! * **B never leaves the session.** The way out is an explicit row in the HUD. B is the
//!   easiest button on the device to press by accident, and losing every open window to it
//!   would be unrecoverable.

use spatiand_input::Control;
use spatiand_shell::{Intent, NavDirection};

/// Map a control to an intent, or `None` if it means nothing to the shell.
pub fn intent_for(control: Control) -> Option<Intent> {
    Some(match control {
        Control::Up => Intent::Navigate(NavDirection::Up),
        Control::Down => Intent::Navigate(NavDirection::Down),
        Control::Left => Intent::Navigate(NavDirection::Left),
        Control::Right => Intent::Navigate(NavDirection::Right),
        Control::A => Intent::Accept,
        Control::B => Intent::Back,
        Control::Steam => Intent::ToggleHud,
        Control::Quick => Intent::ToggleLauncher,
        // The right pad's click is the pointer's select, handled by the pointer rather than by
        // the menu state machine, so it is deliberately not an intent.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_menu_buttons_are_bound_the_way_they_are_labelled() {
        assert_eq!(intent_for(Control::Steam), Some(Intent::ToggleHud));
        assert_eq!(intent_for(Control::Quick), Some(Intent::ToggleLauncher));
    }

    #[test]
    fn the_dpad_maps_to_the_matching_direction() {
        // A transposition here is invisible in review and instantly obvious in a headset.
        assert_eq!(
            intent_for(Control::Up),
            Some(Intent::Navigate(NavDirection::Up))
        );
        assert_eq!(
            intent_for(Control::Down),
            Some(Intent::Navigate(NavDirection::Down))
        );
        assert_eq!(
            intent_for(Control::Left),
            Some(Intent::Navigate(NavDirection::Left))
        );
        assert_eq!(
            intent_for(Control::Right),
            Some(Intent::Navigate(NavDirection::Right))
        );
    }

    #[test]
    fn a_is_accept_and_b_is_back() {
        assert_eq!(intent_for(Control::A), Some(Intent::Accept));
        assert_eq!(intent_for(Control::B), Some(Intent::Back));
    }

    #[test]
    fn the_pointer_controls_are_not_menu_intents() {
        // Pad contact and clicks drive the laser; routing them through the menu state machine
        // as well would make every click do two things at once.
        for c in [
            Control::RPadClick,
            Control::LPadClick,
            Control::RPadTouch,
            Control::LPadTouch,
        ] {
            assert_eq!(
                intent_for(c),
                None,
                "{} should not be a menu intent",
                c.name()
            );
        }
    }

    #[test]
    fn no_control_silently_quits_the_session() {
        // Exhaustive: nothing in the map may produce a "leave" on its own. The way out is a
        // HUD row you have to navigate to and confirm.
        for c in Control::ALL {
            match intent_for(c) {
                Some(Intent::Back) => assert_eq!(c, Control::B),
                Some(_) | None => {}
            }
        }
    }

    #[test]
    fn every_intent_is_reachable_from_some_button() {
        // A shell state you cannot get into is dead code that still costs a code path.
        let all: Vec<Intent> = Control::ALL.into_iter().filter_map(intent_for).collect();
        for wanted in [
            Intent::Accept,
            Intent::Back,
            Intent::ToggleHud,
            Intent::ToggleLauncher,
            Intent::Navigate(NavDirection::Up),
            Intent::Navigate(NavDirection::Down),
            Intent::Navigate(NavDirection::Left),
            Intent::Navigate(NavDirection::Right),
        ] {
            assert!(all.contains(&wanted), "{wanted:?} is unreachable");
        }
    }
}

/// Which key a control types into the focused application.
///
/// Evdev codes, matching `spatiand_shell::keyboard`, so the two tables can be read against the
/// same kernel header.
///
/// This is the fixed half of something that should eventually be configurable. A game wants
/// every control remappable, per application, and forwarded without the application ever
/// knowing a gamepad was involved — which is what Game Mode does and what this will have to
/// become. Until then the mapping is the one that makes a remote-control-shaped application
/// usable from the sofa: the D-pad is the arrow keys, A confirms, B goes back.
///
/// Deliberately **not** every button. A control that means something to the shell keeps
/// meaning that, because losing the way out of a full-screen application is much worse than
/// not being able to type one more key into it.
pub fn key_for(control: Control) -> Option<u32> {
    // From `linux/input-event-codes.h`.
    const KEY_ESC: u32 = 1;
    const KEY_ENTER: u32 = 28;
    const KEY_UP: u32 = 103;
    const KEY_LEFT: u32 = 105;
    const KEY_RIGHT: u32 = 106;
    const KEY_DOWN: u32 = 108;
    const KEY_BACKSPACE: u32 = 14;
    const KEY_TAB: u32 = 15;

    Some(match control {
        Control::Up => KEY_UP,
        Control::Down => KEY_DOWN,
        Control::Left => KEY_LEFT,
        Control::Right => KEY_RIGHT,
        Control::A => KEY_ENTER,
        Control::B => KEY_ESC,
        // X and Y are the two that a remote-shaped application most often wants next, and
        // these are the two keys such an application most often binds.
        Control::X => KEY_BACKSPACE,
        Control::Y => KEY_TAB,
        _ => return None,
    })
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn the_d_pad_is_the_arrow_keys() {
        assert_eq!(key_for(Control::Up), Some(103));
        assert_eq!(key_for(Control::Down), Some(108));
        assert_eq!(key_for(Control::Left), Some(105));
        assert_eq!(key_for(Control::Right), Some(106));
    }

    #[test]
    fn a_confirms_and_b_goes_back() {
        assert_eq!(key_for(Control::A), Some(28));
        assert_eq!(key_for(Control::B), Some(1));
    }

    #[test]
    fn the_way_out_of_the_session_is_never_typed_into_an_application() {
        // STEAM and the QAM button open the shell's own surfaces, and the bumpers move
        // between windows. If any of them also typed, a full-screen application could take
        // the only way out of itself.
        for control in [
            Control::Steam,
            Control::Quick,
            Control::L1,
            Control::R1,
            Control::L4,
            Control::R4,
            Control::L5,
            Control::R5,
        ] {
            assert_eq!(key_for(control), None, "{control:?} would be typed");
        }
    }

    #[test]
    fn no_two_controls_type_the_same_key() {
        // A duplicate would be silent: both buttons would work, and one of them would be
        // doing something nobody meant it to.
        let mut seen = std::collections::HashMap::new();
        for control in Control::ALL {
            if let Some(code) = key_for(control) {
                if let Some(other) = seen.insert(code, control) {
                    panic!("{control:?} and {other:?} both type {code}");
                }
            }
        }
    }
}
