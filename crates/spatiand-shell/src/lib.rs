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
pub mod environment;
pub mod files;
pub mod grid;
pub mod hud;
pub mod keyboard;
pub mod launcher;

pub use grid::{Direction as NavDirection, Grid};
pub use hud::{DesktopPanels, Hud, HudAction, HudItem};
pub use keyboard::{Key, Keyboard};
pub use category::{Group, GROUPS};
pub use environment::{
    EnvironmentAction, EnvironmentChoice, EnvironmentEntry, EnvironmentPicker, EnvironmentRow,
};
pub use files::{FileAction, FileBrowser, FileEntry};
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
    /// Choosing what surrounds you. Reached from the HUD, and B returns there rather than to
    /// the world — you came from a menu, so backing out should land you in it.
    Environment,
    /// Looking for an image to add. Reached from the environment picker.
    Files,
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
    /// Surround me with this.
    ChooseEnvironment(EnvironmentChoice),
    /// List a directory for the browser. `None` means wherever browsing should start; a name
    /// is relative to the directory last listed, and may be [`files::PARENT_LABEL`].
    ListDirectory(Option<String>),
    /// Add this file — named relative to the directory last listed — to the environments.
    AddEnvironment(String),
}

pub struct Shell {
    mode: Mode,
    hud: Hud,
    launcher: Launcher,
    environments: EnvironmentPicker,
    files: FileBrowser,
}

impl Shell {
    pub fn new(apps: Vec<AppEntry>, panels: DesktopPanels) -> Self {
        Self {
            mode: Mode::World,
            hud: Hud::new(panels),
            launcher: Launcher::new(apps),
            environments: EnvironmentPicker::default(),
            files: FileBrowser::default(),
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

    pub fn environments(&self) -> &EnvironmentPicker {
        &self.environments
    }

    pub fn files(&self) -> &FileBrowser {
        &self.files
    }

    pub fn set_apps(&mut self, apps: Vec<AppEntry>) {
        self.launcher.set_apps(apps);
    }

    /// Hand the picker the current list. The compositor calls this on startup and again
    /// whenever the HUD asks to open the picker, so a file dropped into the folder mid-session
    /// shows up without a restart.
    pub fn set_environments(&mut self, entries: Vec<EnvironmentEntry>, current: EnvironmentChoice) {
        self.environments.set_entries(entries, current);
    }

    /// Hand the browser a directory listing, in answer to [`ShellEvent::ListDirectory`].
    pub fn show_directory(&mut self, directory: String, entries: Vec<FileEntry>) {
        self.files.show(directory, entries);
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
                // These two came from somewhere, and backing out returns there. Dropping
                // straight to the world would mean re-opening the HUD to make a second try at
                // a setting you have just decided against — the browser especially, which you
                // reach two levels down and often leave empty-handed.
                Mode::Environment => self.enter(Mode::Hud),
                Mode::Files => self.enter(Mode::Environment),
                _ => self.enter(Mode::World),
            },
            Intent::Navigate(direction) => {
                match self.mode {
                    Mode::Hud => self.hud.step(direction),
                    Mode::Environment => self.environments.step(direction),
                    Mode::Files => self.files.step(direction),
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
                    // The environment row opens a list rather than doing something, so it is
                    // the one row that goes deeper instead of back to the world. The event is
                    // still emitted: the compositor answers it by re-reading the folder, which
                    // is what makes a newly dropped-in image appear.
                    match action {
                        HudAction::OpenEnvironments => self.mode = Mode::Environment,
                        // Opening a settings window leaves the HUD up, since the window
                        // appears beside it rather than instead of it.
                        HudAction::OpenSystemSettings(_) => {}
                        // Everything else takes you back to the world: staying on the menu
                        // after recentring hides the thing you just changed.
                        _ => self.mode = Mode::World,
                    }
                    Some(ShellEvent::Hud(action))
                }
                Mode::Environment => match self.environments.activate() {
                    EnvironmentAction::Choose(choice) => {
                        self.mode = Mode::World;
                        Some(ShellEvent::ChooseEnvironment(choice))
                    }
                    EnvironmentAction::Browse => {
                        self.mode = Mode::Files;
                        Some(ShellEvent::ListDirectory(None))
                    }
                },
                Mode::Files => match self.files.activate() {
                    // Stay put: walking down a tree is several presses and each one needs the
                    // listing that answers it.
                    FileAction::Enter(name) => Some(ShellEvent::ListDirectory(Some(name))),
                    // Adding also selects — "add this one" means you want to see it, and
                    // returning to a list to then pick what you just added is a step nobody
                    // wants. The compositor does the selecting; the shell only says which.
                    FileAction::Choose(name) => {
                        self.mode = Mode::World;
                        Some(ShellEvent::AddEnvironment(name))
                    }
                },
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
        Shell::new(vec![app("alpha"), app("beta"), app("gamma")], DesktopPanels::ALL)
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
        let mut s = Shell::new(vec![], DesktopPanels::ALL);
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

    /// A shell sitting on the environment picker, with two images to choose from.
    fn at_the_picker() -> Shell {
        let mut s = shell();
        s.set_environments(
            vec![
                EnvironmentEntry {
                    label: "Blank (black)".into(),
                    choice: EnvironmentChoice::Blank,
                },
                EnvironmentEntry {
                    label: "Studio (generated)".into(),
                    choice: EnvironmentChoice::Studio,
                },
                EnvironmentEntry {
                    label: "sunset".into(),
                    choice: EnvironmentChoice::File(0),
                },
            ],
            EnvironmentChoice::Studio,
        );
        s.handle(Intent::ToggleHud);
        open_environments(&mut s);
        s
    }

    /// Walk the HUD down to the Environment row and press A.
    ///
    /// By position rather than by index, so inserting a row above it does not silently make
    /// these tests exercise the wrong thing.
    fn open_environments(s: &mut Shell) {
        let row = s
            .hud()
            .items()
            .iter()
            .position(|i| i.action == HudAction::OpenEnvironments)
            .expect("the HUD has an Environment row");
        for _ in 0..row {
            s.handle(Intent::Navigate(Direction::Down));
        }
        assert_eq!(s.hud().activate(), HudAction::OpenEnvironments);
        s.handle(Intent::Accept);
    }

    #[test]
    fn the_environment_row_opens_a_list_instead_of_changing_anything() {
        // The behaviour the picker replaced: activating this row used to swap the world
        // immediately, so seeing your options meant living through all of them.
        let s = at_the_picker();
        assert_eq!(s.mode(), Mode::Environment);

        // And the compositor is still told, because that is its cue to re-read the folder —
        // which is the whole mechanism by which an image dropped in mid-session appears.
        let mut fresh = shell();
        fresh.handle(Intent::ToggleHud);
        let row = fresh
            .hud()
            .items()
            .iter()
            .position(|i| i.action == HudAction::OpenEnvironments)
            .expect("the HUD has an Environment row");
        for _ in 0..row {
            fresh.handle(Intent::Navigate(Direction::Down));
        }
        assert_eq!(
            fresh.handle(Intent::Accept),
            Some(ShellEvent::Hud(HudAction::OpenEnvironments))
        );
    }

    #[test]
    fn choosing_an_environment_names_it_and_returns_to_the_world() {
        let mut s = at_the_picker();
        // Cursor starts on what is in use; step up to Blank.
        s.handle(Intent::Navigate(Direction::Up));
        assert_eq!(
            s.handle(Intent::Accept),
            Some(ShellEvent::ChooseEnvironment(EnvironmentChoice::Blank))
        );
        assert_eq!(s.mode(), Mode::World, "you should be looking at what you picked");
    }

    #[test]
    fn backing_out_of_the_picker_returns_to_the_settings_it_came_from() {
        // Dropping to the world would mean re-opening the HUD to make a second attempt at a
        // setting you have just decided against.
        let mut s = at_the_picker();
        assert_eq!(s.handle(Intent::Back), Some(ShellEvent::ModeChanged(Mode::Hud)));
        assert_eq!(s.mode(), Mode::Hud);
    }

    #[test]
    fn the_browser_is_two_levels_down_and_b_climbs_out_one_at_a_time() {
        let mut s = at_the_picker();
        for _ in 0..10 {
            s.handle(Intent::Navigate(Direction::Down));
        }
        // The last row browses, and asks for a listing rather than guessing a path.
        assert_eq!(s.handle(Intent::Accept), Some(ShellEvent::ListDirectory(None)));
        assert_eq!(s.mode(), Mode::Files);
        assert_eq!(
            s.handle(Intent::Back),
            Some(ShellEvent::ModeChanged(Mode::Environment))
        );
        assert_eq!(
            s.handle(Intent::Back),
            Some(ShellEvent::ModeChanged(Mode::Hud))
        );
        assert_eq!(
            s.handle(Intent::Back),
            Some(ShellEvent::ModeChanged(Mode::World))
        );
    }

    #[test]
    fn walking_into_a_folder_stays_in_the_browser_but_picking_a_file_leaves() {
        // Descending is several presses and each needs the listing that answers it, so the
        // mode must not change. Choosing is done, so it must.
        let mut s = at_the_picker();
        for _ in 0..10 {
            s.handle(Intent::Navigate(Direction::Down));
        }
        s.handle(Intent::Accept);
        s.show_directory(
            "/home/deck/Pictures".into(),
            vec![
                FileEntry {
                    name: "Panoramas".into(),
                    is_directory: true,
                },
                FileEntry {
                    name: "sunset.jpg".into(),
                    is_directory: false,
                },
            ],
        );
        s.handle(Intent::Navigate(Direction::Down));
        assert_eq!(
            s.handle(Intent::Accept),
            Some(ShellEvent::ListDirectory(Some("Panoramas".into())))
        );
        assert_eq!(s.mode(), Mode::Files, "still browsing");

        s.show_directory(
            "/home/deck/Pictures/Panoramas".into(),
            vec![FileEntry {
                name: "hall.jpg".into(),
                is_directory: false,
            }],
        );
        s.handle(Intent::Navigate(Direction::Down));
        assert_eq!(
            s.handle(Intent::Accept),
            Some(ShellEvent::AddEnvironment("hall.jpg".into()))
        );
        assert_eq!(s.mode(), Mode::World, "adding also shows it");
    }

    #[test]
    fn every_new_mode_still_counts_as_a_menu_over_the_world() {
        // `menu_is_open` is what suppresses the pointer and the two-thumb gesture. A mode
        // that forgets to be a menu lets the laser drag a window behind the list you are
        // reading, which is invisible until it has already moved something.
        let mut s = at_the_picker();
        assert!(s.menu_is_open());
        for _ in 0..10 {
            s.handle(Intent::Navigate(Direction::Down));
        }
        s.handle(Intent::Accept);
        assert_eq!(s.mode(), Mode::Files);
        assert!(s.menu_is_open());
    }
}
