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
            assert_eq!(intent_for(c), None, "{} should not be a menu intent", c.name());
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
