//! Choosing a file, without a file-chooser service.
//!
//! The desktop's own chooser is a portal, and a host is often a machine with no desktop
//! session to provide one — and even with one, it would open on the host's screen, which is
//! not where the person choosing is looking. So this is a plain list: folders first, then what
//! can be chosen, one click to go in and one to pick.

use std::path::{Path, PathBuf};

use eframe::egui;

pub enum Outcome {
    Browsing,
    Chosen(PathBuf),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wants {
    /// Anything that is not a folder, programs first in mind.
    Files,
    Images,
    Folders,
}

pub struct Browser {
    dir: PathBuf,
    wants: Wants,
    listing: Vec<(String, bool)>,
    hidden: bool,
    typed: String,
}

impl Browser {
    pub fn files(start: PathBuf) -> Browser {
        Browser::new(start, Wants::Files)
    }

    pub fn images(start: PathBuf) -> Browser {
        Browser::new(start, Wants::Images)
    }

    pub fn folders(start: PathBuf) -> Browser {
        Browser::new(start, Wants::Folders)
    }

    fn new(start: PathBuf, wants: Wants) -> Browser {
        let mut browser = Browser {
            dir: PathBuf::new(),
            wants,
            listing: Vec::new(),
            hidden: false,
            typed: String::new(),
        };
        browser.go(start);
        browser
    }

    fn go(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.typed = self.dir.to_string_lossy().to_string();
        self.refresh();
    }

    fn refresh(&mut self) {
        let mut listing: Vec<(String, bool)> = std::fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with('.') && !self.hidden {
                            return None;
                        }
                        // Follows links, so a link to a folder is a folder.
                        let is_dir = e.path().is_dir();
                        let wanted = is_dir
                            || match self.wants {
                                Wants::Folders => false,
                                Wants::Files => true,
                                Wants::Images => is_image(&e.path()),
                            };
                        wanted.then_some((name, is_dir))
                    })
                    .collect()
            })
            .unwrap_or_default();
        listing.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        self.listing = listing;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, title: &str) -> Outcome {
        let mut outcome = Outcome::Browsing;
        ui.heading(title);
        ui.horizontal(|ui| {
            if ui.button("⬆ Up").clicked() {
                if let Some(parent) = self.dir.parent() {
                    self.go(parent.to_path_buf());
                }
            }
            if ui.button("Home").clicked() {
                self.go(crate::home());
            }
            let field = ui.add(egui::TextEdit::singleline(&mut self.typed).desired_width(480.0));
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = PathBuf::from(self.typed.trim());
                if path.is_dir() {
                    self.go(path);
                } else if path.exists() && self.wants != Wants::Folders {
                    outcome = Outcome::Chosen(path);
                }
            }
            if ui.checkbox(&mut self.hidden, "hidden").changed() {
                self.refresh();
            }
        });
        ui.horizontal(|ui| {
            if self.wants == Wants::Folders && ui.button("Use this folder").clicked() {
                outcome = Outcome::Chosen(self.dir.clone());
            }
            if ui.button("Cancel").clicked() {
                outcome = Outcome::Cancelled;
            }
        });
        ui.separator();
        let mut enter = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            if self.listing.is_empty() {
                ui.label(egui::RichText::new("Nothing here to choose.").weak());
            }
            for (name, is_dir) in &self.listing {
                let label = if *is_dir {
                    format!("📁  {name}")
                } else {
                    format!("     {name}")
                };
                let button = egui::Button::new(egui::RichText::new(label).size(15.0))
                    .frame(false)
                    .min_size(egui::vec2(ui.available_width(), 28.0));
                if ui.add(button).clicked() {
                    enter = Some((self.dir.join(name), *is_dir));
                }
            }
        });
        if let Some((path, is_dir)) = enter {
            if is_dir {
                self.go(path);
            } else {
                outcome = Outcome::Chosen(path);
            }
        }
        outcome
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "png" | "svg" | "ico" | "jpg" | "jpeg" | "bmp" | "xpm"))
}
