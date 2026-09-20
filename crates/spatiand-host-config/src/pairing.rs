//! Pairing a headset from here instead of from a terminal.
//!
//! The same conversation `spatiand-host --pair` has with the running host, over the same
//! socket; see the host's `control.rs`. Being able to do it here means a second headset can be
//! paired by the person wearing the first, with nobody at a shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;

use crate::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Asking,
    Open(u64),
    Compare { who: String, code: String },
    Answered,
    Over { paired: bool, message: String },
}

pub struct Pairing {
    stage: Stage,
    lines: Receiver<String>,
    writer: UnixStream,
}

impl Pairing {
    fn start() -> Result<Pairing, String> {
        let stream = UnixStream::connect(spatiand_host_catalog::control_socket_path())
            .map_err(|_| "The host is not running, so it cannot pair.".to_string())?;
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
        writeln!(writer, "pair").map_err(|e| e.to_string())?;
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
            let _ = tx.send("gone".into());
        });
        Ok(Pairing {
            stage: Stage::Asking,
            lines,
            writer,
        })
    }

    /// Take in whatever the host said. `true` if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(line) = self.lines.try_recv() {
            changed = true;
            let mut words = line.split_whitespace();
            self.stage = match words.next() {
                Some("open") => Stage::Open(words.next().and_then(|s| s.parse().ok()).unwrap_or(120)),
                Some("code") => {
                    let who = words.next().unwrap_or("?").to_string();
                    let code = words.collect::<Vec<_>>().join(" ");
                    Stage::Compare { who, code }
                }
                Some("paired") => Stage::Over {
                    paired: true,
                    message: "Paired. The headset can connect from now on without asking."
                        .into(),
                },
                Some("refused") => Stage::Over {
                    paired: false,
                    message: "Refused. Nothing was written down.".into(),
                },
                Some("closed") => Stage::Over {
                    paired: false,
                    message: "Nobody came in two minutes, so pairing closed again.".into(),
                },
                Some("busy") => Stage::Over {
                    paired: false,
                    message: "Another pairing is already under way on this host.".into(),
                },
                Some("gone") if !matches!(self.stage, Stage::Over { .. }) => Stage::Over {
                    paired: false,
                    message: "The host went away.".into(),
                },
                _ => continue,
            };
        }
        changed
    }

    fn answer(&mut self, yes: bool) {
        let _ = writeln!(self.writer, "{}", if yes { "yes" } else { "no" });
        self.stage = Stage::Answered;
    }
}

pub fn page(config: &mut Config, ui: &mut egui::Ui) {
    ui.heading("Pair a headset");
    let host = crate::hostname();
    let stage = config.pairing.as_ref().map(|p| p.stage.clone());
    match stage {
        None | Some(Stage::Over { .. }) => {
            if let Some(Stage::Over { paired, message }) = &stage {
                let colour = if *paired {
                    egui::Color32::from_rgb(120, 210, 140)
                } else {
                    egui::Color32::from_rgb(230, 170, 90)
                };
                ui.colored_label(colour, message);
                ui.add_space(10.0);
            }
            ui.label(
                "Let another headset use this computer. It is asked once; after that it connects \
                 by itself.",
            );
            ui.add_space(10.0);
            if ui
                .add(egui::Button::new("Start pairing").min_size(egui::vec2(180.0, 38.0)))
                .clicked()
            {
                match Pairing::start() {
                    Ok(p) => config.pairing = Some(p),
                    Err(e) => config.status = e,
                }
            }
        }
        Some(Stage::Asking) => {
            ui.label("Asking the host…");
        }
        Some(Stage::Open(seconds)) => {
            ui.label(format!("Pairing is open for {seconds} seconds."));
            ui.add_space(8.0);
            ui.label("On the headset: Settings → Remote computers → Add a computer, and type");
            ui.label(egui::RichText::new(&host).size(28.0).strong());
            ui.label("or this computer's address.");
            ui.add_space(8.0);
            if ui.button("Stop").clicked() {
                config.pairing = None;
            }
        }
        Some(Stage::Compare { who, code }) => {
            ui.label(format!("A headset ({who}) is asking to pair. Does it show this code?"));
            ui.add_space(8.0);
            ui.label(egui::RichText::new(&code).size(52.0).strong().monospace());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Yes, they match").min_size(egui::vec2(180.0, 40.0)))
                    .clicked()
                {
                    if let Some(p) = config.pairing.as_mut() {
                        p.answer(true);
                    }
                }
                if ui
                    .add(egui::Button::new("No").min_size(egui::vec2(100.0, 40.0)))
                    .clicked()
                {
                    if let Some(p) = config.pairing.as_mut() {
                        p.answer(false);
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "If they differ, something else is answering in the headset's place. Say no.",
                )
                .weak(),
            );
        }
        Some(Stage::Answered) => {
            ui.label("Answered; waiting for the host…");
        }
    }
}
