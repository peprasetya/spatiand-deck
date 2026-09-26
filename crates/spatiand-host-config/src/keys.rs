//! Driving the settings with keys: the arrows, Enter and Escape.
//!
//! Those are what the headset sends for the D-pad, A and B -- Spatiand's own layout types them
//! into whatever window is focused -- so this is how the settings are used without aiming the
//! pointer at every button, the same way Spatiand's own menus are.
//!
//! egui does most of it already: the arrows move focus to the nearest widget in that
//! direction, and Enter or Space presses the focused one. What it leaves out:
//!
//! * **The first press.** The arrows only move focus *from* something, and nothing has focus
//!   when a window opens, so they did nothing at all. The first arrow or Enter now puts focus
//!   on the current page in the list on the left, and the next one moves from there.
//! * **Back.** Escape only drops focus in egui. Here it goes back a step, as B does in
//!   Spatiand: out of the file chooser, out of the editor, and from a page to the page list.
//!   In a text field it only leaves the field, which is what Escape means there.
//! * **Seeing it.** A focused button is drawn as if pressed, which through the glasses is easy
//!   to miss, so a ring goes round it while the keys are in use.
//! * **Following it.** Focus moving down a long list scrolls the list with it.

use eframe::egui;

#[derive(Default)]
pub struct Keys {
    /// Focus should land on the current page next frame. A frame late on purpose: landing in
    /// the frame the arrow was pressed would let egui move it one step on straight away.
    land_next: bool,
    land_now: bool,
    /// What had focus at the end of the last frame. Escape has already cleared it by the time
    /// this frame can ask.
    last_focus: Option<egui::Id>,
    /// Whether a text field had the keyboard last frame, where Escape means "leave the field".
    was_typing: bool,
    /// The keys are being used rather than the pointer: the ring is shown, and focus is
    /// followed by scrolling.
    keyboard: bool,
    /// The widget last scrolled into view, so it is scrolled to once rather than held there
    /// against the wheel.
    followed: Option<egui::Id>,
    /// The page list's buttons, as drawn this frame.
    pub page_buttons: Vec<egui::Id>,
    /// Whether focus was on one of them at the end of the last frame.
    was_on_list: bool,
}

impl Keys {
    /// At the start of a frame. True when Escape asks to go back a step.
    pub fn begin(&mut self, ctx: &egui::Context) -> bool {
        use egui::Key;
        let (arrow, enter, escape, tab, pointer) = ctx.input(|i| {
            (
                [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight]
                    .into_iter()
                    .any(|k| i.key_pressed(k)),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::Tab),
                i.pointer.any_pressed(),
            )
        });
        if arrow || enter || escape || tab {
            self.keyboard = true;
            // Focus moves at the end of this frame; draw the frame after, where it is seen.
            ctx.request_repaint();
        }
        if pointer {
            self.keyboard = false;
        }

        self.land_now = std::mem::take(&mut self.land_next);
        let focused = ctx.memory(|m| m.focused());
        if focused.is_none() && (arrow || enter) {
            self.land_next = true;
        }
        if self.keyboard && focused.is_some() && focused != self.followed {
            if let Some(response) = focused.and_then(|id| ctx.read_response(id)) {
                response.scroll_to_me(None);
            }
            self.followed = focused;
        }
        // Read against last frame's list before it is drawn again.
        self.was_on_list = self.last_focus.is_some_and(|id| self.page_buttons.contains(&id));
        self.page_buttons.clear();
        escape && !self.was_typing
    }

    /// Put focus on the current page next frame.
    pub fn land_soon(&mut self) {
        self.land_next = true;
    }

    /// Whether the current page's button should take focus now. Asked by that button.
    pub fn take_landing(&mut self) -> bool {
        std::mem::take(&mut self.land_now)
    }

    /// Whether focus was in the page list when Escape was pressed.
    pub fn was_on_page_list(&self) -> bool {
        self.was_on_list
    }

    /// At the end of a frame: remember where focus is, and ring it.
    pub fn end(&mut self, ctx: &egui::Context) {
        let focused = ctx.memory(|m| m.focused());
        self.last_focus = focused;
        // A text field, not merely something focused: egui's `wants_keyboard_input` is true
        // for a focused button too. A text field is the one thing that keeps a state of its own.
        self.was_typing = focused.is_some_and(|id| egui::text_edit::TextEditState::load(ctx, id).is_some());
        if !self.keyboard {
            return;
        }
        let Some(response) = focused.and_then(|id| ctx.read_response(id)) else {
            return;
        };
        // Drawn over everything, so it is not hidden under the next widget, and around what is
        // visible of the widget, so a half-scrolled one is not ringed outside its list.
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("keyboard focus"),
        ));
        painter.rect_stroke(
            response.interact_rect.expand(2.0),
            6.0_f32,
            egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(110, 185, 255)),
            egui::StrokeKind::Outside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page list of two and one button on the page, run for one frame with these keys down.
    fn frame(ctx: &egui::Context, keys: &mut Keys, pressed: &[egui::Key]) -> (bool, [egui::Id; 3]) {
        let events = pressed
            .iter()
            .map(|&key| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .collect();
        let input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            ..Default::default()
        };
        let mut back = false;
        let mut ids = [egui::Id::NULL; 3];
        let _ = ctx.run(input, |ctx| {
            back = keys.begin(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                for (i, label) in ["Applications", "Network"].into_iter().enumerate() {
                    let response = ui.button(label);
                    keys.page_buttons.push(response.id);
                    // The first is the current page.
                    if i == 0 && keys.take_landing() {
                        response.request_focus();
                    }
                    ids[i] = response.id;
                }
                ids[2] = ui.button("Save").id;
            });
            keys.end(ctx);
        });
        (back, ids)
    }

    fn focused(ctx: &egui::Context) -> Option<egui::Id> {
        ctx.memory(|m| m.focused())
    }

    #[test]
    fn the_first_arrow_lands_on_the_current_page_and_the_next_moves() {
        let ctx = egui::Context::default();
        let mut keys = Keys::default();
        frame(&ctx, &mut keys, &[]);
        assert_eq!(focused(&ctx), None, "nothing has focus in a new window");
        frame(&ctx, &mut keys, &[egui::Key::ArrowDown]);
        let (_, ids) = frame(&ctx, &mut keys, &[]);
        assert_eq!(focused(&ctx), Some(ids[0]), "landed, and not moved on by the same press");
        frame(&ctx, &mut keys, &[egui::Key::ArrowDown]);
        let (_, ids) = frame(&ctx, &mut keys, &[]);
        assert_eq!(focused(&ctx), Some(ids[1]), "the next press moves");
    }

    #[test]
    fn escape_on_a_page_goes_back_and_on_the_list_does_not() {
        let ctx = egui::Context::default();
        let mut keys = Keys::default();
        let (_, ids) = frame(&ctx, &mut keys, &[]);
        ctx.memory_mut(|m| m.request_focus(ids[2]));
        frame(&ctx, &mut keys, &[]);
        let (back, _) = frame(&ctx, &mut keys, &[egui::Key::Escape]);
        assert!(back, "Escape from a page's button asks to go back");
        assert!(!keys.was_on_page_list());

        ctx.memory_mut(|m| m.request_focus(ids[0]));
        frame(&ctx, &mut keys, &[]);
        let (back, _) = frame(&ctx, &mut keys, &[egui::Key::Escape]);
        assert!(back);
        assert!(keys.was_on_page_list(), "already on the list: nowhere further back");
    }

    #[test]
    fn escape_in_a_text_field_only_leaves_it() {
        let ctx = egui::Context::default();
        let mut keys = Keys::default();
        let mut text = String::new();
        let mut run = |keys: &mut Keys, pressed: &[egui::Key], focus: bool| {
            let events = pressed
                .iter()
                .map(|&key| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .collect();
            let mut back = false;
            let _ = ctx.run(egui::RawInput { events, ..Default::default() }, |ctx| {
                back = keys.begin(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    let r = ui.text_edit_singleline(&mut text);
                    if focus {
                        r.request_focus();
                    }
                });
                keys.end(ctx);
            });
            back
        };
        run(&mut keys, &[], true);
        run(&mut keys, &[], false);
        assert!(!run(&mut keys, &[egui::Key::Escape], false));
    }
}
