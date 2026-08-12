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
    Calibrate,
    /// Toggle the glasses between mono and side-by-side.
    ToggleStereo,
    /// Cycle the 360 environment.
    NextEnvironment,
    /// Hand the display back and return to the desktop session.
    ReturnToDesktop,
    /// Float a system settings panel as a 2D window.
    OpenSystemSettings(&'static str),
    /// Close the HUD.
    Dismiss,
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
        Self::new(true)
    }
}

impl Hud {
    /// Build the HUD. `has_desktop_settings` hides the entries that shell out to KDE on a
    /// system without it, rather than offering a row that does nothing.
    pub fn new(has_desktop_settings: bool) -> Self {
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
                detail: "Change the world around you",
                action: HudAction::NextEnvironment,
            },
            HudItem {
                label: "Stereo",
                detail: "Switch the glasses between 3D and flat",
                action: HudAction::ToggleStereo,
            },
        ];
        if has_desktop_settings {
            items.push(HudItem {
                label: "Wi-Fi and Bluetooth",
                detail: "Opens the system panel as a window in front of you",
                action: HudAction::OpenSystemSettings("kcm_networkmanagement"),
            });
            items.push(HudItem {
                label: "Display and sound",
                detail: "Opens the system panel as a window in front of you",
                action: HudAction::OpenSystemSettings("kcm_kscreen"),
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
        assert!(!Hud::new(false).items().is_empty());
        assert!(!Hud::new(true).items().is_empty());
    }

    #[test]
    fn a_system_without_kde_is_not_offered_kde_panels() {
        // Offering a row that silently does nothing is worse than not offering it: the wearer
        // concludes the whole HUD is broken.
        let bare = Hud::new(false);
        assert!(!bare
            .items()
            .iter()
            .any(|i| matches!(i.action, HudAction::OpenSystemSettings(_))));
        let full = Hud::new(true);
        assert!(full
            .items()
            .iter()
            .any(|i| matches!(i.action, HudAction::OpenSystemSettings(_))));
    }

    #[test]
    fn leaving_is_always_the_last_entry() {
        // Muscle memory: "down until it stops, then A" should always be the way out, whether
        // or not the optional rows are present.
        for kde in [true, false] {
            let hud = Hud::new(kde);
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
