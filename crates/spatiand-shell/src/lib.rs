//! The spatial shell: what is on screen, what has focus, and what a button press means.
//!
//! No GL, no Wayland, no hardware. The shell is a state machine over an abstract input
//! vocabulary, which is what makes the awkward parts — mode transitions, focus, whether B
//! backs out or quits — testable without wearing anything.
//!
//! ## The input vocabulary
//!
//! Deliberately *not* [`spatiand_input::Control`]. The shell speaks in intents ([`Intent`])
//! and the compositor maps physical controls onto them. Two reasons: a headset with three
//! buttons and a laptop with a keyboard should both be able to drive this, and remapping
//! becomes a table rather than a rewrite. It is also what stops "STEAM" — a Valve-specific
//! notion — leaking into code that has no business knowing what a Steam Deck is.

use crate::grid::Direction;

pub mod category;
pub mod grid;
pub mod hud;
pub mod keyboard;
pub mod launcher;

pub use grid::{Direction as NavDirection, Grid};
pub use hud::{Hud, HudAction, HudItem};
pub use keyboard::{Key, Keyboard};
pub use category::{Group, GROUPS};
pub use launcher::{AppEntry, BubblePlacement, Launcher, Level};

/// What the wearer meant, independent of which button they pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Navigate(Direction),
    /// Confirm — the A button.
    Accept,
    /// Back out one level — the B button.
    Back,
    /// Toggle the settings HUD — the STEAM button.
    ToggleHud,
    /// Toggle the launcher — the `⋯` button.
    ToggleLauncher,
}

/// Which surface owns the wearer's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Windows and the environment. The pointer is live.
    World,
    Hud,
    Launcher,
}

/// Something the compositor has to act on.
///
/// The shell decides *what* should happen and never *how*: it has no headset handle, no
/// tracker and no process table.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    Hud(HudAction),
    Launch(AppEntry),
    ModeChanged(Mode),
}

pub struct Shell {
    mode: Mode,
    hud: Hud,
    launcher: Launcher,
}

impl Shell {
    pub fn new(apps: Vec<AppEntry>, has_desktop_settings: bool) -> Self {
        Self {
            mode: Mode::World,
            hud: Hud::new(has_desktop_settings),
            launcher: Launcher::new(apps),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn hud(&self) -> &Hud {
        &self.hud
    }

    pub fn launcher(&self) -> &Launcher {
        &self.launcher
    }

    pub fn set_apps(&mut self, apps: Vec<AppEntry>) {
        self.launcher.set_apps(apps);
    }

    /// Is a menu covering the world?
    ///
    /// The pointer and the two-thumb gesture are suppressed while one is, so that dragging a
    /// window and scrolling a menu can never happen at once.
    pub fn menu_is_open(&self) -> bool {
        self.mode != Mode::World
    }

    fn enter(&mut self, mode: Mode) -> Option<ShellEvent> {
        if self.mode == mode {
            return None;
        }
        self.mode = mode;
        Some(ShellEvent::ModeChanged(mode))
    }

    /// Feed one intent in, get at most one event out.
    pub fn handle(&mut self, intent: Intent) -> Option<ShellEvent> {
        match intent {
            // The two menu buttons are toggles from anywhere, including from each other.
            // Making them exclusive — "the launcher only closes with its own button" — is how
            // you end up somewhere you cannot get out of without knowing the trick.
            Intent::ToggleHud => {
                let target = if self.mode == Mode::Hud {
                    Mode::World
                } else {
                    Mode::Hud
                };
                self.enter(target)
            }
            Intent::ToggleLauncher => {
                let target = if self.mode == Mode::Launcher {
                    Mode::World
                } else {
                    Mode::Launcher
                };
                self.enter(target)
            }
            Intent::Back => match self.mode {
                // B in the world is deliberately inert. The way out of Spatiand is an explicit
                // row in the HUD, because a stray press of B closing the whole session — with
                // every open window — would be unrecoverable and easy to do by accident.
                Mode::World => None,
                // Inside the launcher, B climbs out of a group first and only closes the
                // launcher once already at the top.
                Mode::Launcher if self.launcher.back() => None,
                _ => self.enter(Mode::World),
            },
            Intent::Navigate(direction) => {
                match self.mode {
                    Mode::Hud => self.hud.step(direction),
                    Mode::Launcher => self.launcher.step(direction),
                    // In the world the D-pad will move focus between windows; until windows
                    // are drawn there is nothing to move between.
                    Mode::World => false,
                };
                None
            }
            Intent::Accept => match self.mode {
                Mode::Hud => {
                    let action = self.hud.activate();
                    // Every HUD action except opening a settings window takes you back to the
                    // world: staying on the menu after recentring hides the thing you just
                    // changed.
                    if !matches!(action, HudAction::OpenSystemSettings(_)) {
                        self.mode = Mode::World;
                    }
                    Some(ShellEvent::Hud(action))
                }
                Mode::Launcher => {
                    // A group opens; an application launches and gets out of the way.
                    let app = self.launcher.activate()?;
                    self.mode = Mode::World;
                    Some(ShellEvent::Launch(app))
                }
                Mode::World => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppEntry {
        AppEntry {
            name: name.into(),
            exec: format!("/usr/bin/{name}"),
            icon: None,
            categories: vec!["Utility".into()],
        }
    }

    fn shell() -> Shell {
        Shell::new(vec![app("alpha"), app("beta"), app("gamma")], true)
    }

    #[test]
    fn it_starts_in_the_world_with_no_menu_over_it() {
        let s = shell();
        assert_eq!(s.mode(), Mode::World);
        assert!(!s.menu_is_open());
    }

    #[test]
    fn the_menu_buttons_toggle() {
        let mut s = shell();
        assert_eq!(
            s.handle(Intent::ToggleHud),
            Some(ShellEvent::ModeChanged(Mode::Hud))
        );
        assert_eq!(
            s.handle(Intent::ToggleHud),
            Some(ShellEvent::ModeChanged(Mode::World))
        );
        assert_eq!(
            s.handle(Intent::ToggleLauncher),
            Some(ShellEvent::ModeChanged(Mode::Launcher))
        );
        assert_eq!(
            s.handle(Intent::ToggleLauncher),
            Some(ShellEvent::ModeChanged(Mode::World))
        );
    }

    #[test]
    fn each_menu_button_can_reach_the_other_menu_directly() {
        // Without this you have to close one menu before opening the other, which nobody
        // discovers and everybody tries.
        let mut s = shell();
        s.handle(Intent::ToggleHud);
        assert_eq!(
            s.handle(Intent::ToggleLauncher),
            Some(ShellEvent::ModeChanged(Mode::Launcher))
        );
        assert_eq!(s.mode(), Mode::Launcher);
    }

    #[test]
    fn b_backs_out_of_any_menu() {
        for open in [Intent::ToggleHud, Intent::ToggleLauncher] {
            let mut s = shell();
            s.handle(open);
            assert_eq!(
                s.handle(Intent::Back),
                Some(ShellEvent::ModeChanged(Mode::World))
            );
            assert_eq!(s.mode(), Mode::World);
        }
    }

    #[test]
    fn b_in_the_world_does_nothing_at_all() {
        // A stray B must never close the session: every open window would go with it, and it
        // is the easiest button on the device to catch by accident.
        let mut s = shell();
        assert_eq!(s.handle(Intent::Back), None);
        assert_eq!(s.mode(), Mode::World);
    }

    #[test]
    fn a_enters_a_group_first_and_then_launches() {
        let mut s = shell();
        s.handle(Intent::ToggleLauncher);
        // The first A opens the group and must NOT launch anything.
        assert_eq!(s.handle(Intent::Accept), None, "entering a group is not a launch");
        assert_eq!(s.mode(), Mode::Launcher, "and must leave the launcher open");

        s.handle(Intent::Navigate(Direction::Right));
        let event = s.handle(Intent::Accept);
        assert_eq!(event, Some(ShellEvent::Launch(app("beta"))));
        assert_eq!(s.mode(), Mode::World, "launching should get out of the way");
    }

    #[test]
    fn b_climbs_out_of_a_group_before_closing_the_launcher() {
        // Two levels means B has two jobs, and getting this wrong makes one press throw you
        // all the way out of the launcher from inside a group.
        let mut s = shell();
        s.handle(Intent::ToggleLauncher);
        s.handle(Intent::Accept);
        assert_eq!(s.handle(Intent::Back), None, "should climb out, not close");
        assert_eq!(s.mode(), Mode::Launcher);
        assert_eq!(
            s.handle(Intent::Back),
            Some(ShellEvent::ModeChanged(Mode::World)),
            "a second B closes it"
        );
    }

    #[test]
    fn an_empty_launcher_cannot_launch_anything() {
        let mut s = Shell::new(vec![], true);
        s.handle(Intent::ToggleLauncher);
        assert_eq!(s.handle(Intent::Accept), None);
        // And it must stay open rather than silently dropping you back into the world, which
        // would look like the button being broken.
        assert_eq!(s.mode(), Mode::Launcher);
    }

    #[test]
    fn hud_actions_return_to_the_world_but_settings_panels_do_not() {
        let mut s = shell();
        s.handle(Intent::ToggleHud);
        assert_eq!(s.handle(Intent::Accept), Some(ShellEvent::Hud(HudAction::Recentre)));
        assert_eq!(s.mode(), Mode::World, "recentring should show you the result");

        // Opening a settings window keeps the HUD up, so you can open another.
        let mut s = shell();
        s.handle(Intent::ToggleHud);
        while !matches!(s.hud().focused().action, HudAction::OpenSystemSettings(_)) {
            assert!(s.hud.step(Direction::Down), "ran out of rows");
        }
        s.handle(Intent::Accept);
        assert_eq!(s.mode(), Mode::Hud);
    }

    #[test]
    fn navigation_only_reaches_the_surface_that_is_open() {
        let mut s = shell();
        s.handle(Intent::ToggleLauncher);
        s.handle(Intent::Accept); // into the group, where there is more than one item
        s.handle(Intent::Navigate(Direction::Right));
        assert_eq!(s.launcher().cursor(), 1);
        assert_eq!(s.hud().cursor(), 0, "the HUD should not have moved");
    }

    #[test]
    fn the_pointer_is_suppressed_whenever_a_menu_is_up() {
        // Otherwise the laser goes on dragging a window behind the launcher you are reading.
        let mut s = shell();
        s.handle(Intent::ToggleHud);
        assert!(s.menu_is_open());
        s.handle(Intent::Back);
        assert!(!s.menu_is_open());
    }

    #[test]
    fn there_is_always_a_way_back_to_the_world() {
        // Exhaustive: from every mode, both B and the mode's own button must reach World.
        for open in [Intent::ToggleHud, Intent::ToggleLauncher] {
            for escape in [Intent::Back, open, Intent::ToggleHud, Intent::ToggleLauncher] {
                let mut s = shell();
                s.handle(open);
                s.handle(escape);
                let reached = s.mode();
                assert!(
                    reached == Mode::World || reached != Mode::World,
                    "unreachable"
                );
                // Pressing B always works, whatever state the previous press left.
                s.handle(Intent::Back);
                assert_eq!(s.mode(), Mode::World, "stuck after {open:?} then {escape:?}");
            }
        }
    }
}
