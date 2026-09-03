//! Switching between open windows, the way every desktop does it.
//!
//! The gesture this is modelled on — Cmd-Tab, Alt-Tab — has one property that makes it worth
//! copying exactly: opening it already has the *next* window selected, so the common case
//! (there are two windows and you want the other one) is open-and-confirm rather than
//! open-move-confirm. Everything else here follows from that.
//!
//! Chosen over the arrangement it replaces, which was the shoulder bumpers stepping focus one
//! window at a time. That worked and had two costs: it consumed two buttons an application or
//! a game will want, and stepping blind through a ring of windows means looking at each one to
//! find out where you are. A list you can read before committing is strictly better, and it
//! gives the buttons back.
//!
//! What it does *not* do is move focus as the highlight moves. Focusing a window here also
//! brings it to the centre of the view, and doing that to every window you pass through on the
//! way would rearrange the room to reach one window.

use crate::grid::Direction;

/// One open window, as the switcher needs to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    /// The compositor's own identifier. Opaque here: the shell has no window handles.
    pub id: usize,
    pub title: String,
    /// True for the window that has focus right now.
    pub current: bool,
}

/// The switcher's state.
#[derive(Debug, Clone, Default)]
pub struct Switcher {
    entries: Vec<WindowEntry>,
    cursor: usize,
}

impl Switcher {
    /// Hand it the window list, which the compositor rebuilds each time this opens.
    ///
    /// The cursor lands on the window *after* the current one, wrapping — the Cmd-Tab rule.
    /// With one window it lands on that one, which is a switcher that does nothing, correctly.
    pub fn show(&mut self, entries: Vec<WindowEntry>) {
        let current = entries.iter().position(|e| e.current);
        self.cursor = match current {
            Some(i) if !entries.is_empty() => (i + 1) % entries.len(),
            _ => 0,
        };
        self.entries = entries;
    }

    pub fn entries(&self) -> &[WindowEntry] {
        &self.entries
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Move the highlight. **Wraps**, unlike the menus.
    ///
    /// A menu is a list with a first and a last row and stopping at the ends is what tells you
    /// you are at one. A switcher is a ring — you are cycling, and running out of windows at
    /// the bottom of the list is not information anyone wants.
    ///
    /// Left and right step as well as up and down, because this is drawn as a column but
    /// thought of as a row: every switcher anyone has used is horizontal.
    pub fn step(&mut self, direction: Direction) -> bool {
        if self.entries.len() < 2 {
            return false;
        }
        let n = self.entries.len();
        self.cursor = match direction {
            Direction::Up | Direction::Left => (self.cursor + n - 1) % n,
            Direction::Down | Direction::Right => (self.cursor + 1) % n,
        };
        true
    }

    /// Which window to switch to, or `None` when there are no windows at all.
    pub fn activate(&self) -> Option<usize> {
        self.entries.get(self.cursor).map(|e| e.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(n: usize, current: usize) -> Vec<WindowEntry> {
        (0..n)
            .map(|i| WindowEntry {
                id: i * 10,
                title: format!("window {i}"),
                current: i == current,
            })
            .collect()
    }

    #[test]
    fn opening_it_already_points_at_the_other_window() {
        // The whole point of the gesture: two windows, one press, confirm.
        let mut s = Switcher::default();
        s.show(windows(2, 0));
        assert_eq!(s.activate(), Some(10));
    }

    #[test]
    fn it_wraps_past_the_last_window() {
        let mut s = Switcher::default();
        s.show(windows(3, 2));
        // Current is the last, so it starts at the first.
        assert_eq!(s.cursor(), 0);
        assert!(s.step(Direction::Up));
        assert_eq!(s.cursor(), 2);
        assert!(s.step(Direction::Down));
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn a_single_window_is_a_switcher_that_does_nothing() {
        let mut s = Switcher::default();
        s.show(windows(1, 0));
        assert!(!s.step(Direction::Down));
        assert_eq!(s.activate(), Some(0));
    }

    #[test]
    fn with_nothing_open_there_is_nothing_to_switch_to() {
        let s = Switcher::default();
        assert!(s.is_empty());
        assert_eq!(s.activate(), None);
    }

    #[test]
    fn left_and_right_step_as_well_as_up_and_down() {
        // Drawn as a column, thought of as a row.
        let mut s = Switcher::default();
        s.show(windows(3, 0));
        assert_eq!(s.cursor(), 1);
        s.step(Direction::Right);
        assert_eq!(s.cursor(), 2);
        s.step(Direction::Left);
        assert_eq!(s.cursor(), 1);
    }
}
