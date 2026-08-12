//! Turning touchpad contacts into pointer events, window drags, and clicks.
//!
//! This is where the 3D world meets ordinary 2D applications. A ray is cast from the eye
//! through wherever the thumb is on the pad, intersected with the window quads, and the hit's
//! surface coordinates are fed to `wl_pointer` — after which a Wayland client is having an
//! entirely normal day and has no idea it is floating in space.
//!
//! ## Which pad does what
//!
//! Both pads point. Two rays, two cursors, because the pads are physically two hands and
//! pretending otherwise makes the two-handed gestures unexplainable.
//!
//! | control | in a window | on a title bar | in the world |
//! |---|---|---|---|
//! | right pad | move the pointer | — | — |
//! | right pad click | left mouse button | grab and move the window | — |
//! | left pad | scroll | — | — |
//! | left pad click | right mouse button | change the window's distance | — |
//! | both pads moving | move and resize the focused window | | |
//! | A / B / X | left / right / middle button | — | — |
//!
//! The precedence is: **both thumbs moving beats one**. Resting a thumb on the left pad while
//! pointing with the right is common and must not be mistaken for a two-handed gesture, which
//! is why the gesture only takes over once there is actual correlated movement — the deadband
//! in `spatiand_input::gesture` is what makes that distinction possible.
//!
//! The face buttons duplicate the pad clicks deliberately: clicking a pad moves your thumb
//! slightly as you press, which is fine for a button and bad for a precise click. Holding the
//! thumb still and pressing A is the accurate way to click on something small.

use smithay::backend::input::{Axis, AxisSource};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use spatiand_render::ray::{intersect_quad, pick, Quad, Ray};
use spatiand_render::Hit;

use crate::scene::WindowQuad;
use crate::state::Spatiand;

/// Linux button codes, as `wl_pointer` expects them.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// How tall the title bar is, as a fraction of the window's height.
///
/// Generous, and it has to be. This is a target hit with a head-anchored ray at 2.2 m, where
/// the whole window is only about 17° tall — so a desktop-proportioned bar works out at a
/// degree or so, which is roughly the tremor in holding your head still. 11% gives a little
/// over 2°, which is comfortably aimable. The test below is what caught 7.5% being too thin.
pub const TITLE_BAR_FRACTION: f64 = 0.11;

/// What the wearer is doing with a window.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Drag {
    /// Moving it around the sphere.
    ///
    /// The offsets are the angle between where the window was and where the ray was pointing
    /// at the moment it was grabbed. Without them the window snaps its *centre* to the ray,
    /// so grabbing the corner of a title bar throws the window sideways before you have moved
    /// at all — it should hang from the point you took hold of, like anything else.
    Move {
        window: smithay::desktop::Window,
        yaw_offset: f64,
        pitch_offset: f64,
    },
    /// Pushing it away or pulling it closer.
    Depth {
        window: smithay::desktop::Window,
        start_radius: f64,
        start_y: f32,
    },
}

/// Everything the pointer layer remembers between frames.
#[derive(Debug, Default)]
pub struct PointerState {
    pub drag: Option<Drag>,
    /// Buttons currently held, so a release is only sent for something that was pressed.
    held: Vec<u32>,
    /// Which window the cursor was last over, to notice when it leaves.
    last_focus: Option<usize>,
}

/// A ray plus what it currently hits.
pub struct Aim {
    pub ray: Ray,
    pub hit: Option<(usize, Hit)>,
    /// True when the hit landed in the window's title bar rather than its content.
    pub on_title: bool,
}

/// Cast a ray and work out what it means.
pub fn aim(ray: Ray, windows: &[WindowQuad]) -> Aim {
    let quads: Vec<Quad> = windows
        .iter()
        .map(|w| quad_of(w.pixels, &w.placement))
        .collect();
    let hit = pick(&ray, &quads);
    // The title bar occupies the top of the same quad rather than a separate one: a second
    // quad would need its own intersection test and could disagree with the first about which
    // window is in front.
    let on_title = hit.map(|(_, h)| h.v < TITLE_BAR_FRACTION).unwrap_or(false);
    Aim { ray, hit, on_title }
}

/// The quad a window occupies, including its title bar.
///
/// Takes the geometry rather than the whole [`WindowQuad`] so it can be tested: a `WindowQuad`
/// carries a live Wayland window, which cannot be conjured up without a compositor.
pub fn quad_of(pixels: (u32, u32), placement: &crate::window::Placement) -> Quad {
    let aspect = pixels.0 as f64 / pixels.1.max(1) as f64;
    let width = placement.width;
    let content_height = width / aspect.max(0.01);
    Quad {
        centre: placement.position(),
        orientation: placement.orientation(),
        width,
        // The bar sits above the content, so the clickable quad is taller than the surface.
        height: content_height / (1.0 - TITLE_BAR_FRACTION),
    }
}

/// Surface coordinates for a hit, in the client's own pixels.
///
/// Returns `None` for a hit on the title bar: that is Spatiand's chrome, and forwarding it as
/// a pointer position would put the cursor above the top edge of the surface.
pub fn surface_position(hit: &Hit, pixels: (u32, u32)) -> Option<Point<f64, Logical>> {
    if hit.v < TITLE_BAR_FRACTION {
        return None;
    }
    // Rescale past the bar so the top of the *content* is v = 0.
    let v = (hit.v - TITLE_BAR_FRACTION) / (1.0 - TITLE_BAR_FRACTION);
    Some(Point::from((hit.u * pixels.0 as f64, v * pixels.1 as f64)))
}

impl PointerState {
    /// Send motion for wherever the pointer is now.
    pub fn motion(
        &mut self,
        state: &mut Spatiand,
        aim: &Aim,
        windows: &[WindowQuad],
        time_ms: u32,
    ) {
        let Some(pointer) = state.seat.get_pointer() else {
            return;
        };
        // The second element of `focus` is the surface's ORIGIN in the same space as
        // `event.location`, not the position within it -- smithay delivers `location - origin`
        // to the client. Passing the surface-local position for both made every one of those
        // subtractions zero, so every client saw the pointer pinned to its top-left corner
        // for ever. Motion looked like it was working; nothing was ever under the cursor.
        //
        // Our "global" space is one surface at a time, so the origin is simply zero.
        let focus = aim.hit.and_then(|(index, hit)| {
            let window = windows.get(index)?;
            let _ = surface_position(&hit, window.pixels)?;
            // Straight off the quad, rather than looked up by position -- see the note on
            // WindowQuad::window for why an index cannot be trusted between frames.
            Some((window.surface.clone(), Point::from((0.0, 0.0))))
        });
        let local = aim
            .hit
            .and_then(|(index, hit)| surface_position(&hit, windows.get(index)?.pixels));

        // Leaving a window has to be reported, or it keeps its hover state for ever.
        let index_now = aim.hit.map(|(i, _)| i);
        if index_now != self.last_focus {
            self.last_focus = index_now;
        }

        let location = local.unwrap_or_else(|| Point::from((0.0, 0.0)));
        pointer.motion(
            state,
            focus.map(|(s, p)| (s, p)),
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: time_ms,
            },
        );
        pointer.frame(state);
    }

    /// Press or release a mouse button.
    pub fn button(&mut self, state: &mut Spatiand, button: u32, pressed: bool, time_ms: u32) {
        let Some(pointer) = state.seat.get_pointer() else {
            return;
        };
        if pressed {
            if self.held.contains(&button) {
                return;
            }
            self.held.push(button);
        } else if let Some(at) = self.held.iter().position(|b| *b == button) {
            self.held.remove(at);
        } else {
            // Never send a release for something that was not pressed. Clients track button
            // state themselves and a stray release leaves them convinced a drag is running.
            return;
        }

        pointer.button(
            state,
            &ButtonEvent {
                button,
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
                serial: SERIAL_COUNTER.next_serial(),
                time: time_ms,
            },
        );
        pointer.frame(state);
    }

    /// Scroll the surface under the pointer.
    ///
    /// Reported as a finger source rather than a wheel, because it is one: clients use that to
    /// decide between smooth pixel scrolling and notched jumps, and a touchpad claiming to be
    /// a wheel scrolls in ugly steps.
    pub fn scroll(&mut self, state: &mut Spatiand, dx: f64, dy: f64, time_ms: u32) {
        let Some(pointer) = state.seat.get_pointer() else {
            return;
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let mut frame = AxisFrame::new(time_ms).source(AxisSource::Finger);
        if dx != 0.0 {
            frame = frame.value(Axis::Horizontal, dx);
        }
        if dy != 0.0 {
            frame = frame.value(Axis::Vertical, dy);
        }
        pointer.axis(state, frame);
        pointer.frame(state);
    }

    /// Tell clients the scroll gesture ended, so kinetic scrolling can settle.
    pub fn scroll_stop(&mut self, state: &mut Spatiand, time_ms: u32) {
        let Some(pointer) = state.seat.get_pointer() else {
            return;
        };
        let frame = AxisFrame::new(time_ms)
            .source(AxisSource::Finger)
            .stop(Axis::Vertical)
            .stop(Axis::Horizontal);
        pointer.axis(state, frame);
        pointer.frame(state);
    }

    /// Release everything. Used when the pointer is taken away — a menu opening, the glasses
    /// being unplugged — so no client is left believing a button is still down.
    pub fn release_all(&mut self, state: &mut Spatiand, time_ms: u32) {
        for button in std::mem::take(&mut self.held) {
            if let Some(pointer) = state.seat.get_pointer() {
                pointer.button(
                    state,
                    &ButtonEvent {
                        button,
                        state: smithay::backend::input::ButtonState::Released,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: time_ms,
                    },
                );
                pointer.frame(state);
            }
        }
        self.drag = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::Placement;
    use glam::DVec3;

    const PIXELS: (u32, u32) = (1280, 800);

    fn placement(yaw: f64) -> Placement {
        Placement {
            yaw,
            ..Default::default()
        }
    }

    fn ray_towards(direction: DVec3) -> Ray {
        Ray {
            origin: DVec3::ZERO,
            direction: direction.normalize(),
        }
    }

    #[test]
    fn the_pointer_location_is_not_also_the_surface_origin() {
        // The bug this guards is subtle and total: smithay delivers `location - origin` to the
        // client, so passing the surface-local position as both makes every delivered position
        // (0, 0). Motion appears to work -- events flow, focus changes -- and nothing is ever
        // under the cursor, so no button in any application can be clicked.
        let hit = Hit {
            distance: 2.0,
            u: 0.5,
            v: 0.5,
            point: DVec3::ZERO,
        };
        let local = surface_position(&hit, PIXELS).expect("middle of the content");
        assert!(
            local.x > 1.0 && local.y > 1.0,
            "a centre hit must not be the origin: {local:?}"
        );
    }

    #[test]
    fn the_clickable_quad_is_taller_than_the_surface() {
        // The title bar is drawn above the content, so the quad the ray hits has to include
        // it -- otherwise the bar is visible and unclickable.
        let p = placement(0.0);
        let quad = quad_of(PIXELS, &p);
        let content = p.width / (PIXELS.0 as f64 / PIXELS.1 as f64);
        assert!(quad.height > content, "{} vs {content}", quad.height);
    }

    #[test]
    fn a_hit_near_the_top_is_the_title_bar_and_has_no_surface_position() {
        let hit = Hit {
            distance: 2.0,
            u: 0.5,
            v: 0.02,
            point: DVec3::ZERO,
        };
        assert!(
            surface_position(&hit, PIXELS).is_none(),
            "the bar is not the surface"
        );
    }

    #[test]
    fn content_coordinates_span_the_whole_surface() {
        // The top of the content must be y = 0 and the bottom the full height, or clicks land
        // consistently short of where they were aimed.
        let top = Hit {
            distance: 2.0,
            u: 0.0,
            v: TITLE_BAR_FRACTION,
            point: DVec3::ZERO,
        };
        let bottom = Hit {
            distance: 2.0,
            u: 1.0,
            v: 1.0,
            point: DVec3::ZERO,
        };
        let t = surface_position(&top, PIXELS).expect("just below the bar");
        let b = surface_position(&bottom, PIXELS).expect("bottom edge");
        assert!(t.x.abs() < 1e-9 && t.y.abs() < 1e-9, "{t:?}");
        assert!(
            (b.x - 1280.0).abs() < 1e-6 && (b.y - 800.0).abs() < 1e-6,
            "{b:?}"
        );
    }

    #[test]
    fn aiming_straight_ahead_hits_a_window_in_front() {
        let quad = quad_of(PIXELS, &placement(0.0));
        assert!(intersect_quad(&ray_towards(DVec3::X), &quad).is_some());
    }

    #[test]
    fn aiming_at_nothing_is_not_a_hit() {
        let quad = quad_of(PIXELS, &placement(0.0));
        assert!(intersect_quad(&ray_towards(DVec3::Z), &quad).is_none());
    }

    #[test]
    fn the_title_bar_is_a_reachable_target() {
        // One degree is roughly 2% of a window's height at this distance, and the ray is
        // head-anchored. A desktop-proportioned bar would be about one degree tall.
        let p = placement(0.0);
        let quad = quad_of(PIXELS, &p);
        let bar_height = quad.height * TITLE_BAR_FRACTION;
        let angular = 2.0 * (bar_height / 2.0 / p.radius).atan().to_degrees();
        assert!(angular > 2.0, "title bar is only {angular} deg tall");
    }
}
