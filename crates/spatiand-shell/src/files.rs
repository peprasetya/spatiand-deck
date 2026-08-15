//! Finding a file without leaving the headset.
//!
//! This is a file browser rather than a call out to KDE's open dialog, and that was a real
//! choice. `kdialog` would have been fewer lines, but it means: a dependency on KDE for
//! something core rather than for something deep, a subprocess whose stdout has to be drained
//! without blocking a render loop, and a 2D dialog floating somewhere in a 3D room being driven
//! by a touchpad. A one-column list that the D-pad already knows how to walk avoids all three,
//! and it works on a machine with no desktop environment at all — which is the portability rule
//! the rest of the shell is held to.
//!
//! The cost is honest: this browser can do nothing but walk directories and pick a file. No
//! typing a path, no search, no thumbnails. That is enough for "the panorama I just downloaded"
//! and not enough for much else.
//!
//! Like the rest of the shell it holds no paths — only names, which the compositor resolves
//! against the directory it last listed.

use crate::grid::Direction;

/// The row that climbs out of the current directory. Always present, always first, so "go up"
/// is in the same place everywhere including in a directory that is otherwise empty.
pub const PARENT_LABEL: &str = "..";

/// One line of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_directory: bool,
}

/// What pressing A on a row means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    /// Descend into this directory, or climb with [`PARENT_LABEL`].
    Enter(String),
    /// Take this file.
    Choose(String),
}

/// A one-column directory listing.
#[derive(Debug, Clone, Default)]
pub struct FileBrowser {
    /// Where we are, for the heading. Display only — the compositor owns the real path.
    directory: String,
    entries: Vec<FileEntry>,
    cursor: usize,
}

impl FileBrowser {
    /// Show a directory. The cursor returns to the top, which is right: you have just moved
    /// somewhere new and there is no previous position that means anything.
    pub fn show(&mut self, directory: String, entries: Vec<FileEntry>) {
        self.directory = directory;
        self.entries = entries;
        self.cursor = 0;
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Every line, the leading `..` included.
    pub fn rows(&self) -> Vec<FileEntry> {
        let mut rows = vec![FileEntry {
            name: PARENT_LABEL.into(),
            is_directory: true,
        }];
        rows.extend(self.entries.iter().cloned());
        rows
    }

    pub fn step(&mut self, direction: Direction) -> bool {
        let before = self.cursor;
        match direction {
            Direction::Up => self.cursor = self.cursor.saturating_sub(1),
            Direction::Down => {
                if self.cursor + 1 < self.entries.len() + 1 {
                    self.cursor += 1;
                }
            }
            Direction::Left | Direction::Right => {}
        }
        self.cursor != before
    }

    /// What pressing A does. Row 0 is always the parent.
    pub fn activate(&self) -> FileAction {
        let Some(entry) = self.cursor.checked_sub(1).and_then(|i| self.entries.get(i)) else {
            return FileAction::Enter(PARENT_LABEL.into());
        };
        if entry.is_directory {
            FileAction::Enter(entry.name.clone())
        } else {
            FileAction::Choose(entry.name.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_directory: true,
        }
    }

    fn file(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_directory: false,
        }
    }

    fn browser() -> FileBrowser {
        let mut b = FileBrowser::default();
        b.show(
            "/home/deck/Pictures".into(),
            vec![dir("Panoramas"), file("sunset.jpg"), file("forest.png")],
        );
        b
    }

    #[test]
    fn going_up_is_always_the_first_row() {
        // Including in an empty directory, which is precisely where you most need it and
        // where a listing-derived row would not exist.
        let mut b = FileBrowser::default();
        b.show("/tmp/empty".into(), vec![]);
        assert_eq!(b.rows().len(), 1);
        assert_eq!(b.activate(), FileAction::Enter(PARENT_LABEL.into()));
    }

    #[test]
    fn a_directory_is_entered_and_a_file_is_taken() {
        // The one distinction the browser exists to make. Getting it backwards would try to
        // load a directory as a panorama.
        let mut b = browser();
        b.step(Direction::Down);
        assert_eq!(b.activate(), FileAction::Enter("Panoramas".into()));
        b.step(Direction::Down);
        assert_eq!(b.activate(), FileAction::Choose("sunset.jpg".into()));
    }

    #[test]
    fn moving_somewhere_new_puts_the_cursor_back_at_the_top() {
        let mut b = browser();
        b.step(Direction::Down);
        b.step(Direction::Down);
        b.show("/home/deck".into(), vec![file("a.jpg")]);
        assert_eq!(b.cursor(), 0);
        assert_eq!(b.directory(), "/home/deck");
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut b = browser();
        assert!(!b.step(Direction::Up));
        for _ in 0..20 {
            b.step(Direction::Down);
        }
        assert_eq!(b.cursor(), b.rows().len() - 1);
        assert!(!b.step(Direction::Down));
    }

    #[test]
    fn a_shorter_listing_does_not_strand_the_cursor_past_the_end() {
        // Walking into a directory with fewer things in it than the one you left.
        let mut b = browser();
        for _ in 0..3 {
            b.step(Direction::Down);
        }
        b.show("/tmp".into(), vec![]);
        assert_eq!(b.cursor(), 0);
        assert_eq!(b.activate(), FileAction::Enter(PARENT_LABEL.into()));
    }
}
