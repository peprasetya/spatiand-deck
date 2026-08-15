//! Choosing what surrounds you.
//!
//! This is a *picker*, not a cycle. The difference matters more than it sounds: with a cycle
//! the only way to reach the fourth panorama is to look at the second and third on the way,
//! and the only way to go back one is to go forward all the way round. Once there is more than
//! a handful of images that stops being a control and becomes a chore.
//!
//! The shell has no filesystem, so [`EnvironmentChoice::File`] carries an index into a list the
//! compositor owns rather than a path. The shell's whole job here is to say which row was
//! pressed.

use crate::grid::Direction;

/// The last row of the picker. Not an environment — a way to go and find one.
pub const BROWSE_LABEL: &str = "Add an image...";

/// What the wearer has chosen to be surrounded by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentChoice {
    /// Black, and nothing else.
    Blank,
    /// The generated studio, which is always available because it is computed rather than
    /// loaded.
    Studio,
    /// One of the discovered images, by position in the compositor's list.
    File(usize),
}

/// One environment on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEntry {
    pub label: String,
    pub choice: EnvironmentChoice,
}

/// What pressing A on a row means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentAction {
    Choose(EnvironmentChoice),
    /// Open the file browser.
    Browse,
}

/// One line as it should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvironmentRow<'a> {
    pub label: &'a str,
    /// This is the one currently wrapped around you.
    pub in_use: bool,
}

/// The environment picker.
#[derive(Debug, Clone, Default)]
pub struct EnvironmentPicker {
    entries: Vec<EnvironmentEntry>,
    current: Option<EnvironmentChoice>,
    cursor: usize,
}

impl EnvironmentPicker {
    /// Replace the list. Called whenever the picker is opened, so an image dropped into the
    /// folder mid-session appears without a restart.
    ///
    /// The cursor lands on whatever is in use rather than staying where it was: opening the
    /// list should show you where you are, and the row you last *looked* at is not that.
    pub fn set_entries(&mut self, entries: Vec<EnvironmentEntry>, current: EnvironmentChoice) {
        self.entries = entries;
        self.current = Some(current);
        self.cursor = self
            .entries
            .iter()
            .position(|e| e.choice == current)
            .unwrap_or(0);
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn current(&self) -> Option<EnvironmentChoice> {
        self.current
    }

    /// Every line, the trailing browse row included.
    ///
    /// There is always at least one row even before the compositor has said anything, because
    /// the browse row does not come from the list.
    pub fn rows(&self) -> Vec<EnvironmentRow<'_>> {
        let mut rows: Vec<EnvironmentRow<'_>> = self
            .entries
            .iter()
            .map(|e| EnvironmentRow {
                label: &e.label,
                in_use: Some(e.choice) == self.current,
            })
            .collect();
        rows.push(EnvironmentRow {
            label: BROWSE_LABEL,
            in_use: false,
        });
        rows
    }

    fn len(&self) -> usize {
        self.entries.len() + 1
    }

    /// Move the highlight. Sideways does nothing — it is a single column.
    pub fn step(&mut self, direction: Direction) -> bool {
        let before = self.cursor;
        match direction {
            Direction::Up => self.cursor = self.cursor.saturating_sub(1),
            Direction::Down => {
                if self.cursor + 1 < self.len() {
                    self.cursor += 1;
                }
            }
            Direction::Left | Direction::Right => {}
        }
        self.cursor != before
    }

    /// What pressing A does. The row past the end of the list is the browse row.
    pub fn activate(&self) -> EnvironmentAction {
        match self.entries.get(self.cursor) {
            Some(entry) => EnvironmentAction::Choose(entry.choice),
            None => EnvironmentAction::Browse,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, choice: EnvironmentChoice) -> EnvironmentEntry {
        EnvironmentEntry {
            label: label.into(),
            choice,
        }
    }

    fn picker() -> EnvironmentPicker {
        let mut p = EnvironmentPicker::default();
        p.set_entries(
            vec![
                entry("Blank", EnvironmentChoice::Blank),
                entry("Studio", EnvironmentChoice::Studio),
                entry("sunset", EnvironmentChoice::File(0)),
                entry("forest", EnvironmentChoice::File(1)),
            ],
            EnvironmentChoice::Studio,
        );
        p
    }

    #[test]
    fn opening_the_list_puts_the_cursor_on_what_you_are_looking_at() {
        // The whole reason for a picker over a cycle is knowing where you are. A cursor that
        // starts at the top tells you nothing.
        let p = picker();
        assert_eq!(p.cursor(), 1);
        assert!(p.rows()[1].in_use);
        assert!(!p.rows()[0].in_use);
    }

    #[test]
    fn any_row_is_reachable_without_passing_through_the_others() {
        // The complaint that motivated this: a cycle makes "the fourth one" cost three
        // environment loads, each of which uploads a texture.
        let mut p = picker();
        assert_eq!(p.activate(), EnvironmentAction::Choose(EnvironmentChoice::Studio));
        p.step(Direction::Down);
        p.step(Direction::Down);
        assert_eq!(p.activate(), EnvironmentAction::Choose(EnvironmentChoice::File(1)));
        // And back up, which a cycle cannot do at all.
        p.step(Direction::Up);
        assert_eq!(p.activate(), EnvironmentAction::Choose(EnvironmentChoice::File(0)));
    }

    #[test]
    fn blank_is_a_choice_like_any_other() {
        let mut p = picker();
        p.step(Direction::Up);
        assert_eq!(p.activate(), EnvironmentAction::Choose(EnvironmentChoice::Blank));
    }

    #[test]
    fn the_last_row_browses_rather_than_choosing() {
        let mut p = picker();
        for _ in 0..20 {
            p.step(Direction::Down);
        }
        assert_eq!(p.activate(), EnvironmentAction::Browse);
        assert_eq!(p.rows().last().map(|r| r.label), Some(BROWSE_LABEL));
    }

    #[test]
    fn an_empty_picker_still_offers_a_way_out_of_being_empty() {
        // A machine with no images and a compositor that has not spoken yet. Activating must
        // reach the browser rather than panicking on an empty list — this is exactly the state
        // someone hits on a first run.
        let p = EnvironmentPicker::default();
        assert_eq!(p.rows().len(), 1);
        assert_eq!(p.activate(), EnvironmentAction::Browse);
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut p = picker();
        for _ in 0..20 {
            p.step(Direction::Up);
        }
        assert_eq!(p.cursor(), 0);
        assert!(!p.step(Direction::Up));
        for _ in 0..20 {
            p.step(Direction::Down);
        }
        assert_eq!(p.cursor(), p.rows().len() - 1);
        assert!(!p.step(Direction::Down));
    }

    #[test]
    fn sideways_does_nothing_in_a_single_column() {
        let mut p = picker();
        assert!(!p.step(Direction::Left));
        assert!(!p.step(Direction::Right));
    }

    #[test]
    fn a_selection_that_no_longer_exists_does_not_leave_the_cursor_out_of_bounds() {
        // A file can be deleted between one opening and the next. Falling back to the top is
        // fine; indexing past the end is not.
        let mut p = picker();
        p.set_entries(
            vec![entry("Blank", EnvironmentChoice::Blank)],
            EnvironmentChoice::File(7),
        );
        assert_eq!(p.cursor(), 0);
        assert!(p.rows().iter().all(|r| !r.in_use));
    }
}
