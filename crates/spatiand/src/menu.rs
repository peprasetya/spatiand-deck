//! What the open menu says.
//!
//! One description of a menu — a title, a column of rows, an explanation, a line of hints —
//! that every menu in the shell is expressed as. The scene knows how to draw *that* and
//! nothing about settings, environments or directories, which is why adding a fourth menu is
//! a function here rather than a branch in the renderer.
//!
//! This replaced a formatted string. The string carried its own selection marker, `\u{25b8}`,
//! baked into the text — which meant the whole panel had to be rasterised and re-uploaded
//! every time the cursor moved a row, and the panel's width depended on which row was
//! selected, because the marker is wider than the spaces standing in for it. Describing the
//! selection as *data* lets the renderer draw it as a shape, so moving the cursor now costs no
//! uploads at all and cannot change the layout.

use spatiand_shell::{files::PARENT_LABEL, Mode, Shell};

/// One line of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    pub label: String,
    /// Set to the right of the label, dimmer: a status rather than a name. Whether an
    /// environment is the one you are inside, whether a listing entry is a folder.
    pub trailing: Option<String>,
}

impl MenuRow {
    fn plain(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            trailing: None,
        }
    }

    fn with(label: impl Into<String>, trailing: &str) -> Self {
        Self {
            label: label.into(),
            trailing: Some(trailing.into()),
        }
    }
}

/// A menu, ready to be laid out and drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuModel {
    pub title: String,
    pub rows: Vec<MenuRow>,
    pub cursor: usize,
    /// One paragraph about whatever is selected, or about where you are.
    pub detail: String,
    /// Which buttons do what.
    pub footer: String,
}

impl MenuModel {
    /// Everything except the cursor, which is what decides whether the textures need rebuilding.
    ///
    /// The cursor is deliberately excluded: it moves constantly and changes nothing that was
    /// rasterised, since the selection is drawn as a shape. The detail *is* included, because
    /// in the settings list it is the focused row's explanation and does change with the cursor.
    fn content_key(&self) -> String {
        let mut key = format!("{}\u{1f}{}\u{1f}", self.title, self.footer);
        for row in &self.rows {
            key.push_str(&row.label);
            key.push('\u{1e}');
            key.push_str(row.trailing.as_deref().unwrap_or(""));
            key.push('\u{1e}');
        }
        key.push('\u{1f}');
        key.push_str(&self.detail);
        key
    }

    /// True when nothing rasterised for `other` can be reused for this.
    pub fn differs_from(&self, other: &Self) -> bool {
        self.content_key() != other.content_key()
    }
}

/// The menu the shell currently has open, if any.
///
/// `None` means there is nothing to draw: the world, or a launcher, which is a grid of glass
/// bubbles rather than a card and draws itself.
pub fn model(shell: &Shell) -> Option<MenuModel> {
    match shell.mode() {
        Mode::World => None,
        Mode::Hud => {
            let hud = shell.hud();
            Some(MenuModel {
                title: "Settings".into(),
                rows: hud.items().iter().map(|i| MenuRow::plain(i.label)).collect(),
                cursor: hud.cursor(),
                detail: hud.focused().detail.into(),
                footer: FOOTER_SELECT.into(),
            })
        }
        Mode::Environment => {
            let picker = shell.environments();
            Some(MenuModel {
                title: "Environment".into(),
                rows: picker
                    .rows()
                    .iter()
                    .map(|r| {
                        if r.in_use {
                            // Spelt out rather than marked with a glyph. A tick has to survive
                            // being read through optics at an angle, and a font that has no
                            // tick in it substitutes a box.
                            MenuRow::with(r.label, "In use")
                        } else {
                            MenuRow::plain(r.label)
                        }
                    })
                    .collect(),
                cursor: picker.cursor(),
                detail: "Pick what surrounds you. The last row opens a folder to add your own \
                         panoramic image."
                    .into(),
                footer: FOOTER_SELECT.into(),
            })
        }
        Mode::Files => {
            let files = shell.files();
            let rows = files.rows();
            let empty = rows.len() == 1;
            Some(MenuModel {
                title: "Add an image".into(),
                rows: rows
                    .iter()
                    .map(|r| {
                        if r.name == PARENT_LABEL {
                            // "Go up" rather than the two dots the shell uses internally. The
                            // dots are a convention from a command line nobody is looking at.
                            MenuRow::plain("Go up")
                        } else if r.is_directory {
                            MenuRow::with(&r.name, "Folder")
                        } else {
                            MenuRow::plain(&r.name)
                        }
                    })
                    .collect(),
                cursor: files.cursor(),
                // Where you are, not what the row does. In a browser the question is always
                // "which folder is this", and the answer does not fit in a row.
                detail: if empty {
                    // Otherwise an empty folder looks exactly like a folder that failed to open.
                    format!("{}\nNo folders or images here.", files.directory())
                } else {
                    files.directory().into()
                },
                footer: "A open    B back".into(),
            })
        }
        Mode::Launcher if shell.launcher().is_empty() => Some(MenuModel {
            title: "No applications".into(),
            rows: Vec::new(),
            cursor: 0,
            detail: "Nothing was found in the system's application folders.".into(),
            footer: "B back".into(),
        }),
        Mode::Launcher => None,
    }
}

const FOOTER_SELECT: &str = "A select    B back";

#[cfg(test)]
mod tests {
    use super::*;
    use spatiand_shell::grid::Direction;
    use spatiand_shell::hud::HudAction;
    use spatiand_shell::DesktopPanels;
    use spatiand_shell::Intent;

    /// Walk the settings list to the environment row and press A.
    ///
    /// By action rather than by position: the row moves whenever the HUD gains an entry, and a
    /// test that counts presses passes for a while and then silently opens something else.
    fn open_environments(shell: &mut Shell) {
        for _ in 0..shell.hud().items().len() {
            if shell.hud().activate() == HudAction::OpenEnvironments {
                break;
            }
            shell.handle(Intent::Navigate(Direction::Down));
        }
        assert_eq!(shell.hud().activate(), HudAction::OpenEnvironments);
        shell.handle(Intent::Accept);
    }

    fn hud_shell() -> Shell {
        let mut shell = Shell::new(Vec::new(), DesktopPanels::ALL);
        shell.handle(Intent::ToggleHud);
        shell
    }

    #[test]
    fn the_world_has_no_menu() {
        assert!(model(&Shell::new(Vec::new(), DesktopPanels::ALL)).is_none());
    }

    #[test]
    fn the_settings_menu_lists_every_row_and_explains_the_focused_one() {
        let shell = hud_shell();
        let m = model(&shell).expect("the hud is open");
        assert_eq!(m.rows.len(), shell.hud().items().len());
        assert_eq!(m.detail, shell.hud().focused().detail);
        assert!(!m.footer.is_empty());
    }

    #[test]
    fn moving_the_cursor_does_not_change_what_has_to_be_rasterised_except_the_detail() {
        // The point of the model: the selection is a shape the renderer draws, not a marker
        // baked into the text, so walking the list must not invalidate the rows.
        let mut shell = hud_shell();
        let before = model(&shell).unwrap();
        shell.handle(Intent::Navigate(Direction::Down));
        let after = model(&shell).unwrap();
        assert_ne!(before.cursor, after.cursor);
        assert_eq!(before.rows, after.rows, "the rows must be reusable");
        assert_eq!(before.title, after.title);
    }

    #[test]
    fn the_environment_in_use_is_marked_and_the_rest_are_not() {
        use spatiand_shell::environment::{EnvironmentChoice, EnvironmentEntry};
        let mut shell = hud_shell();
        open_environments(&mut shell);
        shell.set_environments(
            vec![
                EnvironmentEntry {
                    label: "Blank".into(),
                    choice: EnvironmentChoice::Blank,
                },
                EnvironmentEntry {
                    label: "Studio".into(),
                    choice: EnvironmentChoice::Studio,
                },
            ],
            EnvironmentChoice::Studio,
        );
        let m = model(&shell).expect("the picker is open");
        assert_eq!(m.title, "Environment");
        let marked: Vec<_> = m.rows.iter().filter(|r| r.trailing.is_some()).collect();
        assert_eq!(marked.len(), 1, "exactly one environment is in use");
        assert_eq!(marked[0].label, "Studio");
        // And the browse row is still the last one, unmarked.
        assert!(m.rows.last().unwrap().trailing.is_none());
    }

    #[test]
    fn a_directory_listing_says_which_rows_are_folders_and_which_way_is_up() {
        use spatiand_shell::files::FileEntry;
        let mut shell = hud_shell();
        open_environments(&mut shell);
        // Straight to the browse row, which is always last.
        for _ in 0..20 {
            shell.handle(Intent::Navigate(Direction::Down));
        }
        shell.handle(Intent::Accept);
        shell.show_directory(
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
        let m = model(&shell).expect("the browser is open");
        assert_eq!(m.rows[0].label, "Go up");
        assert_eq!(m.rows[1].trailing.as_deref(), Some("Folder"));
        assert_eq!(m.rows[2].trailing, None);
        assert!(m.detail.contains("/home/deck/Pictures"));
    }

    #[test]
    fn an_empty_folder_says_so_rather_than_looking_broken() {
        use spatiand_shell::files::FileEntry;
        let mut shell = hud_shell();
        open_environments(&mut shell);
        for _ in 0..20 {
            shell.handle(Intent::Navigate(Direction::Down));
        }
        shell.handle(Intent::Accept);
        shell.show_directory("/tmp/empty".into(), Vec::<FileEntry>::new());
        let m = model(&shell).unwrap();
        assert_eq!(m.rows.len(), 1, "only the way out");
        assert!(m.detail.contains("No folders or images"));
    }

    #[test]
    fn the_content_key_ignores_the_cursor_and_notices_everything_else() {
        let a = MenuModel {
            title: "Settings".into(),
            rows: vec![MenuRow::plain("One"), MenuRow::plain("Two")],
            cursor: 0,
            detail: "d".into(),
            footer: "f".into(),
        };
        let moved = MenuModel { cursor: 1, ..a.clone() };
        assert!(!a.differs_from(&moved), "a moved cursor rebuilds nothing");
        let renamed = MenuModel {
            rows: vec![MenuRow::plain("One"), MenuRow::with("Two", "In use")],
            ..a.clone()
        };
        assert!(a.differs_from(&renamed), "a new trailing label must rebuild");
        let explained = MenuModel {
            detail: "different".into(),
            ..a.clone()
        };
        assert!(a.differs_from(&explained));
    }
}
