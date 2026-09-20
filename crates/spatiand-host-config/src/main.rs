//! `spatiand-host-config` — the host's settings, as an app the headset opens.
//!
//! Every host offers this first, before anything has been configured, because it is how
//! anything gets configured: which applications the headset can open, how each one runs, how
//! much of the network the host may use, and pairing another headset. It edits the same two
//! files the host reads (`apps.toml` and `host.toml`); the host notices each save within a
//! second and the headset's launcher follows.
//!
//! It is drawn for being used through the glasses — large type, big targets, one thing per
//! page — with the trackpad pointer and the on-screen keyboard. It also runs on the host's own
//! desktop, where it is simply a settings window.

mod edit;
mod files;
mod pairing;

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use spatiand_host_catalog::{icons, import, Settings};
use spatiand_stream::{App, Catalog};

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Host settings")
            .with_app_id("spatiand-host-config")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Host settings",
        options,
        Box::new(|cc| Ok(Box::new(Config::new(&cc.egui_ctx)))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Apps,
    Running,
    Import,
    Network,
    Pair,
    About,
}

pub struct Config {
    page: Page,
    catalog: Catalog,
    settings: Settings,
    /// A problem reading the files, shown until the next successful save.
    trouble: Option<String>,
    /// One line about the last thing that happened.
    status: String,
    /// The entry being edited, if any, and where it came from.
    editing: Option<edit::Editing>,
    /// Installed applications, read when the import page is first opened.
    entries: Option<Vec<spatiand_platform::DesktopEntry>>,
    filter: String,
    /// Icons already turned into textures, keyed by what they were made from.
    textures: HashMap<String, Option<egui::TextureHandle>>,
    pairing: Option<pairing::Pairing>,
    settings_draft: Settings,
    fingerprint: String,
    /// What the host last said is open: window id, app id, title.
    running: Vec<(u32, String, String)>,
    /// Restart pressed once; the second press does it.
    restart_armed: bool,
}

impl Config {
    fn new(ctx: &egui::Context) -> Config {
        // Readable through the glasses: a window of 1280x800 fills about 40 degrees, and
        // egui's default 14-pixel text is too small to read at that size.
        ctx.set_zoom_factor(1.6);
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(10.0, 10.0);
            style.spacing.button_padding = egui::vec2(12.0, 6.0);
            style.spacing.interact_size.y = 30.0;
        });
        let mut trouble = None;
        let catalog = spatiand_host_catalog::load_catalog(&spatiand_host_catalog::catalog_path())
            .unwrap_or_else(|e| {
                trouble = Some(e);
                Catalog::default()
            });
        let settings =
            spatiand_host_catalog::load_settings(&spatiand_host_catalog::settings_path())
                .unwrap_or_else(|e| {
                    trouble = Some(e);
                    Settings::default()
                });
        let fingerprint = spatiand_stream::Identity::load_or_create(&spatiand_host_catalog::config_dir())
            .map(|i| i.fingerprint().to_string())
            .unwrap_or_else(|e| format!("unknown ({e})"));
        Config {
            page: Page::Apps,
            catalog,
            settings,
            trouble,
            status: String::new(),
            editing: None,
            entries: None,
            filter: String::new(),
            textures: HashMap::new(),
            pairing: None,
            settings_draft: settings,
            fingerprint,
            running: Vec::new(),
            restart_armed: false,
        }
    }

    fn save_catalog(&mut self) {
        match spatiand_host_catalog::save_catalog(
            &spatiand_host_catalog::catalog_path(),
            &self.catalog,
        ) {
            Ok(()) => {
                self.trouble = None;
                self.status = "Saved. The headset's launcher updates in a moment.".into();
            }
            Err(e) => self.status = format!("Could not save: {e}"),
        }
    }

    /// The icon for an entry as the host would find it, as a texture.
    fn icon(&mut self, ctx: &egui::Context, app: &App) -> Option<egui::TextureHandle> {
        let key = format!("{}\u{1f}{}\u{1f}{}", app.id, app.exec, app.icon.clone().unwrap_or_default());
        if let Some(cached) = self.textures.get(&key) {
            return cached.clone();
        }
        let entries = self.entries.get_or_insert_with(spatiand_platform::scan);
        let texture = icons::icon_png(app, entries).and_then(|png| texture_from_png(ctx, &key, &png));
        self.textures.insert(key, texture.clone());
        texture
    }

    /// Ask the running host to start an entry, the way the headset does.
    fn open_on_headset(&mut self, id: &str) {
        self.status = match send_to_host(&format!("launch {id}")) {
            Ok(reply) if reply.starts_with("launched") => {
                format!("Started {id}; it opens as a window in the headset.")
            }
            Ok(reply) => reply.trim_start_matches("failed ").to_string(),
            Err(e) => e,
        };
    }
}

pub fn texture_from_png(ctx: &egui::Context, key: &str, png: &[u8]) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(key, color, egui::TextureOptions::LINEAR))
}

/// One command to the running host, and every line of its answer up to `end`.
fn ask_host(line: &str) -> Result<Vec<String>, String> {
    use std::io::{BufRead, BufReader, Write};
    let path = spatiand_host_catalog::control_socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|_| "The host is not running.".to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for reply in BufReader::new(stream).lines() {
        let reply = reply.map_err(|e| format!("The host did not answer: {e}"))?;
        if reply == "end" {
            break;
        }
        lines.push(reply);
    }
    Ok(lines)
}

/// One command to the running host, and its first answer.
fn send_to_host(line: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    let path = spatiand_host_catalog::control_socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|_| "The host is not running, so nothing can be started from here.".to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| format!("The host did not answer: {e}"))?;
    Ok(reply.trim().to_string())
}

impl eframe::App for Config {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(p) = self.pairing.as_mut() {
            if p.poll() {
                ctx.request_repaint();
            }
            // A pairing only changes when a line arrives; checking a few times a second is
            // plenty and keeps the window idle — an idle window costs the headset nothing.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::SidePanel::left("pages")
            .resizable(false)
            .exact_width(170.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Host");
                ui.add_space(8.0);
                for (page, label) in [
                    (Page::Apps, "Applications"),
                    (Page::Running, "Running"),
                    (Page::Import, "Add installed"),
                    (Page::Network, "Network"),
                    (Page::Pair, "Pair a headset"),
                    (Page::About, "About"),
                ] {
                    let chosen = self.page == page && self.editing.is_none();
                    let button = egui::Button::new(egui::RichText::new(label).size(15.0))
                        .selected(chosen)
                        .min_size(egui::vec2(150.0, 34.0));
                    if ui.add(button).clicked() {
                        self.page = page;
                        self.editing = None;
                        self.status.clear();
                        self.restart_armed = false;
                        if page == Page::Running {
                            self.refresh_running();
                        }
                    }
                }
            });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            if let Some(trouble) = &self.trouble {
                ui.colored_label(egui::Color32::from_rgb(230, 120, 90), trouble);
            }
            ui.label(if self.status.is_empty() { " " } else { &self.status });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.editing.is_some() {
                edit::show(self, ui);
                return;
            }
            match self.page {
                Page::Apps => self.apps_page(ui),
                Page::Running => self.running_page(ui),
                Page::Import => self.import_page(ui),
                Page::Network => self.network_page(ui),
                Page::Pair => pairing::page(self, ui),
                Page::About => self.about_page(ui),
            }
        });
    }
}

impl Config {
    fn apps_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Applications");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Add by hand").clicked() {
                    let mut app = App::new(import::unique_id("app", &self.catalog), "New app", "");
                    app.args.clear();
                    self.editing = Some(edit::Editing::new(app, None));
                }
                if ui.button("Add installed").clicked() {
                    self.page = Page::Import;
                }
            });
        });
        ui.label("What the headset can open from this computer. Changes reach it when saved.");
        ui.add_space(6.0);
        if self.catalog.apps.is_empty() {
            ui.add_space(20.0);
            ui.label("Nothing yet. Add an installed application, or one by hand.");
            return;
        }
        let ctx = ui.ctx().clone();
        let apps = self.catalog.apps.clone();
        let mut remove = None;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            for (index, app) in apps.iter().enumerate() {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        match self.icon(&ctx, app) {
                            Some(texture) => {
                                ui.add(egui::Image::new((texture.id(), egui::vec2(40.0, 40.0))));
                            }
                            None => {
                                ui.add_sized([40.0, 40.0], egui::Label::new(
                                    egui::RichText::new(initial(&app.name)).size(24.0),
                                ));
                            }
                        }
                        // The buttons get a fixed share on the right and the text the rest,
                        // truncated to fit — measured, not left to a right-to-left layout,
                        // which let a long command line push the buttons out of the window.
                        let text_width = (ui.available_width() - BUTTONS_WIDTH).max(60.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(text_width, 46.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.set_max_width(text_width);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&app.name).size(17.0).strong(),
                                    )
                                    .truncate(),
                                );
                                ui.add(
                                    egui::Label::new(egui::RichText::new(describe(app)).weak())
                                        .truncate(),
                                );
                            },
                        );
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 46.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.button("Remove").clicked() {
                                    remove = Some(index);
                                }
                                if ui.button("Edit").clicked() {
                                    self.editing = Some(edit::Editing::new(app.clone(), Some(index)));
                                }
                                if ui.button("Open").clicked() {
                                    self.open_on_headset(&app.id);
                                }
                            },
                        );
                    });
                });
            }
        });
        if let Some(index) = remove {
            let gone = self.catalog.apps.remove(index);
            self.save_catalog();
            self.status = format!("Removed {}. Anything of it already open keeps running.", gone.name);
        }
    }

    fn refresh_running(&mut self) {
        match ask_host("list") {
            Ok(lines) => {
                self.running = lines
                    .iter()
                    .filter_map(|l| {
                        let rest = l.strip_prefix("window ")?;
                        let (id, rest) = rest.split_once(' ')?;
                        let (app, title) = rest.split_once(' ').unwrap_or((rest, ""));
                        Some((id.parse().ok()?, app.to_string(), title.to_string()))
                    })
                    .collect();
            }
            Err(e) => self.status = e,
        }
    }

    fn running_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Running");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    self.refresh_running();
                }
            });
        });
        ui.label("Everything open on this computer for the headset. Close asks politely, the way a window's own close button does; Force quit ends the application and everything it started, unsaved work included.");
        ui.add_space(6.0);
        if self.running.is_empty() {
            ui.label(egui::RichText::new("Nothing is open.").weak());
        }
        let mut refresh = false;
        for (window, app, title) in self.running.clone() {
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let text_width = (ui.available_width() - BUTTONS_WIDTH).max(60.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(text_width, 34.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_max_width(text_width);
                            let shown = if title.is_empty() { app.clone() } else { title.clone() };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{shown}   ({app})")).size(16.0),
                                )
                                .truncate(),
                            );
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 34.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button("Force quit").clicked() {
                                self.status = match send_to_host(&format!("kill {app}")) {
                                    Ok(r) if r == "done" => format!("{app} was ended."),
                                    Ok(r) => r.trim_start_matches("failed ").to_string(),
                                    Err(e) => e,
                                };
                                refresh = true;
                            }
                            if ui.button("Close").clicked() {
                                self.status = match send_to_host(&format!("close {window}")) {
                                    Ok(r) if r == "done" => format!("Asked {app} to close."),
                                    Ok(r) => r.trim_start_matches("failed ").to_string(),
                                    Err(e) => e,
                                };
                                refresh = true;
                            }
                        },
                    );
                });
            });
        }
        if refresh {
            // A window takes a moment to go; the list is read again after it has.
            std::thread::sleep(std::time::Duration::from_millis(300));
            self.refresh_running();
        }

        ui.add_space(20.0);
        ui.separator();
        ui.label("Restarting the host closes every application it is running, this window included. It is back a few seconds later; open this again from the launcher.");
        let label = if self.restart_armed { "Press again to restart" } else { "Restart the host" };
        if ui.add(egui::Button::new(label).min_size(egui::vec2(200.0, 36.0))).clicked() {
            if self.restart_armed {
                if let Err(e) = send_to_host("restart") {
                    self.status = e;
                }
                self.restart_armed = false;
            } else {
                self.restart_armed = true;
            }
        }
    }

    fn import_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Add an installed application");
        ui.label("Everything this computer's menus know about. Adding one copies its command and icon; edit it afterwards to change either.");
        ui.horizontal(|ui| {
            ui.label("Find:");
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(300.0));
        });
        let entries = self.entries.get_or_insert_with(spatiand_platform::scan).clone();
        let candidates = import::candidates(&entries, &self.catalog);
        let filter = self.filter.to_lowercase();
        let ctx = ui.ctx().clone();
        let mut add = None;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            for candidate in candidates
                .iter()
                .filter(|c| filter.is_empty() || c.app.name.to_lowercase().contains(&filter))
            {
                ui.horizontal(|ui| {
                    // One height for every row, so a row with a picture and one without line
                    // their buttons up.
                    ui.set_min_height(38.0);
                    match self.icon(&ctx, &candidate.app) {
                        Some(texture) => {
                            ui.add(egui::Image::new((texture.id(), egui::vec2(32.0, 32.0))));
                        }
                        None => {
                            ui.add_sized([32.0, 32.0], egui::Label::new(initial(&candidate.app.name)));
                        }
                    }
                    let text_width = (ui.available_width() - 110.0).max(60.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(text_width, 38.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_max_width(text_width);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&candidate.app.name).size(16.0),
                                )
                                .truncate(),
                            );
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 38.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if candidate.already {
                                ui.label(egui::RichText::new("added").weak());
                            } else if ui.button("Add").clicked() {
                                add = Some(candidate.app.clone());
                            }
                        },
                    );
                });
            }
        });
        if let Some(mut app) = add {
            app.id = import::unique_id(&app.id, &self.catalog);
            let name = app.name.clone();
            self.catalog.apps.push(app);
            self.save_catalog();
            self.status = format!("Added {name}.");
        }
    }

    fn network_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Network");
        ui.label("How much this computer may send to the headset. A window that is not changing sends nothing at all, whatever these say.");
        ui.add_space(10.0);
        let draft = &mut self.settings_draft;
        egui::Grid::new("network").num_columns(2).spacing([20.0, 14.0]).show(ui, |ui| {
            ui.label("Most it may use");
            let mut mbit = draft.max_kbit as f32 / 1000.0;
            if ui
                .add(egui::Slider::new(&mut mbit, 2.0..=200.0).logarithmic(true).suffix(" Mbit/s"))
                .changed()
            {
                draft.max_kbit = (mbit * 1000.0).round() as u32;
            }
            ui.end_row();
            ui.label("");
            ui.label(egui::RichText::new("About 25 on 5 GHz Wi-Fi or Tailscale, 100 or more on a cable. Shared between the windows that are open.").weak());
            ui.end_row();

            ui.label("Most pictures a second");
            ui.add(egui::Slider::new(&mut draft.max_fps, 10..=120).suffix(" fps"));
            ui.end_row();

            ui.label("A still window drops to");
            ui.add(egui::Slider::new(&mut draft.idle_fps, 0..=30).suffix(" fps"));
            ui.end_row();

            ui.label("…after being still for");
            ui.add(egui::Slider::new(&mut draft.idle_after_ms, 50..=5000).suffix(" ms"));
            ui.end_row();

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut draft.port).range(1024..=65535));
            ui.end_row();
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let changed = self.settings_draft != self.settings;
            if ui.add_enabled(changed, egui::Button::new("Save")).clicked() {
                match spatiand_host_catalog::save_settings(
                    &spatiand_host_catalog::settings_path(),
                    &self.settings_draft,
                ) {
                    Ok(()) => {
                        let port_moved = self.settings_draft.port != self.settings.port;
                        self.settings = self.settings_draft;
                        self.status = if port_moved {
                            "Saved. The new port is used when the host next starts.".into()
                        } else {
                            "Saved. Every open window restarts its stream at the new rate.".into()
                        };
                    }
                    Err(e) => self.status = format!("Could not save: {e}"),
                }
            }
            if ui.add_enabled(changed, egui::Button::new("Undo")).clicked() {
                self.settings_draft = self.settings;
            }
            if ui.button("Defaults").clicked() {
                self.settings_draft = Settings::default();
            }
        });
    }

    fn about_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("About this host");
        egui::Grid::new("about").num_columns(2).spacing([20.0, 10.0]).show(ui, |ui| {
            ui.label("Computer");
            ui.label(hostname());
            ui.end_row();
            ui.label("Its fingerprint");
            ui.label(egui::RichText::new(&self.fingerprint).monospace().size(12.0));
            ui.end_row();
            ui.label("Applications");
            ui.label(spatiand_host_catalog::catalog_path().display().to_string());
            ui.end_row();
            ui.label("Settings");
            ui.label(spatiand_host_catalog::settings_path().display().to_string());
            ui.end_row();
        });
    }
}

/// Room kept on the right of a row for its buttons.
const BUTTONS_WIDTH: f32 = 250.0;

fn describe(app: &App) -> String {
    let mut out = app.exec.clone();
    if !app.args.is_empty() {
        out.push(' ');
        out.push_str(&app.args.join(" "));
    }
    let mut tags = Vec::new();
    if app.kind == spatiand_stream::AppKind::Vr {
        tags.push("VR");
    }
    if app.eyes != spatiand_stream::Eyes::Mono {
        tags.push("stereo");
    }
    if !tags.is_empty() {
        out = format!("{}  ·  {out}", tags.join(", "));
    }
    out
}

fn initial(name: &str) -> String {
    name.chars().next().unwrap_or('?').to_uppercase().to_string()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "this computer".into())
}

/// Home, for the file browser to start in.
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}
