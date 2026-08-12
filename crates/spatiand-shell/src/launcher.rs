//! The home screen — a grid of applications as glass bubbles.
//!
//! Opened with the `⋯` button, in the manner of visionOS or NebulaOS: a floating arc of icons
//! in front of you rather than a window containing a list. The apps come from the system's own
//! desktop entries, so whatever is installed shows up without Spatiand keeping a catalogue of
//! its own.
//!
//! The layout is an **arc, not a plane**. Icons are placed at a fixed distance around the
//! viewer, which keeps every bubble the same size and the same focal distance — on a plane the
//! outer icons are further away and smaller, and the eyes have to re-converge as the cursor
//! travels. That is tiring in a way that is hard to attribute to layout.

use crate::grid::{Direction, Grid};

/// One launchable application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub name: String,
    /// Command line, already stripped of desktop-entry field codes.
    pub exec: String,
    /// Absolute path to an icon, if one was found. Absent is normal and not an error: the
    /// bubble falls back to the app's initial, which is legible at bubble size anyway.
    pub icon: Option<String>,
}

/// Angular spacing between adjacent bubbles, degrees.
///
/// At the 40° field of one eye, 11° puts roughly three bubbles across the view. Tighter and
/// the glass edges of neighbouring bubbles interfere; wider and a five-column grid needs a
/// head turn to see its ends.
pub const COLUMN_SPACING_DEG: f32 = 11.0;
pub const ROW_SPACING_DEG: f32 = 10.0;
/// How far out the arc sits, metres. Matches the default window radius so switching between
/// the launcher and a window does not change focal distance.
pub const ARC_RADIUS_M: f32 = 2.0;
/// Bubbles per row.
pub const COLUMNS: usize = 5;

/// Where one bubble goes, in the viewer-centred frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BubblePlacement {
    /// Radians, positive to the left — the same convention as `spatiand::window::Placement`.
    pub yaw: f32,
    pub pitch: f32,
    pub radius: f32,
    /// 1.0 for a normal bubble; the focused one is grown slightly.
    pub scale: f32,
}

/// The launcher's state.
#[derive(Debug, Clone)]
pub struct Launcher {
    apps: Vec<AppEntry>,
    grid: Grid,
}

impl Launcher {
    pub fn new(apps: Vec<AppEntry>) -> Self {
        let grid = Grid::new(COLUMNS, apps.len());
        Self { apps, grid }
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    pub fn apps(&self) -> &[AppEntry] {
        &self.apps
    }

    pub fn cursor(&self) -> usize {
        self.grid.cursor()
    }

    pub fn focused(&self) -> Option<&AppEntry> {
        self.apps.get(self.grid.cursor())
    }

    /// Replace the app list, keeping the cursor valid.
    pub fn set_apps(&mut self, apps: Vec<AppEntry>) {
        self.grid.resize(apps.len());
        self.apps = apps;
    }

    pub fn step(&mut self, direction: Direction) -> bool {
        self.grid.step(direction)
    }

    /// Where a bubble sits.
    ///
    /// Rows are laid out downward from a little above the horizon, so a single-row launcher
    /// sits at a comfortable reading height rather than at your feet.
    pub fn placement(&self, index: usize) -> BubblePlacement {
        let (_, row) = self.grid.position(index);
        let column_offset = self.grid.centred_offset(index);
        // Centre the block of rows vertically about the eye line.
        let row_offset = row as f32 - (self.grid.rows().max(1) as f32 - 1.0) * 0.5;
        BubblePlacement {
            // Positive yaw is to the left, and column 0 is the leftmost, so the offset runs
            // against the column index. Getting this backwards mirrors the whole grid and
            // makes the D-pad appear to move the cursor the wrong way.
            yaw: -column_offset * COLUMN_SPACING_DEG.to_radians(),
            pitch: -row_offset * ROW_SPACING_DEG.to_radians(),
            radius: ARC_RADIUS_M,
            scale: if index == self.grid.cursor() { 1.18 } else { 1.0 },
        }
    }

    /// Every bubble, in draw order.
    ///
    /// The focused bubble is emitted **last** so it draws over its neighbours: it is scaled up
    /// and would otherwise be clipped by whatever happens to come after it.
    pub fn placements(&self) -> Vec<(usize, BubblePlacement)> {
        let mut out: Vec<(usize, BubblePlacement)> = (0..self.apps.len())
            .map(|i| (i, self.placement(i)))
            .collect();
        out.sort_by_key(|(i, _)| *i == self.grid.cursor());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppEntry {
        AppEntry {
            name: name.into(),
            exec: format!("/usr/bin/{}", name.to_lowercase()),
            icon: None,
        }
    }

    fn launcher_of(n: usize) -> Launcher {
        Launcher::new((0..n).map(|i| app(&format!("App{i}"))).collect())
    }

    #[test]
    fn an_empty_launcher_has_nothing_focused_and_does_not_panic() {
        let l = Launcher::new(vec![]);
        assert!(l.is_empty());
        assert!(l.focused().is_none());
        assert!(l.placements().is_empty());
    }

    #[test]
    fn the_cursor_starts_on_the_first_app() {
        let l = launcher_of(8);
        assert_eq!(l.focused().map(|a| a.name.as_str()), Some("App0"));
    }

    #[test]
    fn pressing_right_moves_the_focus_right_in_the_world() {
        // The mirror test. Column 0 is leftmost, and +yaw is left, so moving right must make
        // the yaw DECREASE. A sign slip here makes the D-pad feel inverted.
        let mut l = launcher_of(5);
        let before = l.placement(l.cursor()).yaw;
        l.step(Direction::Right);
        let after = l.placement(l.cursor()).yaw;
        assert!(after < before, "yaw went {before} -> {after}");
    }

    #[test]
    fn the_focused_bubble_is_larger_than_the_others() {
        let l = launcher_of(5);
        assert!(l.placement(0).scale > l.placement(1).scale);
    }

    #[test]
    fn the_focused_bubble_draws_last() {
        // It is scaled up, so drawing it before its neighbours lets them clip its edges.
        let mut l = launcher_of(5);
        l.step(Direction::Right);
        let order = l.placements();
        assert_eq!(order.last().map(|(i, _)| *i), Some(l.cursor()));
        assert_eq!(order.len(), 5, "every app must still be drawn exactly once");
    }

    #[test]
    fn a_full_row_is_centred_on_straight_ahead() {
        let l = launcher_of(COLUMNS);
        let total: f32 = (0..COLUMNS).map(|i| l.placement(i).yaw).sum();
        assert!(total.abs() < 1e-6, "row should balance about zero, got {total}");
        // With an odd column count the middle bubble is dead ahead.
        assert!(l.placement(COLUMNS / 2).yaw.abs() < 1e-6);
    }

    #[test]
    fn a_single_row_sits_on_the_eye_line() {
        // Not below it: a one-row launcher pitched down means looking at your feet to pick an
        // app, which is the sort of thing that only shows up when wearing the thing.
        let l = launcher_of(3);
        assert!(l.placement(0).pitch.abs() < 1e-6);
    }

    #[test]
    fn extra_rows_are_balanced_above_and_below() {
        let l = launcher_of(COLUMNS * 2);
        let top = l.placement(0).pitch;
        let bottom = l.placement(COLUMNS).pitch;
        assert!(top > 0.0, "the first row should be the upper one");
        assert!((top + bottom).abs() < 1e-6, "rows should straddle the eye line");
    }

    #[test]
    fn every_bubble_is_the_same_distance_away() {
        // The reason the layout is an arc. Equal radius means equal apparent size and one
        // focal distance for the whole grid.
        let l = launcher_of(9);
        for i in 0..9 {
            assert_eq!(l.placement(i).radius, ARC_RADIUS_M);
        }
    }

    #[test]
    fn rescanning_the_app_list_keeps_the_cursor_in_range() {
        let mut l = launcher_of(12);
        for _ in 0..8 {
            l.step(Direction::Right);
            l.step(Direction::Down);
        }
        l.set_apps(vec![app("Only")]);
        assert_eq!(l.cursor(), 0);
        assert_eq!(l.focused().map(|a| a.name.as_str()), Some("Only"));
    }

    #[test]
    fn the_grid_stays_inside_a_head_turn() {
        // A full row must not span so far that its ends are behind you. 5 columns at 11 deg
        // is 44 deg total, comfortably inside a natural head turn.
        let l = launcher_of(COLUMNS);
        let widest = (0..COLUMNS)
            .map(|i| l.placement(i).yaw.abs())
            .fold(0.0f32, f32::max);
        assert!(widest.to_degrees() < 45.0, "half-width {}", widest.to_degrees());
    }
}
