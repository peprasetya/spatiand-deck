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

use std::time::{Duration, Instant};

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
        // Y closes the selected window in the window list, and does nothing anywhere else --
        // in the world it belongs to the application, like every other button.
        Control::Y => Intent::Close,
        Control::Steam => Intent::ToggleHud,
        Control::Quick => Intent::ToggleLauncher,
        // The right pad's click is the pointer's select, handled by the pointer rather than by
        // the menu state machine, so it is deliberately not an intent.
        _ => return None,
    })
}

/// How long a direction has to be held in a menu before it starts repeating.
///
/// Long enough that one deliberate press never moves two rows -- the same wait the volume
/// rocker uses, so every held button on the machine starts at the same moment.
const REPEAT_DELAY: Duration = Duration::from_millis(400);

/// How often a held direction moves once it is repeating: ten rows a second, a long list
/// crossed quickly, and slow enough to let go on the row wanted.
const REPEAT_EVERY: Duration = Duration::from_millis(100);

/// The D-pad in the menus, held down, going on the way a held arrow key does.
///
/// The first step is the press itself, handled with every other press. This only adds the
/// ones after it, while the same direction stays down. Only in the menus: in the world the
/// D-pad belongs to the focused application, which repeats -- or does not -- for itself.
#[derive(Debug, Default)]
pub struct NavRepeat {
    held: Option<(Control, Instant)>,
}

impl NavRepeat {
    /// Once a frame, with the direction held now, if any. Returns the step due, if one is.
    ///
    /// At most one per call, however long the frame was, so a stall never turns into a jump.
    pub fn tick(&mut self, held: Option<Control>, now: Instant) -> Option<Intent> {
        let Some(control) = held else {
            self.held = None;
            return None;
        };
        match self.held {
            // A new direction, or the first frame of one: its press has just been handled.
            Some((was, _)) if was != control => self.held = Some((control, now + REPEAT_DELAY)),
            None => self.held = Some((control, now + REPEAT_DELAY)),
            Some((_, next)) if now < next => {}
            Some((_, next)) => {
                // Keep the cadence when on time; start afresh from now after a long frame.
                let following = if next + REPEAT_EVERY > now {
                    next + REPEAT_EVERY
                } else {
                    now + REPEAT_EVERY
                };
                self.held = Some((control, following));
                return intent_for(control);
            }
        }
        None
    }
}

/// The D-pad direction held in this state, if any. One at a time: with two down (a thumb
/// rolling across the pad) the first found goes on, which is what a keyboard does.
pub fn held_direction(buttons: spatiand_input::Buttons) -> Option<Control> {
    [Control::Up, Control::Down, Control::Left, Control::Right]
        .into_iter()
        .find(|c| buttons.is_down(*c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_press_moves_once() {
        let t = Instant::now();
        let mut r = NavRepeat::default();
        assert_eq!(r.tick(Some(Control::Down), t), None, "the press itself is handled elsewhere");
        assert_eq!(r.tick(Some(Control::Down), t + Duration::from_millis(300)), None);
        assert_eq!(r.tick(None, t + Duration::from_millis(350)), None);
        assert_eq!(r.tick(None, t + Duration::from_millis(900)), None);
    }

    #[test]
    fn holding_goes_on_after_the_delay_and_then_steadily() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut r = NavRepeat::default();
        let down = Some(Intent::Navigate(NavDirection::Down));
        assert_eq!(r.tick(Some(Control::Down), t), None);
        assert_eq!(r.tick(Some(Control::Down), ms(399)), None);
        assert_eq!(r.tick(Some(Control::Down), ms(400)), down);
        assert_eq!(r.tick(Some(Control::Down), ms(450)), None);
        assert_eq!(r.tick(Some(Control::Down), ms(500)), down);
        assert_eq!(r.tick(Some(Control::Down), ms(600)), down);
        // A frame that ran long pays out one step, not three.
        assert_eq!(r.tick(Some(Control::Down), ms(950)), down);
        assert_eq!(r.tick(Some(Control::Down), ms(960)), None);
    }

    #[test]
    fn changing_direction_waits_again() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut r = NavRepeat::default();
        r.tick(Some(Control::Down), t);
        assert!(r.tick(Some(Control::Down), ms(400)).is_some());
        assert_eq!(r.tick(Some(Control::Right), ms(450)), None);
        assert_eq!(r.tick(Some(Control::Right), ms(800)), None);
        assert_eq!(
            r.tick(Some(Control::Right), ms(850)),
            Some(Intent::Navigate(NavDirection::Right))
        );
    }

    #[test]
    fn the_two_menu_buttons_are_bound_the_way_they_are_labelled() {
        assert_eq!(intent_for(Control::Steam), Some(Intent::ToggleHud));
        assert_eq!(intent_for(Control::Quick), Some(Intent::ToggleLauncher));
    }

    #[test]
    fn the_shoulders_and_the_paddles_belong_to_whatever_is_running() {
        // Six buttons Spatiand does not touch, and the rule is worth stating as a rule: every
        // control this session claims is one an application or a game cannot have.
        //
        // The shoulders used to step focus between windows and the switcher briefly lived on a
        // paddle. Both were the same mistake at different sizes. Cycling windows is now
        // reached from the HUD, where it costs no buttons at all, and these six are bound to
        // nothing in either direction -- no intent, and no key typed into the focused
        // application either -- so the coming per-application mapping has them to give away.
        for control in [
            Control::L1,
            Control::R1,
            Control::L4,
            Control::R4,
            Control::L5,
            Control::R5,
        ] {
            assert_eq!(intent_for(control), None, "{control:?} is claimed again");
        }
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
            Intent::Close,
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
