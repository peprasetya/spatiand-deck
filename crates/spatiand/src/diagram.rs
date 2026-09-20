//! The picture beside the layout editor: a Steam Deck and the glasses, with what every control
//! does written beside it, the way Steam's configurator shows its controller.
//!
//! Drawn with tiny-skia into one image, rebuilt only when a label changes. Shapes are laid out on
//! a fixed canvas and scaled; labels are set in two columns, left controls on the left and right
//! controls on the right, each joined to its control by a line.
//!
//! Wide and short on purpose. The picture hangs above the editor's card, and what a headset is
//! short of is vertical field: at 1800 x 700 the pair fits where a squarer picture beside the
//! card was simply off the side of the view.

use spatiand_mapper::editor::Callout;
use spatiand_mapper::{Button, Group};
use spatiand_render::{TextImage, TextRenderer};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub const WIDTH: f32 = 1800.0;
pub const HEIGHT: f32 = 700.0;

const LABEL_EM: f32 = 30.0;
const LABEL_SPACING: f32 = 44.0;
const LEFT_COLUMN: f32 = 400.0;
const RIGHT_COLUMN: f32 = 1400.0;

const PLATE: [u8; 4] = [10, 12, 18, 215];
const BODY: [u8; 4] = [40, 44, 54, 255];
const OUTLINE: [u8; 4] = [120, 130, 150, 255];
const CONTROL: [u8; 4] = [72, 78, 94, 255];
const BOUND: [u8; 4] = [86, 142, 230, 255];
const LINE: [u8; 4] = [150, 162, 186, 170];
const INK: [u8; 4] = [232, 238, 250, 255];

#[derive(Clone, Copy)]
enum Shape {
    Circle(f32),
    Rect(f32, f32),
}

/// Where each control is drawn, and as what.
fn spot(callout: Callout) -> (f32, f32, Shape) {
    use Button as B;
    match callout {
        Callout::Button(b) => match b {
            B::A => (1280.0, 275.0, Shape::Circle(17.0)),
            B::B => (1316.0, 240.0, Shape::Circle(17.0)),
            B::X => (1244.0, 240.0, Shape::Circle(17.0)),
            B::Y => (1280.0, 205.0, Shape::Circle(17.0)),
            B::DpadUp => (520.0, 210.0, Shape::Rect(22.0, 26.0)),
            B::DpadDown => (520.0, 270.0, Shape::Rect(22.0, 26.0)),
            B::DpadLeft => (490.0, 240.0, Shape::Rect(26.0, 22.0)),
            B::DpadRight => (550.0, 240.0, Shape::Rect(26.0, 22.0)),
            B::L1 => (560.0, 108.0, Shape::Rect(170.0, 16.0)),
            B::R1 => (1240.0, 108.0, Shape::Rect(170.0, 16.0)),
            B::L2 => (560.0, 78.0, Shape::Rect(150.0, 24.0)),
            B::R2 => (1240.0, 78.0, Shape::Rect(150.0, 24.0)),
            B::L4 => (436.0, 300.0, Shape::Rect(12.0, 44.0)),
            B::L5 => (436.0, 390.0, Shape::Rect(12.0, 44.0)),
            B::R4 => (1364.0, 300.0, Shape::Rect(12.0, 44.0)),
            B::R5 => (1364.0, 390.0, Shape::Rect(12.0, 44.0)),
            B::View => (600.0, 165.0, Shape::Circle(11.0)),
            B::Menu => (1200.0, 165.0, Shape::Circle(11.0)),
            B::LStick => (640.0, 240.0, Shape::Circle(16.0)),
            B::RStick => (1160.0, 240.0, Shape::Circle(16.0)),
            // A ring around the stick's cap: touching it is the whole top of the stick, and
            // it has to be distinguishable from the click at its centre.
            B::LStickTouch => (640.0, 240.0, Shape::Circle(30.0)),
            B::RStickTouch => (1160.0, 240.0, Shape::Circle(30.0)),
            B::LPadTouch => (585.0, 420.0, Shape::Circle(6.0)),
            B::RPadTouch => (1215.0, 420.0, Shape::Circle(6.0)),
            B::GlassesUp => (1045.0, 610.0, Shape::Circle(9.0)),
            B::GlassesDown => (1045.0, 645.0, Shape::Circle(9.0)),
        },
        Callout::Group(g) => match g {
            Group::LeftStick => (640.0, 240.0, Shape::Circle(44.0)),
            Group::RightStick => (1160.0, 240.0, Shape::Circle(44.0)),
            Group::LeftTrigger => (560.0, 78.0, Shape::Rect(150.0, 24.0)),
            Group::RightTrigger => (1240.0, 78.0, Shape::Rect(150.0, 24.0)),
            Group::Gyro => (900.0, 480.0, Shape::Circle(22.0)),
            Group::GlassesGyro => (900.0, 628.0, Shape::Circle(14.0)),
        },
    }
}

/// Which column a control's label goes in.
fn on_left(callout: Callout) -> bool {
    match callout {
        Callout::Group(Group::Gyro) => true,
        Callout::Group(Group::GlassesGyro) => false,
        other => spot(other).0 < WIDTH / 2.0,
    }
}

fn paint(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    p.anti_alias = true;
    p
}

fn rounded(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
}

fn fill_rounded(pixmap: &mut Pixmap, t: Transform, rect: (f32, f32, f32, f32), r: f32, rgba: [u8; 4]) {
    let mut pb = PathBuilder::new();
    rounded(&mut pb, rect.0, rect.1, rect.2, rect.3, r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint(rgba), FillRule::Winding, t, None);
    }
}

fn stroke_rounded(pixmap: &mut Pixmap, t: Transform, rect: (f32, f32, f32, f32), r: f32, width: f32, rgba: [u8; 4]) {
    let mut pb = PathBuilder::new();
    rounded(&mut pb, rect.0, rect.1, rect.2, rect.3, r);
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint(rgba), &stroke, t, None);
    }
}

fn draw_shape(pixmap: &mut Pixmap, t: Transform, (x, y, shape): (f32, f32, Shape), rgba: [u8; 4]) {
    match shape {
        Shape::Circle(r) => {
            if let Some(path) = PathBuilder::from_circle(x, y, r) {
                pixmap.fill_path(&path, &paint(rgba), FillRule::Winding, t, None);
            }
        }
        Shape::Rect(w, h) => fill_rounded(pixmap, t, (x - w / 2.0, y - h / 2.0, w, h), 6.0, rgba),
    }
}

/// Draw the picture with these labels, `scale` times the canvas size.
pub fn render(text: &mut TextRenderer, callouts: &[(Callout, String)], scale: f32) -> TextImage {
    let width = (WIDTH * scale) as u32;
    let height = (HEIGHT * scale) as u32;
    let Some(mut pixmap) = Pixmap::new(width.max(1), height.max(1)) else {
        return TextImage {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        };
    };
    let t = Transform::from_scale(scale, scale);

    fill_rounded(&mut pixmap, t, (0.0, 0.0, WIDTH, HEIGHT), 48.0, PLATE);

    // The Deck.
    fill_rounded(&mut pixmap, t, (430.0, 120.0, 940.0, 460.0), 130.0, BODY);
    stroke_rounded(&mut pixmap, t, (430.0, 120.0, 940.0, 460.0), 130.0, 3.0, OUTLINE);
    fill_rounded(&mut pixmap, t, (700.0, 175.0, 400.0, 255.0), 16.0, [14, 16, 22, 255]);
    // The trackpads, STEAM and the ... button: drawn, never labelled. None of them is a
    // layout's to take -- the pads are the pointer in every application, and the two buttons
    // open Spatiand's own menus.
    for pad_x in [585.0, 1215.0] {
        fill_rounded(&mut pixmap, t, (pad_x - 65.0, 375.0, 130.0, 130.0), 20.0, CONTROL);
    }
    draw_shape(&mut pixmap, t, (505.0, 532.0, Shape::Circle(15.0)), OUTLINE);
    draw_shape(&mut pixmap, t, (1295.0, 532.0, Shape::Circle(15.0)), OUTLINE);

    // The glasses, below the Deck, where there is room left over.
    for lens_x in [770.0, 910.0] {
        fill_rounded(&mut pixmap, t, (lens_x, 600.0, 120.0, 56.0), 22.0, BODY);
        stroke_rounded(&mut pixmap, t, (lens_x, 600.0, 120.0, 56.0), 22.0, 3.0, OUTLINE);
    }
    fill_rounded(&mut pixmap, t, (888.0, 613.0, 24.0, 10.0), 4.0, OUTLINE);
    fill_rounded(&mut pixmap, t, (1028.0, 604.0, 24.0, 48.0), 8.0, BODY);

    let bound = |c: Callout| callouts.iter().any(|(k, _)| *k == c);
    // Groups first, so the buttons drawn on top of them (stick clicks, pad clicks) stay visible.
    for group in Group::ALL {
        let c = Callout::Group(group);
        draw_shape(&mut pixmap, t, spot(c), if bound(c) { BOUND } else { CONTROL });
    }
    for button in Button::ALL {
        let c = Callout::Button(button);
        let (x, y, shape) = spot(c);
        let tint = if bound(c) { BOUND } else { CONTROL };
        match (button, shape) {
            (Button::LStick | Button::RStick | Button::LPadTouch | Button::RPadTouch, _)
                if !bound(c) => {}
            _ => draw_shape(&mut pixmap, t, (x, y, shape), tint),
        }
    }

    // Straight alpha from here on: tiny-skia keeps premultiplied pixels, and the labels are
    // composited by hand.
    let mut rgba = pixmap.data().to_vec();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }

    for left in [true, false] {
        let mut column: Vec<(Callout, &str, f32, f32)> = callouts
            .iter()
            .filter(|(c, _)| on_left(*c) == left)
            .map(|(c, label)| {
                let (x, y, _) = spot(*c);
                (*c, label.as_str(), x, y)
            })
            .collect();
        column.sort_by(|a, b| a.3.total_cmp(&b.3));
        let mut next_free = 40.0f32;
        for (_, label, x, y) in column {
            let label_y = y.max(next_free);
            if label_y > HEIGHT - 30.0 {
                break;
            }
            next_free = label_y + LABEL_SPACING;
            let end_x = if left { LEFT_COLUMN + 8.0 } else { RIGHT_COLUMN - 8.0 };
            line(&mut rgba, width, height, scale, (end_x, label_y), (x, y), LINE);
            let image = text.render(label, LABEL_EM * scale, (360.0 * scale) as u32, INK);
            let img_x = if left {
                LEFT_COLUMN * scale - image.width as f32
            } else {
                RIGHT_COLUMN * scale
            };
            let img_y = label_y * scale - image.height as f32 / 2.0;
            blit(&mut rgba, width, height, &image, img_x as i32, img_y as i32);
        }
    }

    TextImage {
        width,
        height,
        rgba,
    }
}

/// A one-pixel-wide line with a soft edge, straight onto straight-alpha pixels.
fn line(rgba: &mut [u8], width: u32, height: u32, scale: f32, from: (f32, f32), to: (f32, f32), color: [u8; 4]) {
    let (x0, y0) = (from.0 * scale, from.1 * scale);
    let (x1, y1) = (to.0 * scale, to.1 * scale);
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()) as usize).max(1);
    for i in 0..=steps {
        let f = i as f32 / steps as f32;
        let x = (x0 + (x1 - x0) * f).round() as i32;
        let y = (y0 + (y1 - y0) * f).round() as i32;
        for (dx, dy) in [(0, 0), (0, 1)] {
            put(rgba, width, height, x + dx, y + dy, color);
        }
    }
}

fn put(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let i = (y as usize * width as usize + x as usize) * 4;
    over(&mut rgba[i..i + 4], color);
}

fn blit(rgba: &mut [u8], width: u32, height: u32, image: &TextImage, x0: i32, y0: i32) {
    for y in 0..image.height as i32 {
        for x in 0..image.width as i32 {
            let (tx, ty) = (x0 + x, y0 + y);
            if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                continue;
            }
            let s = ((y as usize * image.width as usize) + x as usize) * 4;
            let src = [image.rgba[s], image.rgba[s + 1], image.rgba[s + 2], image.rgba[s + 3]];
            if src[3] == 0 {
                continue;
            }
            let d = (ty as usize * width as usize + tx as usize) * 4;
            over(&mut rgba[d..d + 4], src);
        }
    }
}

/// Porter-Duff "over" on straight alpha.
fn over(dst: &mut [u8], src: [u8; 4]) {
    let sa = src[3] as f32 / 255.0;
    let da = dst[3] as f32 / 255.0;
    let out = sa + da * (1.0 - sa);
    if out <= 0.0 {
        return;
    }
    for c in 0..3 {
        let v = (src[c] as f32 * sa + dst[c] as f32 * da * (1.0 - sa)) / out;
        dst[c] = v.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_is_drawn_inside_the_canvas() {
        let all = Button::ALL
            .iter()
            .map(|b| Callout::Button(*b))
            .chain(Group::ALL.iter().map(|g| Callout::Group(*g)));
        for c in all {
            let (x, y, _) = spot(c);
            assert!((0.0..WIDTH).contains(&x) && (0.0..HEIGHT).contains(&y), "{c:?}");
        }
    }

    #[test]
    fn left_hand_controls_are_labelled_on_the_left() {
        assert!(on_left(Callout::Group(Group::LeftStick)));
        assert!(on_left(Callout::Button(Button::DpadUp)));
        assert!(!on_left(Callout::Button(Button::A)));
        assert!(!on_left(Callout::Group(Group::RightStick)));
    }

    #[test]
    fn over_leaves_the_background_where_the_ink_is_clear() {
        let mut px = [10, 20, 30, 255];
        over(&mut px, [255, 255, 255, 0]);
        assert_eq!(px, [10, 20, 30, 255]);
        over(&mut px, [255, 255, 255, 255]);
        assert_eq!(px, [255, 255, 255, 255]);
    }
}
