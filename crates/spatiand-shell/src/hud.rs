//! The STEAM-button HUD — spatial settings and the things you need mid-session.
//!
//! Deliberately one flat list rather than a tree. You are reading it through optics with about
//! 640 usable pixels across a panel, wearing something on your face, probably standing up.
//! Nested menus are how you lose your place under those conditions.
//!
//! Recentre and calibrate live here because they are the two things that fix a session gone
//! wrong, and both had to be reachable without a terminal — that was the requirement that
//! shaped the whole in-world calibration flow.
//!
//! Anything genuinely deep (a wifi password, Bluetooth pairing) is not reimplemented; the
//! entry marked [`HudAction::OpenSystemSettings`] floats KDE's own dialog as an ordinary
//! window in the 3D space. That costs nothing where Plasma is present and is simply hidden
//! where it is not, which is what keeps the shell portable.

use crate::grid::Direction;

// There is deliberately no "display and sound" row either.
//
// It opened KDE's screen KCM, which on a headset is a panel for rearranging monitors that are
// not there — and it was the one entry that could take the session down with it, since the
// module reconfigures the very outputs Spatiand is driving. But the reason it is gone is not
// that it crashed. Apparent size here is controlled by moving a window in the world, not by a
// resolution; the glasses have one native mode. A spatial desktop has no display settings in
// the sense a monitor does, the same way an iPad or a Vision Pro does not. Sound has a
// picker of its own in the sidecar, which is where the output actually gets chosen.
//
// Screen blanking and lock are the one part of that panel that would still mean something,
// and they are unresolved rather than dismissed: SteamOS's own idle handling and this
// session have not been tested against each other. When that is settled it belongs as its
// own row, phrased as what it does, not as a KCM.

// There is deliberately no "stereo on/off" row.
//
// Spatiand is a stereo desktop; a mono mode is not a feature of it, it is a broken version of
// it. The row existed because the display negotiation *could* be toggled, which is not the
// same as it being worth offering — and in use it did nothing legible except make the glasses
// flicker while they renegotiated. Resolution is never a user setting either, for the same
// reason: the glasses have one native mode and apparent size is controlled by moving things
// in the world.

/// What activating a HUD row asks the compositor to do.
///
/// The shell never performs these itself. It has no headset handle, no tracker and no process
/// table, and giving it any of them would make it untestable — the reason it is a separate
/// crate at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HudAction {
    /// Make the direction you are facing the new forward. The single most-used control in any
    /// 3DoF headset, because yaw drift is bounded but never zero.
    Recentre,
    /// Re-run the in-world axis calibration.
    ///
    /// There is deliberately no companion control for stepping through pitch/roll
    /// interpretations. One existed, and it was a symptom: the sensor convention was being
    /// treated as something to be guessed at per wearer when it is a fact about where the IMU
    /// is soldered. It now comes from the device table, so there is nothing to step through —
    /// see `AxisMap::from_mounting`. This entry is for hardware whose mounting nobody has
    /// measured yet, which is the only case left that needs it.
    Calibrate,
    /// Save what the wearer is looking at, so a problem can be shown rather than described.
    Screenshot,
    /// Show or hide the on-screen keyboard.
    ToggleKeyboard,
    /// Open the environment picker.
    ///
    /// This was once "cycle to the next one", which is a control that gets worse with every
    /// image you add: reaching the fourth means loading and looking at the second and third,
    /// and there is no way back other than all the way round. The compositor should answer
    /// this by re-reading the environments folder and calling `Shell::set_environments`, which
    /// is what lets an image dropped in mid-session appear without a restart.
    OpenEnvironments,
    /// Hand the display back and return to the desktop session.
    ReturnToDesktop,
    /// Float one of the desktop's own settings panels as a 2D window.
    ///
    /// Carries what the panel *is* -- "wifi", "bluetooth" -- and not how to open it. It used
    /// to carry a KDE settings-module name, which put one desktop's vocabulary in the crate
    /// that is meant not to have any. The compositor asks
    /// `spatiand_platform::settings_command` what that means on this machine.
    OpenSystemSettings(&'static str),
    /// Close the HUD.
    Dismiss,
}

/// Which of the desktop's own settings panels exist on this machine.
///
/// Two flags rather than one, because they are separately installable: Plasma ships the runner
/// and the network module, while the Bluetooth module comes from bluedevil. Treating them as a
/// single "has KDE" answer is what produced a row labelled Bluetooth that only ever opened
/// Wi-Fi.
///
/// The shell does not work this out for itself. It has no filesystem to look at and no
/// business having one — the compositor asks `spatiand_platform::panel_available` and passes
/// the answers in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DesktopPanels {
    pub network: bool,
    pub bluetooth: bool,
}

impl DesktopPanels {
    /// Nothing to shell out to. What a machine without a desktop session gets.
    pub const NONE: Self = Self { network: false, bluetooth: false };
    /// Everything present. The Deck, and what the tests assume unless they say otherwise.
    pub const ALL: Self = Self { network: true, bluetooth: true };
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudItem {
    pub label: &'static str,
    /// One line of explanation. Worth the space: these are actions people reach for rarely and
    /// under mild stress, and a bare verb is not enough to commit to pressing A.
    pub detail: &'static str,
    pub action: HudAction,
}

/// The HUD's state.
#[derive(Debug, Clone)]
pub struct Hud {
    items: Vec<HudItem>,
    cursor: usize,
}

impl Default for Hud {
    fn default() -> Self {
        Self::new(DesktopPanels::ALL)
    }
}

impl Hud {
    /// Build the HUD. `panels` hides the entries that shell out to the desktop's own settings
    /// on a system that does not have them, rather than offering a row that does nothing.
    pub fn new(panels: DesktopPanels) -> Self {
        let mut items = vec![
            HudItem {
                label: "Recentre",
                detail: "Make where you are looking the new forward",
                action: HudAction::Recentre,
            },
            HudItem {
                label: "Calibrate head tracking",
                detail: "Three short movements, about half a minute",
                action: HudAction::Calibrate,
            },
            HudItem {
                label: "Environment",
                detail: "Choose what surrounds you, or add an image",
                action: HudAction::OpenEnvironments,
            },
            HudItem {
                label: "Keyboard",
                detail: "Show a keyboard you can point at and click",
                action: HudAction::ToggleKeyboard,
            },
            HudItem {
                label: "Take a screenshot",
                detail: "Saves what you are looking at to your Pictures folder",
                action: HudAction::Screenshot,
            },
        ];
        if panels.network {
            items.push(HudItem {
                label: "Wi-Fi",
                detail: "Join a network, in the system panel as a window in front of you",
                action: HudAction::OpenSystemSettings("wifi"),
            });
        }
        if panels.bluetooth {
            items.push(HudItem {
                label: "Bluetooth",
                detail: "Pair headphones or a controller, in a window in front of you",
                action: HudAction::OpenSystemSettings("bluetooth"),
            });
        }
        items.push(HudItem {
            label: "Leave Spatiand",
            detail: "Close every window and return to the desktop",
            action: HudAction::ReturnToDesktop,
        });
        Self { items, cursor: 0 }
    }

    pub fn items(&self) -> &[HudItem] {
        &self.items
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn focused(&self) -> &HudItem {
        // `new` always pushes at least one item, so this cannot be empty.
        &self.items[self.cursor.min(self.items.len() - 1)]
    }

    /// Move the highlight. Left and right are ignored: it is a single column, and having them
    /// wrap onto another row would be a surprise.
    pub fn step(&mut self, direction: Direction) -> bool {
        let before = self.cursor;
        match direction {
            Direction::Up => self.cursor = self.cursor.saturating_sub(1),
            Direction::Down => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
            }
            Direction::Left | Direction::Right => {}
        }
        self.cursor != before
    }

    /// What pressing A does.
    pub fn activate(&self) -> HudAction {
        self.focused().action.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recentre_and_calibrate_are_both_present_and_near_the_top() {
        // These are the two controls that rescue a session, and the requirement was explicit
        // that neither should need a terminal. Burying them defeats that as surely as
        // removing them.
        let hud = Hud::default();
        let labels: Vec<_> = hud.items().iter().map(|i| i.action.clone()).collect();
        let recentre = labels.iter().position(|a| *a == HudAction::Recentre);
        let calibrate = labels.iter().position(|a| *a == HudAction::Calibrate);
        assert_eq!(recentre, Some(0));
        assert!(calibrate.unwrap() <= 2, "calibrate should be reachable at a glance");
    }

    #[test]
    fn the_hud_is_never_empty_so_focused_cannot_panic() {
        assert!(!Hud::new(DesktopPanels::NONE).items().is_empty());
        assert!(!Hud::new(DesktopPanels::ALL).items().is_empty());
    }

    #[test]
    fn a_system_without_kde_is_not_offered_kde_panels() {
        // Offering a row that silently does nothing is worse than not offering it: the wearer
        // concludes the whole HUD is broken.
        let bare = Hud::new(DesktopPanels::NONE);
        assert!(!bare
            .items()
            .iter()
            .any(|i| matches!(i.action, HudAction::OpenSystemSettings(_))));
        let full = Hud::new(DesktopPanels::ALL);
        assert!(full
            .items()
            .iter()
            .any(|i| matches!(i.action, HudAction::OpenSystemSettings(_))));
    }

    #[test]
    fn wifi_and_bluetooth_are_separate_rows_that_open_separate_panels() {
        // They were one row labelled "Wi-Fi and Bluetooth" that opened the network module and
        // nothing else, so half the label was a lie. Two rows, two modules.
        let hud = Hud::new(DesktopPanels::ALL);
        let modules: Vec<_> = hud
            .items()
            .iter()
            .filter_map(|i| match i.action {
                HudAction::OpenSystemSettings(m) => Some((i.label, m)),
                _ => None,
            })
            .collect();
        assert_eq!(
            modules,
            vec![("Wi-Fi", "wifi"), ("Bluetooth", "bluetooth")]
        );
    }

    #[test]
    fn bluetooth_can_be_absent_while_wifi_is_present() {
        // The reason this is two flags: the runner and the network module ship with Plasma,
        // the Bluetooth one ships with bluedevil. One installed without the other is ordinary.
        let hud = Hud::new(DesktopPanels { network: true, bluetooth: false });
        let modules: Vec<_> = hud
            .items()
            .iter()
            .filter_map(|i| match i.action {
                HudAction::OpenSystemSettings(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(modules, vec!["wifi"]);
    }

    #[test]
    fn there_is_no_display_settings_row() {
        // Removed on purpose, and worth a test rather than only a comment: the pull to add
        // "just a resolution setting" back is constant, and on a headset it means nothing.
        // Apparent size comes from where a window sits in the world.
        for panels in [DesktopPanels::ALL, DesktopPanels::NONE] {
            assert!(!Hud::new(panels).items().iter().any(|i| matches!(
                i.action,
                HudAction::OpenSystemSettings("display")
            )));
        }
    }

    #[test]
    fn leaving_is_always_the_last_entry() {
        // Muscle memory: "down until it stops, then A" should always be the way out, whether
        // or not the optional rows are present.
        for panels in [DesktopPanels::ALL, DesktopPanels::NONE] {
            let hud = Hud::new(panels);
            assert_eq!(hud.items().last().map(|i| i.action.clone()), Some(HudAction::ReturnToDesktop));
        }
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut hud = Hud::default();
        assert!(!hud.step(Direction::Up), "already at the top");
        for _ in 0..hud.items().len() * 2 {
            hud.step(Direction::Down);
        }
        assert_eq!(hud.cursor(), hud.items().len() - 1);
        assert!(!hud.step(Direction::Down), "already at the bottom");
    }

    #[test]
    fn sideways_input_does_nothing_in_a_single_column() {
        let mut hud = Hud::default();
        hud.step(Direction::Down);
        let before = hud.cursor();
        assert!(!hud.step(Direction::Left));
        assert!(!hud.step(Direction::Right));
        assert_eq!(hud.cursor(), before);
    }

    #[test]
    fn activating_returns_the_focused_action() {
        let mut hud = Hud::default();
        assert_eq!(hud.activate(), HudAction::Recentre);
        hud.step(Direction::Down);
        assert_eq!(hud.activate(), HudAction::Calibrate);
    }

    #[test]
    fn every_row_explains_itself() {
        // A bare verb is not enough for something you reach for once a month, in a headset.
        for item in Hud::default().items() {
            assert!(!item.label.is_empty());
            assert!(item.detail.len() > 10, "{} has no useful detail", item.label);
        }
    }
}
