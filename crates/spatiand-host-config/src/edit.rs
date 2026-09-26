//! One application's settings, as a form.

use eframe::egui;
use spatiand_host_catalog::{icons, import};
use spatiand_stream::{App, AppKind, AudioMode, Detach, Eyes, PadProfile};

use crate::files::Browser;
use crate::Config;

/// An entry being edited. Kept apart from the catalogue until Save, so Cancel means cancel.
pub struct Editing {
    app: App,
    /// Where it sits in the catalogue, or `None` for a new one.
    index: Option<usize>,
    args: String,
    env: String,
    workdir: String,
    icon: String,
    browsing: Option<(Target, Browser)>,
    problem: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Program,
    Folder,
    Icon,
}

impl Editing {
    pub fn new(app: App, index: Option<usize>) -> Editing {
        Editing {
            args: join_args(&app.args),
            env: app
                .env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n"),
            workdir: app.workdir.clone().unwrap_or_default(),
            icon: app.icon.clone().unwrap_or_default(),
            app,
            index,
            browsing: None,
            problem: None,
        }
    }

    /// Close the file chooser, if it is open. False when there was none to close.
    pub fn close_chooser(&mut self) -> bool {
        self.browsing.take().is_some()
    }

    /// The form as an entry, or what is wrong with it.
    fn finish(&self, config: &Config) -> Result<App, String> {
        let mut app = self.app.clone();
        app.name = app.name.trim().to_string();
        app.exec = app.exec.trim().to_string();
        app.id = import::make_id(&app.id);
        if app.name.is_empty() {
            return Err("It needs a name.".into());
        }
        if app.exec.is_empty() {
            return Err("It needs a program to run.".into());
        }
        if icons::resolve_program(&app.exec).is_none() {
            return Err(format!("There is no program at {}.", app.exec));
        }
        let clash = config
            .catalog
            .apps
            .iter()
            .enumerate()
            .any(|(i, a)| a.id == app.id && Some(i) != self.index);
        if clash || app.id == spatiand_host_catalog::SETTINGS_APP_ID {
            return Err(format!("Another application is already called {}.", app.id));
        }
        app.args = spatiand_platform::launch::split_command(&self.args);
        app.env = Vec::new();
        for line in self.env.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match line.split_once('=') {
                Some((k, v)) if !k.trim().is_empty() => {
                    app.env.push((k.trim().to_string(), v.to_string()))
                }
                _ => return Err(format!("\"{line}\" is not NAME=value.")),
            }
        }
        let workdir = self.workdir.trim();
        app.workdir = (!workdir.is_empty()).then(|| workdir.to_string());
        let icon = self.icon.trim();
        app.icon = (!icon.is_empty()).then(|| icon.to_string());
        app.icon_png = None;
        Ok(app)
    }
}

/// Arguments back into one line, quoting the ones that need it, so that splitting it again
/// gives the same list.
fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn show(config: &mut Config, ui: &mut egui::Ui) {
    let Some(mut editing) = config.editing.take() else { return };

    // Choosing a file is a page of its own: a list of folders is too tall to share one.
    if let Some((target, browser)) = editing.browsing.as_mut() {
        let title = match target {
            Target::Program => "Choose the program",
            Target::Folder => "Choose the folder it runs in",
            Target::Icon => "Choose an icon",
        };
        match browser.show(ui, title) {
            crate::files::Outcome::Chosen(path) => {
                let text = path.to_string_lossy().to_string();
                match target {
                    Target::Program => {
                        editing.app.exec = text;
                        // A program in its own folder usually wants to run there.
                        if editing.workdir.is_empty() {
                            if let Some(dir) = path.parent() {
                                editing.workdir = dir.to_string_lossy().to_string();
                            }
                        }
                        if editing.app.name == "New app" {
                            if let Some(stem) = path.file_stem() {
                                editing.app.name = stem.to_string_lossy().to_string();
                                editing.app.id = import::unique_id(&editing.app.name, &config.catalog);
                            }
                        }
                    }
                    Target::Folder => editing.workdir = text,
                    Target::Icon => editing.icon = text,
                }
                editing.browsing = None;
            }
            crate::files::Outcome::Cancelled => editing.browsing = None,
            crate::files::Outcome::Browsing => {}
        }
        config.editing = Some(editing);
        return;
    }

    ui.heading(if editing.index.is_some() {
        format!("Edit {}", editing.app.name)
    } else {
        "Add an application".into()
    });
    ui.add_space(6.0);

    let mut done: Option<bool> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("edit")
            .num_columns(2)
            .spacing([16.0, 12.0])
            .min_col_width(170.0)
            .show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut editing.app.name).desired_width(420.0));
                ui.end_row();

                ui.label("Program");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut editing.app.exec).desired_width(420.0));
                    if ui.button("Browse…").clicked() {
                        let start = std::path::Path::new(&editing.app.exec)
                            .parent()
                            .filter(|p| p.is_dir())
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(crate::home);
                        editing.browsing = Some((Target::Program, Browser::files(start)));
                    }
                });
                ui.end_row();

                ui.label("Arguments");
                ui.add(
                    egui::TextEdit::singleline(&mut editing.args)
                        .desired_width(420.0)
                        .hint_text("--flag \"a value with spaces\""),
                );
                ui.end_row();

                ui.label("Runs in the folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut editing.workdir)
                            .desired_width(420.0)
                            .hint_text("where it is started from"),
                    );
                    if ui.button("Browse…").clicked() {
                        let start = std::path::PathBuf::from(&editing.workdir);
                        let start = if start.is_dir() { start } else { crate::home() };
                        editing.browsing = Some((Target::Folder, Browser::folders(start)));
                    }
                });
                ui.end_row();

                ui.label("Environment");
                ui.add(
                    egui::TextEdit::multiline(&mut editing.env)
                        .desired_width(420.0)
                        .desired_rows(2)
                        .hint_text("NAME=value, one per line"),
                );
                ui.end_row();

                ui.label("Icon");
                ui.horizontal(|ui| {
                    let preview = App {
                        icon: (!editing.icon.trim().is_empty()).then(|| editing.icon.clone()),
                        ..editing.app.clone()
                    };
                    let ctx = ui.ctx().clone();
                    match config.icon(&ctx, &preview) {
                        Some(texture) => {
                            ui.add(egui::Image::new((texture.id(), egui::vec2(48.0, 48.0))));
                        }
                        None => {
                            ui.add_sized([48.0, 48.0], egui::Label::new("none"));
                        }
                    }
                    ui.vertical(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut editing.icon)
                                .desired_width(320.0)
                                .hint_text("found automatically"),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Choose a picture…").clicked() {
                                let start = std::path::Path::new(&editing.app.exec)
                                    .parent()
                                    .filter(|p| p.is_dir())
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_else(crate::home);
                                editing.browsing = Some((Target::Icon, Browser::images(start)));
                            }
                            if ui.button("Find automatically").clicked() {
                                editing.icon.clear();
                            }
                        });
                        let entries = config.entries.get_or_insert_with(spatiand_platform::scan);
                        let found = icons::find(&preview, entries);
                        ui.label(
                            egui::RichText::new(match &found {
                                Some(f) => format!("{} ({})", f.path.display(), f.source.label()),
                                None => "No picture found; the headset shows its initial.".into(),
                            })
                            .weak()
                            .size(11.0),
                        );
                    });
                });
                ui.end_row();

                ui.label("Shown as");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut editing.app.kind, AppKind::Window, "A window");
                    ui.selectable_value(&mut editing.app.kind, AppKind::Vr, "The whole view (VR)");
                });
                ui.end_row();

                ui.label("Picture");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut editing.app.eyes, Eyes::Mono, "Flat");
                    ui.selectable_value(&mut editing.app.eyes, Eyes::SideBySide, "3D side by side");
                    ui.selectable_value(&mut editing.app.eyes, Eyes::TopBottom, "3D top and bottom");
                });
                ui.end_row();

                ui.label("Sound");
                egui::ComboBox::from_id_salt("audio")
                    .selected_text(audio_label(editing.app.audio))
                    .show_ui(ui, |ui| {
                        for mode in [
                            AudioMode::Auto,
                            AudioMode::Stereo,
                            AudioMode::Surround51,
                            AudioMode::Surround71,
                            AudioMode::Surround714,
                            AudioMode::Ambisonic,
                        ] {
                            ui.selectable_value(&mut editing.app.audio, mode, audio_label(mode));
                        }
                    });
                ui.end_row();

                ui.label("Controller");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut editing.app.pad, PadProfile::Xbox, "Gamepad");
                    ui.selectable_value(
                        &mut editing.app.pad,
                        PadProfile::Ndof8,
                        "8 axes (Second Life viewers)",
                    );
                });
                ui.end_row();

                ui.label("When the headset leaves");
                ui.horizontal(|ui| {
                    let slowed = matches!(editing.app.detach, Detach::Throttle { .. });
                    if ui.selectable_label(!slowed, "Keep running").clicked() {
                        editing.app.detach = Detach::Run;
                    }
                    if ui.selectable_label(slowed, "Slow it down").clicked() && !slowed {
                        editing.app.detach = Detach::Throttle { fps: 5 };
                    }
                    if let Detach::Throttle { fps } = &mut editing.app.detach {
                        ui.add(egui::Slider::new(fps, 1..=30).suffix(" fps"));
                    }
                });
                ui.end_row();

                ui.label("Id");
                ui.vertical(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut editing.app.id).desired_width(240.0));
                    ui.label(
                        egui::RichText::new(
                            "What the headset knows it by. Its controller layout is kept under \
                             this, so changing it starts the layout afresh.",
                        )
                        .weak()
                        .size(11.0),
                    );
                });
                ui.end_row();
            });

        ui.add_space(10.0);
        if let Some(problem) = &editing.problem {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 90), problem);
        }
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new("Save").min_size(egui::vec2(110.0, 34.0))).clicked() {
                done = Some(true);
            }
            if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(110.0, 34.0))).clicked() {
                done = Some(false);
            }
        });
    });

    match done {
        Some(true) => match editing.finish(config) {
            Ok(app) => {
                let name = app.name.clone();
                match editing.index {
                    Some(i) if i < config.catalog.apps.len() => config.catalog.apps[i] = app,
                    _ => config.catalog.apps.push(app),
                }
                config.save_catalog();
                config.status = format!("Saved {name}. The headset's launcher updates in a moment.");
                config.textures.clear();
            }
            Err(problem) => {
                editing.problem = Some(problem);
                config.editing = Some(editing);
            }
        },
        Some(false) => {}
        None => config.editing = Some(editing),
    }
}

fn audio_label(mode: AudioMode) -> &'static str {
    match mode {
        AudioMode::Auto => "As the application chooses",
        AudioMode::Stereo => "Stereo",
        AudioMode::Surround51 => "5.1",
        AudioMode::Surround71 => "7.1",
        AudioMode::Surround714 => "7.1.4",
        AudioMode::Ambisonic => "Ambisonic",
    }
}
