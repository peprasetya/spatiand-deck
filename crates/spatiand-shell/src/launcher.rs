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
/// A bubble subtends about 4.6°, so 9° leaves nearly a full bubble of space between
/// neighbours.
///
/// The numbers here are set by a hard constraint rather than by taste: one eye sees **40°
/// across and only 23° vertically**. Four columns at 9° reach ±15.8° including the glass,
/// inside the 20° half-width. Earlier attempts at 11° and 13° looked reasonable written down
/// and put the outer column past the edge of the field.
pub const COLUMN_SPACING_DEG: f32 = 9.0;
/// Rows are tighter than columns because the vertical field is half the horizontal one, and
/// each bubble still has to fit a label underneath it. Three rows at 7° reach ±10.7°
/// including the label, against a half-height of 11.57°.
pub const ROW_SPACING_DEG: f32 = 7.0;
/// How far out the arc sits, metres. Matches the default window radius so switching between
/// the launcher and a window does not change focal distance.
pub const ARC_RADIUS_M: f32 = 2.0;
/// Bubbles per row.
pub const COLUMNS: usize = 4;
/// Rows shown at once.
///
/// Three, capped deliberately. More than that and the outer rows are past the vertical field
/// and have to be hunted for by tilting your head, which is far worse than paging.
pub const ROWS_PER_PAGE: usize = 3;
/// Bubbles on one page.
pub const PAGE_SIZE: usize = COLUMNS * ROWS_PER_PAGE;

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

    /// Which page the cursor is on. Pages exist so the grid never spills past the field of
    /// view; the alternative is rows you have to find by tilting your head.
    pub fn page(&self) -> usize {
        self.grid.cursor() / PAGE_SIZE
    }

    pub fn pages(&self) -> usize {
        self.apps.len().div_ceil(PAGE_SIZE).max(1)
    }

    /// Indices visible on the current page.
    pub fn visible(&self) -> std::ops::Range<usize> {
        let start = self.page() * PAGE_SIZE;
        start..(start + PAGE_SIZE).min(self.apps.len())
    }

    /// Where a bubble sits.
    ///
    /// Rows are laid out downward from a little above the horizon, so a single-row launcher
    /// sits at a comfortable reading height rather than at your feet.
    pub fn placement(&self, index: usize) -> BubblePlacement {
        // Everything is relative to the page, so bubble 13 on page 2 sits where bubble 1 does.
        let local = index % PAGE_SIZE;
        let row = local / COLUMNS;
        let page_start = (index / PAGE_SIZE) * PAGE_SIZE;
        let on_this_page = (self.apps.len() - page_start).min(PAGE_SIZE);
        let rows_here = on_this_page.div_ceil(COLUMNS).max(1);
        let in_this_row = if row + 1 < rows_here {
            COLUMNS
        } else {
            on_this_page - row * COLUMNS
        };
        let column_offset = (local % COLUMNS) as f32 - (in_this_row as f32 - 1.0) * 0.5;
        // Centre the block of rows vertically about the eye line.
        let row_offset = row as f32 - (rows_here as f32 - 1.0) * 0.5;
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

    /// The bubbles on the current page, in draw order.
    ///
    /// The focused bubble is emitted **last** so it draws over its neighbours: it is scaled up
    /// and would otherwise be clipped by whatever happens to come after it.
    pub fn placements(&self) -> Vec<(usize, BubblePlacement)> {
        let mut out: Vec<(usize, BubblePlacement)> =
            self.visible().map(|i| (i, self.placement(i))).collect();
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
        assert!(total.abs() < 1e-5, "row should balance about zero, got {total}");
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
        // focal distance for the whole grid -- and, because yaw and pitch are angles about the
        // viewer, a bubble stays the same distance away as you turn to face it.
        let l = launcher_of(30);
        for i in 0..30 {
            assert_eq!(l.placement(i).radius, ARC_RADIUS_M);
        }
    }

    #[test]
    fn rescanning_the_app_list_keeps_the_cursor_in_range() {
        let mut l = launcher_of(30);
        for _ in 0..8 {
            l.step(Direction::Right);
            l.step(Direction::Down);
        }
        l.set_apps(vec![app("Only")]);
        assert_eq!(l.cursor(), 0);
        assert_eq!(l.focused().map(|a| a.name.as_str()), Some("Only"));
    }

    #[test]
    fn a_page_never_spills_past_the_field_of_view() {
        // The whole reason for paging. A full page must fit inside a comfortable head turn
        // horizontally and inside the vertical field without tilting -- 23 degrees at the
        // glasses' aspect, so the rows have to stay within about +-21 degrees.
        let l = launcher_of(200);
        for i in l.visible() {
            let p = l.placement(i);
            assert!(p.yaw.to_degrees().abs() <= 20.0, "bubble {i} at yaw {}", p.yaw.to_degrees());
            assert!(p.pitch.to_degrees().abs() <= 21.0, "bubble {i} at pitch {}", p.pitch.to_degrees());
        }
    }

    #[test]
    fn only_one_page_is_drawn_at_a_time() {
        // 39 apps used to draw 39 bubbles, most of them behind the wearer's head.
        let l = launcher_of(200);
        assert_eq!(l.placements().len(), PAGE_SIZE);
        assert!(l.placements().iter().all(|(i, _)| l.visible().contains(i)));
    }

    #[test]
    fn moving_past_the_end_of_a_page_turns_to_the_next_one() {
        let mut l = launcher_of(200);
        assert_eq!(l.page(), 0);
        for _ in 0..ROWS_PER_PAGE {
            l.step(Direction::Down);
        }
        assert_eq!(l.page(), 1, "should have paged");
        // And the cursor's bubble must be on screen, not off the bottom of the previous page.
        assert!(l.visible().contains(&l.cursor()));
    }

    #[test]
    fn the_last_page_is_laid_out_as_if_it_were_full_height() {
        // A ragged final page must still be centred rather than clinging to the top row.
        let l = launcher_of(PAGE_SIZE + 2);
        let mut cursor = Launcher::new(l.apps().to_vec());
        while cursor.page() == 0 {
            if !cursor.step(Direction::Down) && !cursor.step(Direction::Right) {
                break;
            }
        }
        assert_eq!(cursor.page(), 1);
        let p = cursor.placement(PAGE_SIZE);
        assert!(p.pitch.abs() < 1e-6, "a one-row page should sit on the eye line");
    }

    #[test]
    fn a_full_page_fits_the_glasses_field_of_view() {
        // The binding constraint, and the one that went wrong twice. One eye sees 40 deg
        // across but only 23 deg vertically, so the rows are what run out of room first -- and
        // a row that is off the field is only findable by tilting your head.
        //
        // Diameter is 0.16 m at a 2 m radius; the focused bubble is 18% larger, and each
        // carries a label under it.
        let bubble_half = (0.16f32 * 1.18 / 2.0 / ARC_RADIUS_M).atan().to_degrees();
        // The lowest thing on a bubble is the bottom of its label, not the bottom of its
        // glass: scene.rs drops the label by half a focused diameter plus 22 mm, and the label
        // itself is 22 mm tall. Adding the two extents instead of taking the lower of them
        // over-counts by a whole bubble radius.
        let label_bottom = ((0.16f32 * 0.5 * 1.18 + 0.022 + 0.011) / ARC_RADIUS_M)
            .atan()
            .to_degrees();

        let widest = (COLUMNS as f32 - 1.0) / 2.0 * COLUMN_SPACING_DEG + bubble_half;
        assert!(widest <= 20.0, "a row reaches {widest} deg against a 20 deg half-width");

        let tallest =
            (ROWS_PER_PAGE as f32 - 1.0) / 2.0 * ROW_SPACING_DEG + bubble_half.max(label_bottom);
        assert!(
            tallest <= 11.57,
            "a page reaches {tallest} deg against an 11.57 deg half-height"
        );
    }

    #[test]
    fn there_is_real_space_between_neighbouring_bubbles() {
        // Otherwise the grid reads as a wall of glass rather than as separate objects, which
        // is how it looked at 0.30 m bubbles on 11 deg spacing.
        let bubble_deg = 2.0 * (0.16f32 / 2.0 / ARC_RADIUS_M).atan().to_degrees();
        assert!(
            COLUMN_SPACING_DEG > bubble_deg * 1.5,
            "columns {COLUMN_SPACING_DEG} deg vs {bubble_deg} deg bubbles"
        );
        assert!(ROW_SPACING_DEG > bubble_deg * 1.4);
    }
}
