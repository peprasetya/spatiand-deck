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

use spatiand_render::ray::{intersect_plane, pick, Quad, Ray};
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

/// How thick the window's frame is, as a fraction of the content's height.
///
/// Sized by the same argument as the title bar, and for the same reason it had to be
/// generous. The frame used to be 0.012 m — about a third of a degree at 2.2 m — which is
/// fine for something whose only job is to make the window's edge visible against a dark sky,
/// and hopeless as a target. At 10% of the content's height it comes out near 1.8°, which is
/// a little under the bar and still comfortably aimable. `the_frame_is_a_reachable_target`
/// is what holds that.
pub const BORDER_FRACTION: f64 = 0.10;

/// How much of the bar's height the icon and the close button occupy.
///
/// Comfortably under the whole bar, so both sit *in* it with glass showing around them rather
/// than filling it edge to edge. At the bar's ~1.5° this leaves a target of about 1°, which is
/// the smallest thing on the window anyone is asked to hit — and the reason the close button is
/// at the end of the bar, where overshooting lands on the bar rather than on the surface.
const FURNITURE_FRACTION: f64 = 0.66;

/// How far along the bottom edge counts as a corner rather than a side, in border widths.
///
/// Two, so a corner is roughly a 3.6° square. Smaller and it is a target you hit by luck;
/// much larger and the bottom edge stops existing on a narrow window.
const CORNER_REACH: f64 = 2.0;

/// The smallest a window may be dragged, in degrees across.
///
/// Not zero, and not a pixel count. A window below about 8° is a few hundred pixels of
/// squinting, and one dragged to nothing cannot be grabbed again to undo it — the frame it
/// would be grabbed by has gone with it.
const MINIMUM_ANGLE_DEG: f64 = 8.0;

/// And the largest. 90° is already wider than either eye can see at once; past that a window
/// is simply hiding the world with no way to tell how much of it is left.
const MAXIMUM_ANGLE_DEG: f64 = 90.0;

/// Which edge of a window is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Bottom,
    BottomLeft,
    BottomRight,
}

/// What a point on a window's quad means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// The bar along the top. Grab to move.
    Title,
    /// The button at the right of the bar. Press to ask the window to close.
    Close,
    /// The client's own surface. Everything here is forwarded.
    Content,
    /// The frame. Grab to resize.
    Resize(Edge),
}

/// A box on a window's quad, in the quad's own 0..1 coordinates.
///
/// Expressed in quad fractions rather than in metres so that one definition serves both the
/// hit test, which has `(u, v)` from the ray, and the drawing, which has the quad's size. A
/// close button drawn from one set of numbers and aimed at from another is the specific bug
/// this shape exists to make impossible — and it is not a bug anyone finds by reading, only by
/// pressing a thing and having nothing happen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Box2 {
    pub u: f64,
    pub v: f64,
    pub half_u: f64,
    pub half_v: f64,
}

impl Box2 {
    fn contains(&self, u: f64, v: f64) -> bool {
        (u - self.u).abs() <= self.half_u && (v - self.v).abs() <= self.half_v
    }
}

/// A window's quad broken into its parts, in units of the content's height.
///
/// Proportional rather than metric, so the same numbers describe a window at any size or
/// distance and the whole thing is testable without a placement. Multiplying by the content
/// height gives metres, which is all [`quad_of`] does.
///
/// ```text
///   ┌─────────────────────────┐  ─┐ border
///   │        title bar        │   ├ bar
///   ├──┬───────────────────┬──┤  ─┘
///   │  │                   │  │
///   │  │      content      │  │   1.0
///   │  │                   │  │
///   ├──┴───────────────────┴──┤  ─┐ border
///   └─────────────────────────┘  ─┘
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// The content's width — which, in units of its own height, is its aspect ratio.
    pub content_width: f64,
    pub border: f64,
    pub bar: f64,
}

impl Frame {
    pub fn of(pixels: (u32, u32)) -> Self {
        Self {
            content_width: pixels.0 as f64 / pixels.1.max(1) as f64,
            border: BORDER_FRACTION,
            // TITLE_BAR_FRACTION is a share of the bar-plus-content height, which is how the
            // drawing has always expressed it; here everything is relative to the content
            // alone, so it has to be rebased.
            bar: TITLE_BAR_FRACTION / (1.0 - TITLE_BAR_FRACTION),
        }
    }

    pub fn width(&self) -> f64 {
        self.content_width + self.border * 2.0
    }

    pub fn height(&self) -> f64 {
        1.0 + self.bar + self.border * 2.0
    }

    /// Where a quad hit falls, in content coordinates — 0..1 inside, outside that beyond.
    fn content_at(&self, u: f64, v: f64) -> (f64, f64) {
        (
            (u * self.width() - self.border) / self.content_width.max(1e-6),
            v * self.height() - self.border - self.bar,
        )
    }

    /// Where the title bar's furniture sits, in quad fractions.
    ///
    /// Both are square, sized against the bar's height, and inset from the chrome's outer edge
    /// by the border — so they sit over the glass rather than over the surface, whatever the
    /// window's aspect ratio.
    fn furniture(&self, from_right: bool) -> Box2 {
        let side = self.bar * FURNITURE_FRACTION;
        let (w, h) = (self.width(), self.height());
        // The bar's centre is half a content-height above the quad's centre — the chrome
        // reaches further above the content than below it, so the two centres do not coincide.
        let v = 0.5 - 0.5 / h;
        let inset = (self.border + side * 0.5) / w;
        Box2 {
            u: if from_right { 1.0 - inset } else { inset },
            v,
            half_u: side * 0.5 / w,
            half_v: side * 0.5 / h,
        }
    }

    /// The application's icon, at the left of the bar. Decoration only — nothing to press.
    pub fn icon(&self) -> Box2 {
        self.furniture(false)
    }

    /// The close button, at the right of the bar.
    ///
    /// There is no minimise and no maximise, and that is a statement rather than an omission:
    /// neither means anything in a room. A window that is in the way is moved or pushed
    /// further off, and one you are finished with is closed.
    pub fn close(&self) -> Box2 {
        self.furniture(true)
    }

    /// What the wearer is pointing at.
    pub fn zone(&self, u: f64, v: f64) -> Zone {
        let (x, y) = self.content_at(u, v);
        // Everything above the content is the bar, including the frame above it: a strip of
        // border one degree tall that behaves differently from the bar it touches would be
        // impossible to aim at and pointless if you could.
        if y < 0.0 {
            // Except the close button, which is inside the bar and has to be tested first or
            // it is simply a piece of the bar that happens to have a cross drawn on it.
            return if self.close().contains(u, v) {
                Zone::Close
            } else {
                Zone::Title
            };
        }
        let left = x < 0.0;
        let right = x > 1.0;
        let bottom = y > 1.0;
        if !left && !right && !bottom {
            return Zone::Content;
        }
        // An L-shaped corner region wrapping the bottom of each side, as every 2D desktop
        // does it — the corner has to be reachable along both the side and the bottom, or it
        // is only ever found by accident.
        let reach_x = self.border * CORNER_REACH / self.content_width.max(1e-6);
        let reach_y = self.border * CORNER_REACH;
        let near_bottom = y > 1.0 - reach_y;
        let near_left = x < reach_x;
        let near_right = x > 1.0 - reach_x;
        Zone::Resize(match (left, right, bottom) {
            (true, _, _) if near_bottom => Edge::BottomLeft,
            (_, true, _) if near_bottom => Edge::BottomRight,
            (true, _, _) => Edge::Left,
            (_, true, _) => Edge::Right,
            (_, _, _) if near_left => Edge::BottomLeft,
            (_, _, _) if near_right => Edge::BottomRight,
            _ => Edge::Bottom,
        })
    }
}

/// What a resize drag has arrived at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resized {
    pub placement: crate::window::Placement,
    pub pixels: (u32, u32),
}

/// Work out a window's new size and position, part-way through a resize drag.
///
/// `dx` and `dy` are how far the ray has travelled since the grab, in metres across and down
/// the window's own plane.
///
/// The opposite edge stays put, which is what makes this feel like resizing rather than
/// scaling: drag the right edge and the left one does not move. That is entirely a matter of
/// shifting the centre by half of whatever the size changed by, in the right direction.
///
/// Pixels follow the world size at a **constant density**, so the client is asked for more
/// buffer rather than a bigger picture of the same buffer. Text keeps its angular size and
/// more of it fits — which is the difference between resizing a window and zooming it, and
/// the thing the two-thumb gesture deliberately does not do.
pub fn resize(
    edge: Edge,
    start: &crate::window::Placement,
    start_pixels: (u32, u32),
    dx: f64,
    dy: f64,
) -> Resized {
    let aspect = start_pixels.0 as f64 / start_pixels.1.max(1) as f64;
    let start_height = start.width / aspect.max(0.01);
    // Pixels per metre, held fixed for the whole drag.
    let density = start_pixels.0 as f64 / start.width.max(1e-6);

    let (grow_x, grow_y) = match edge {
        Edge::Left => (-dx, 0.0),
        Edge::Right => (dx, 0.0),
        Edge::Bottom => (0.0, dy),
        Edge::BottomLeft => (-dx, dy),
        Edge::BottomRight => (dx, dy),
    };

    // Limits in metres, from angles at this radius, so a window pushed far away is still
    // allowed to be as big on screen as a near one.
    let limit = |degrees: f64| 2.0 * start.radius * (degrees.to_radians() / 2.0).tan();
    let (min_w, max_w) = (limit(MINIMUM_ANGLE_DEG), limit(MAXIMUM_ANGLE_DEG));
    let width = (start.width + grow_x).clamp(min_w, max_w);
    let height = (start_height + grow_y).clamp(min_w / aspect.max(0.01), max_w);

    // Clamping changes how much the window actually grew, and the centre has to follow *that*
    // rather than the drag — otherwise a window held at its minimum keeps sliding sideways.
    let moved_x = width - start.width;
    let moved_y = height - start_height;

    // Half the growth, towards the edge being dragged. `+u` is rightwards and `+v` downwards,
    // and both are a negative rotation: yaw is positive to the left, pitch positive upwards.
    let shift_right = match edge {
        Edge::Left | Edge::BottomLeft => -moved_x * 0.5,
        Edge::Right | Edge::BottomRight => moved_x * 0.5,
        Edge::Bottom => 0.0,
    };
    let shift_down = match edge {
        Edge::Bottom | Edge::BottomLeft | Edge::BottomRight => moved_y * 0.5,
        _ => 0.0,
    };

    Resized {
        placement: crate::window::Placement {
            yaw: start.yaw - shift_right / start.radius,
            pitch: start.pitch - shift_down / start.radius,
            radius: start.radius,
            width,
        },
        pixels: (
            (width * density).round().clamp(160.0, 4096.0) as u32,
            (height * density).round().clamp(120.0, 4096.0) as u32,
        ),
    }
}

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
    /// Dragging an edge or a corner of the frame.
    Resize {
        window: smithay::desktop::Window,
        edge: Edge,
        /// The quad as it stood when the edge was grabbed.
        ///
        /// Measured against throughout, rather than against the window as it is now. The live
        /// quad moves and changes size as the drag proceeds, so measuring against it would
        /// feed the result back into its own input: the window would accelerate away from the
        /// pointer in whichever direction it was first nudged.
        start_quad: Quad,
        start_u: f64,
        start_v: f64,
        start_placement: crate::window::Placement,
        start_pixels: (u32, u32),
    },
}

impl Drag {
    /// The window this drag is about, whichever kind it is.
    pub fn window(&self) -> &smithay::desktop::Window {
        match self {
            Drag::Move { window, .. } | Drag::Resize { window, .. } => window,
        }
    }

    /// Where a resize drag has got to, given where the pointer is now.
    ///
    /// `None` for the other kinds of drag, and for a ray that has swung round behind the
    /// window's plane — at which point there is no sensible answer and holding the last size
    /// is better than jumping to a reflected one.
    pub fn resized(&self, ray: &Ray) -> Option<Resized> {
        let Drag::Resize {
            edge,
            start_quad,
            start_u,
            start_v,
            start_placement,
            start_pixels,
            ..
        } = self
        else {
            return None;
        };
        let now = intersect_plane(ray, start_quad)?;
        Some(resize(
            *edge,
            start_placement,
            *start_pixels,
            (now.u - start_u) * start_quad.width,
            (now.v - start_v) * start_quad.height,
        ))
    }
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
    /// An open menu the ray landed on: which window owns it, which of its popups, and where.
    ///
    /// Separate from `hit` rather than folded into it because a popup is not part of its
    /// window's quad and must not be treated as one. A menu item is not a title bar, is not a
    /// resize edge, and must not start a drag — keeping it in its own field means every one of
    /// those tests below simply never sees it, instead of each having to remember to exclude it.
    pub popup: Option<(usize, usize, Hit)>,
    /// True when the hit landed in the window's title bar rather than its content.
    pub on_title: bool,
    /// Which part of the window the ray is over, if any.
    pub zone: Option<Zone>,
}

impl Aim {
    /// True while the ray is on an open menu.
    pub fn on_popup(&self) -> bool {
        self.popup.is_some()
    }
}

/// Cast a ray and work out what it means.
pub fn aim(ray: Ray, windows: &[WindowQuad]) -> Aim {
    let quads: Vec<Quad> = windows
        .iter()
        .map(|w| quad_of(w.pixels, &w.placement))
        .collect();
    let hit = pick(&ray, &quads);

    // Menus are tested separately and win outright.
    //
    // They need their own quads because a popup routinely hangs off the edge of the window
    // that opened it — a context menu near the bottom of a page, a dropdown wider than the
    // control it belongs to — and those parts are outside the window's quad entirely. Testing
    // only the window would leave the overhanging half of every menu visible and dead.
    //
    // Nearest wins among popups, which is what makes a submenu sitting on top of its parent
    // menu take the click: each nesting level is drawn a step closer to the viewer.
    let mut popup: Option<(usize, usize, Hit)> = None;
    for (w, window) in windows.iter().enumerate() {
        for (p, quad) in window.popups.iter().enumerate() {
            let Some(h) = spatiand_render::ray::intersect_quad(
                &ray,
                &popup_quad(window.pixels, &window.placement, quad.offset, quad.pixels),
            ) else {
                continue;
            };
            if popup.as_ref().map_or(true, |(_, _, best)| h.distance < best.distance) {
                popup = Some((w, p, h));
            }
        }
    }

    // The bar and the frame occupy the edges of the same quad rather than quads of their own:
    // a second quad would need its own intersection test and could disagree with the first
    // about which window is in front.
    let zone = match popup {
        // A menu covers whatever chrome is behind it. Reporting the zone underneath would let
        // a click on the top row of a menu grab the title bar it happens to be sitting over.
        Some(_) => Some(Zone::Content),
        None => hit.and_then(|(index, h)| Some(Frame::of(windows.get(index)?.pixels).zone(h.u, h.v))),
    };
    Aim {
        ray,
        hit,
        popup,
        on_title: zone == Some(Zone::Title),
        zone,
    }
}

/// The quad an open popup occupies, on its parent window's plane.
///
/// Takes plain geometry rather than the `WindowQuad` so it can be tested without a compositor,
/// and mirrors the arithmetic in `Scene::draw_windows` exactly — these two must agree, or a
/// menu is drawn in one place and pressed in another.
///
/// The forward lift the drawing applies is deliberately *not* included: it is a millimetre, it
/// exists only to keep two coplanar quads from fighting over pixels, and folding it in here
/// would couple the hit test to a rendering detail for no measurable change in where the ray
/// lands.
pub fn popup_quad(
    pixels: (u32, u32),
    placement: &crate::window::Placement,
    offset: (i32, i32),
    popup_pixels: (u32, u32),
) -> Quad {
    let aspect = pixels.0 as f64 / pixels.1.max(1) as f64;
    let content_width = placement.width;
    let content_height = content_width / aspect.max(0.01);
    let (w, h) = (pixels.0.max(1) as f64, pixels.1.max(1) as f64);
    // Centre of the popup as a fraction across the parent's surface. `v` runs down from the
    // top while the world's z runs up, which is where the sign comes from.
    let u = (offset.0 as f64 + popup_pixels.0 as f64 * 0.5) / w;
    let v = (offset.1 as f64 + popup_pixels.1 as f64 * 0.5) / h;
    let orientation = placement.orientation();
    let local = glam::DVec3::new(
        0.0,
        -(u - 0.5) * content_width,
        (0.5 - v) * content_height,
    );
    Quad {
        centre: placement.position() + orientation * local,
        orientation,
        width: popup_pixels.0 as f64 * content_width / w,
        height: popup_pixels.1 as f64 * content_height / h,
    }
}

/// The quad a window occupies, including its title bar.
///
/// Takes the geometry rather than the whole [`WindowQuad`] so it can be tested: a `WindowQuad`
/// carries a live Wayland window, which cannot be conjured up without a compositor.
pub fn quad_of(pixels: (u32, u32), placement: &crate::window::Placement) -> Quad {
    let frame = Frame::of(pixels);
    let aspect = pixels.0 as f64 / pixels.1.max(1) as f64;
    let content_height = placement.width / aspect.max(0.01);
    Quad {
        centre: centre_of(pixels, placement),
        orientation: placement.orientation(),
        // The bar sits above the content and the frame surrounds it, so the quad the ray hits
        // is bigger than the surface on every side. A quad the size of the content alone
        // leaves the chrome visible and unclickable, which is how the title bar behaved
        // before it was included here.
        width: content_height * frame.width(),
        height: content_height * frame.height(),
    }
}

/// The centre of the whole quad, which is not the centre of the content.
///
/// The bar hangs above the content, so the chrome is taller upwards than downwards and its
/// midpoint sits above the surface's. Missing this offsets every hit by half a title bar —
/// about a degree — which is small enough to look like poor aim rather than a bug.
fn centre_of(pixels: (u32, u32), placement: &crate::window::Placement) -> glam::DVec3 {
    let frame = Frame::of(pixels);
    let aspect = pixels.0 as f64 / pixels.1.max(1) as f64;
    let content_height = placement.width / aspect.max(0.01);
    let up = placement.orientation() * glam::DVec3::Z;
    placement.position() + up * (content_height * frame.bar * 0.5)
}

/// Surface coordinates for a hit, in the client's own pixels.
///
/// Returns `None` for a hit on the title bar: that is Spatiand's chrome, and forwarding it as
/// a pointer position would put the cursor above the top edge of the surface.
pub fn surface_position(hit: &Hit, pixels: (u32, u32)) -> Option<Point<f64, Logical>> {
    let frame = Frame::of(pixels);
    if frame.zone(hit.u, hit.v) != Zone::Content {
        return None;
    }
    let (x, y) = frame.content_at(hit.u, hit.v);
    Some(Point::from((x * pixels.0 as f64, y * pixels.1 as f64)))
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
        // A menu takes the pointer whenever the ray is on one, and its coordinates are its
        // own: a client positions a popup itself and expects events in the popup's surface,
        // not in the window's. Sending window-local coordinates to a menu would highlight the
        // wrong row -- or, where the menu overhangs the window, no row at all.
        let on_popup = aim.popup.and_then(|(w, p, hit)| {
            let popup = windows.get(w)?.popups.get(p)?;
            let position = Point::from((
                hit.u * popup.pixels.0 as f64,
                hit.v * popup.pixels.1 as f64,
            ));
            Some((popup.surface.clone(), position))
        });

        let focus = match on_popup.clone() {
            Some((surface, _)) => Some((surface, Point::from((0.0, 0.0)))),
            None => aim.hit.and_then(|(index, hit)| {
                let window = windows.get(index)?;
                let _ = surface_position(&hit, window.pixels)?;
                // Straight off the quad, rather than looked up by position -- see the note on
                // WindowQuad::window for why an index cannot be trusted between frames.
                Some((window.surface.clone(), Point::from((0.0, 0.0))))
            }),
        };
        let local = match on_popup {
            Some((_, position)) => Some(position),
            None => aim
                .hit
                .and_then(|(index, hit)| surface_position(&hit, windows.get(index)?.pixels)),
        };

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

    /// Close every open menu.
    ///
    /// Ordinarily a client does this itself when its popup grab is broken. Spatiand does not
    /// hand out grabs — see `XdgShellHandler::grab` for why a ray through a room is a bad fit
    /// for a promise that all input goes to one surface — so the compositor has to say so
    /// explicitly, or a menu dismissed by looking away stays on screen for ever.
    ///
    /// Innermost first: dismissing a parent destroys its children, and a client told about a
    /// popup whose parent has already gone is a protocol error on our side.
    pub fn dismiss_popups(&mut self, state: &mut Spatiand) {
        let open: Vec<smithay::desktop::PopupKind> = state
            .space
            .elements()
            .filter_map(|w| w.toplevel())
            .flat_map(|t| {
                smithay::desktop::PopupManager::popups_for_surface(t.wl_surface())
                    .map(|(popup, _)| popup)
            })
            .collect();
        for popup in open.into_iter().rev() {
            if let smithay::desktop::PopupKind::Xdg(popup) = popup {
                popup.send_popup_done();
            }
        }
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
    // Only the tests cast a *bounded* ray; the module itself always wants the whole plane.
    use spatiand_render::ray::intersect_quad;

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
    fn a_popup_covering_the_whole_surface_lands_exactly_on_it() {
        // The calibration check for the whole popup coordinate system. A popup the size of its
        // parent, at the origin, must come out as the parent's content quad -- same centre,
        // same size. Anything wrong with the sign of v, the aspect, or the pixel scale shows
        // up here rather than as a menu that is slightly off in a headset.
        let p = placement(0.0);
        let quad = popup_quad(PIXELS, &p, (0, 0), PIXELS);
        let aspect = PIXELS.0 as f64 / PIXELS.1 as f64;
        assert!((quad.width - p.width).abs() < 1e-9, "width {}", quad.width);
        assert!(
            (quad.height - p.width / aspect).abs() < 1e-9,
            "height {}",
            quad.height
        );
        assert!(
            quad.centre.distance(p.position()) < 1e-9,
            "centre {:?} vs {:?}",
            quad.centre,
            p.position()
        );
    }

    #[test]
    fn a_popup_in_the_top_left_of_a_window_is_up_and_to_the_left() {
        // The frame is X forward, Y left, Z up. A menu in the top-left quarter of a surface
        // must therefore sit at +Y and +Z of the window's centre. Getting v's sign backwards
        // is the easy mistake -- it puts every menu the same distance below where it belongs,
        // which reads as a placement bug rather than an inverted axis.
        let p = placement(0.0);
        let quarter = (PIXELS.0 / 2, PIXELS.1 / 2);
        let quad = popup_quad(PIXELS, &p, (0, 0), quarter);
        let offset = quad.centre - p.position();
        assert!(offset.y > 0.0, "should be to the left, got {offset:?}");
        assert!(offset.z > 0.0, "should be above, got {offset:?}");
        // A quarter-sized popup is a quarter of the surface in each axis.
        assert!((quad.width - p.width * 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_menu_may_hang_off_the_edge_of_its_window() {
        // Popups routinely overhang -- a context menu near the bottom of a page, a dropdown
        // wider than the control that opened it. That is why they are hit-tested as quads of
        // their own rather than as a region of the parent: the overhanging part is outside the
        // window entirely, and testing only the window would leave it visible and dead.
        let p = placement(0.0);
        let overhanging = popup_quad(PIXELS, &p, (PIXELS.0 as i32 - 40, 0), (400, 300));
        let centre_offset = (overhanging.centre - p.position()).y;
        // Its centre is past the window's right edge, which in this frame is -Y.
        assert!(
            centre_offset < -p.width * 0.5,
            "expected the centre beyond the right edge, got {centre_offset}"
        );
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
    fn the_close_button_is_in_the_bar_at_the_right_and_the_icon_at_the_left() {
        let f = Frame::of(PIXELS);
        let (icon, close) = (f.icon(), f.close());
        assert!(icon.u < 0.5 && close.u > 0.5, "{icon:?} {close:?}");
        // Both inside the quad rather than half off its edge.
        assert!(icon.u - icon.half_u > 0.0);
        assert!(close.u + close.half_u < 1.0);
        // And both in the bar: the lowest point of each is still above the content.
        let below = |b: Box2| f.content_at(b.u, b.v + b.half_v).1;
        assert!(below(icon) < 0.0, "the icon overlaps the surface");
        assert!(below(close) < 0.0, "the close button overlaps the surface");
    }

    #[test]
    fn pressing_the_close_button_is_not_pressing_the_bar() {
        // The distinction the whole zone exists for. If this ever collapses, pressing close
        // starts dragging the window instead, which looks like the button being dead.
        let f = Frame::of(PIXELS);
        let close = f.close();
        assert_eq!(f.zone(close.u, close.v), Zone::Close);
        // A little to the left of it is ordinary bar.
        assert_eq!(f.zone(close.u - close.half_u * 3.0, close.v), Zone::Title);
    }

    #[test]
    fn the_icon_is_not_a_button() {
        // It is there to say which application this is, and a target that does nothing is
        // worse than no target: it gets pressed, and the window does not move.
        let f = Frame::of(PIXELS);
        let icon = f.icon();
        assert_eq!(f.zone(icon.u, icon.v), Zone::Title);
    }

    #[test]
    fn the_furniture_is_square_in_the_world_whatever_the_window_shape() {
        // u and v are fractions of different lengths, so equal fractions are not a square. A
        // wide window would otherwise get a close button stretched into a letterbox.
        for pixels in [(1280u32, 800u32), (800, 1280), (2560, 720)] {
            let f = Frame::of(pixels);
            let c = f.close();
            let (w, h) = (c.half_u * f.width(), c.half_v * f.height());
            assert!((w - h).abs() < 1e-9, "{pixels:?} gave {w} by {h}");
        }
    }

    #[test]
    fn aiming_at_the_close_button_does_not_offer_to_move_the_window() {
        // `on_title` is what starts a drag, so it has to be false here even though the button
        // is geometrically part of the bar.
        let f = Frame::of(PIXELS);
        let close = f.close();
        assert!(matches!(f.zone(close.u, close.v), Zone::Close));
        assert_ne!(f.zone(close.u, close.v), Zone::Title);
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
    fn aiming_straight_ahead_hits_a_window_in_front() {
        let quad = quad_of(PIXELS, &placement(0.0));
        assert!(intersect_quad(&ray_towards(DVec3::X), &quad).is_some());
    }

    #[test]
    fn aiming_at_nothing_is_not_a_hit() {
        let quad = quad_of(PIXELS, &placement(0.0));
        assert!(intersect_quad(&ray_towards(DVec3::Z), &quad).is_none());
    }

    /// A hit at a point in the *content*, 0..1, expressed as a hit on the whole quad.
    fn at_content(pixels: (u32, u32), x: f64, y: f64) -> Hit {
        let f = Frame::of(pixels);
        Hit {
            distance: 2.0,
            u: (x * f.content_width + f.border) / f.width(),
            v: (y + f.border + f.bar) / f.height(),
            point: DVec3::ZERO,
        }
    }

    #[test]
    fn the_frame_is_a_reachable_target() {
        // The frame used to be 0.012 m, about a third of a degree at 2.2 m, which is fine for
        // being seen and hopeless for being aimed at with a head-anchored ray.
        let p = placement(0.0);
        let content_height = p.width / (PIXELS.0 as f64 / PIXELS.1 as f64);
        let border = content_height * BORDER_FRACTION;
        let angular = (border / p.radius).to_degrees();
        assert!(angular > 1.5, "frame is only {angular} deg thick");
    }

    #[test]
    fn the_middle_of_a_window_is_its_content() {
        assert_eq!(Frame::of(PIXELS).zone(0.5, 0.5), Zone::Content);
    }

    #[test]
    fn each_edge_of_the_frame_is_the_edge_it_looks_like() {
        // Sign errors here are invisible: every zone is a valid zone, so getting left and
        // right the wrong way round produces a cursor that resizes the opposite edge and
        // reads as the window fighting back.
        let f = Frame::of(PIXELS);
        let mid_content = f.border + f.bar + 0.5;
        let just_inside = |t: f64| t * 0.5;
        assert_eq!(
            f.zone(just_inside(f.border) / f.width(), mid_content / f.height()),
            Zone::Resize(Edge::Left)
        );
        assert_eq!(
            f.zone(1.0 - just_inside(f.border) / f.width(), mid_content / f.height()),
            Zone::Resize(Edge::Right)
        );
        // Middle of the bottom strip, horizontally centred so it is a side rather than a corner.
        assert_eq!(
            f.zone(0.5, 1.0 - just_inside(f.border) / f.height()),
            Zone::Resize(Edge::Bottom)
        );
        // Anything above the content is the bar, frame included.
        assert_eq!(f.zone(0.5, 0.0), Zone::Title);
        assert_eq!(f.zone(0.5, (f.border + f.bar * 0.5) / f.height()), Zone::Title);
    }

    #[test]
    fn the_bottom_corners_are_reachable_from_both_directions() {
        // An L-shaped region, as on a 2D desktop: a corner you can only hit by coming along
        // the side is a corner you find by accident.
        let f = Frame::of(PIXELS);
        let below = 1.0 - (f.border * 0.5) / f.height();
        let beside = (f.border * 0.5) / f.width();
        assert_eq!(f.zone(beside, below), Zone::Resize(Edge::BottomLeft));
        assert_eq!(f.zone(1.0 - beside, below), Zone::Resize(Edge::BottomRight));
        // From along the bottom, just inside the content's own width.
        let inside_left = (f.border + f.border * 0.5) / f.width();
        assert_eq!(f.zone(inside_left, below), Zone::Resize(Edge::BottomLeft));
        // From up the side, above the corner, is the plain side.
        let up_the_side = (f.border + f.bar + 0.5) / f.height();
        assert_eq!(f.zone(beside, up_the_side), Zone::Resize(Edge::Left));
    }

    #[test]
    fn a_corner_is_big_enough_to_aim_at() {
        let p = placement(0.0);
        let content_height = p.width / (PIXELS.0 as f64 / PIXELS.1 as f64);
        let corner = content_height * BORDER_FRACTION * CORNER_REACH;
        assert!((corner / p.radius).to_degrees() > 3.0);
    }

    #[test]
    fn a_corner_never_swallows_the_whole_bottom_edge() {
        // On a narrow window the two corner regions could meet in the middle, leaving no
        // bottom edge at all.
        let f = Frame::of((400, 900));
        assert_eq!(f.zone(0.5, 1.0 - f.border * 0.5 / f.height()), Zone::Resize(Edge::Bottom));
    }

    #[test]
    fn content_coordinates_still_span_the_whole_surface() {
        // The frame shifted where the content sits inside the quad. If `surface_position` and
        // `zone` disagree by even a little, clicks land consistently short of where they were
        // aimed -- which is what happened when the title bar was first added.
        // Both probes sit a hair inside their corner. The corners themselves are the zone
        // boundaries, and which side of one a float lands on is not a property worth asserting.
        let top_left =
            surface_position(&at_content(PIXELS, 0.001, 0.001), PIXELS).expect("inside");
        assert!(top_left.x < 3.0 && top_left.y < 3.0, "{top_left:?}");
        let bottom_right =
            surface_position(&at_content(PIXELS, 0.999, 0.999), PIXELS).expect("inside");
        assert!(bottom_right.x > 1277.0 && bottom_right.y > 798.0, "{bottom_right:?}");
    }

    #[test]
    fn the_frame_is_not_forwarded_to_the_client() {
        let f = Frame::of(PIXELS);
        let on_border = Hit {
            distance: 2.0,
            u: f.border * 0.5 / f.width(),
            v: 0.5,
            point: DVec3::ZERO,
        };
        assert!(surface_position(&on_border, PIXELS).is_none());
    }

    #[test]
    fn dragging_the_right_edge_out_widens_the_window() {
        let start = placement(0.0);
        let out = resize(Edge::Right, &start, PIXELS, 0.2, 0.0);
        assert!(out.placement.width > start.width);
        assert!(out.pixels.0 > PIXELS.0, "the client should get more buffer");
        // Constant density: this is a resize, not a zoom. Text keeps its angular size and
        // more of it fits, which is the whole distinction from the two-thumb gesture.
        let before = PIXELS.0 as f64 / start.width;
        let after = out.pixels.0 as f64 / out.placement.width;
        assert!((before - after).abs() / before < 0.01, "{before} vs {after}");
    }

    #[test]
    fn the_opposite_edge_stays_put() {
        // What separates resizing from scaling. Drag the right edge and the left one must not
        // move, or the window creeps across the sky as you size it.
        let start = placement(0.0);
        let out = resize(Edge::Right, &start, PIXELS, 0.2, 0.0);
        let left_before = start.yaw + (start.width * 0.5) / start.radius;
        let left_after = out.placement.yaw + (out.placement.width * 0.5) / out.placement.radius;
        assert!((left_before - left_after).abs() < 1e-9, "{left_before} vs {left_after}");
    }

    #[test]
    fn dragging_the_left_edge_out_also_widens_it() {
        // Leftwards is -dx, and the sign has to survive two negations: the growth, and the
        // direction the centre then moves.
        let start = placement(0.0);
        let out = resize(Edge::Left, &start, PIXELS, -0.2, 0.0);
        assert!(out.placement.width > start.width);
        let right_before = start.yaw - (start.width * 0.5) / start.radius;
        let right_after = out.placement.yaw - (out.placement.width * 0.5) / out.placement.radius;
        assert!((right_before - right_after).abs() < 1e-9);
        assert!(out.placement.yaw > start.yaw, "growing leftwards moves the centre left");
    }

    #[test]
    fn dragging_the_bottom_down_keeps_the_top_where_it_is() {
        let start = placement(0.0);
        let content_height = start.width / (PIXELS.0 as f64 / PIXELS.1 as f64);
        let out = resize(Edge::Bottom, &start, PIXELS, 0.0, 0.15);
        assert!(out.pixels.1 > PIXELS.1);
        assert_eq!(out.placement.width, start.width, "the bottom edge is not a width");
        let new_height = out.pixels.1 as f64 / (out.pixels.0 as f64 / out.placement.width);
        let top_before = start.pitch + (content_height * 0.5) / start.radius;
        let top_after = out.placement.pitch + (new_height * 0.5) / out.placement.radius;
        assert!((top_before - top_after).abs() < 1e-3, "{top_before} vs {top_after}");
    }

    #[test]
    fn a_corner_drag_changes_both_dimensions() {
        let start = placement(0.0);
        let out = resize(Edge::BottomRight, &start, PIXELS, 0.2, 0.15);
        assert!(out.pixels.0 > PIXELS.0 && out.pixels.1 > PIXELS.1);
    }

    #[test]
    fn a_window_cannot_be_dragged_down_to_nothing() {
        // The frame is the only thing you can grab to undo it, and it shrinks with the window.
        let start = placement(0.0);
        let out = resize(Edge::Right, &start, PIXELS, -100.0, 0.0);
        let angle = 2.0 * (out.placement.width / 2.0 / out.placement.radius).atan().to_degrees();
        assert!(angle >= MINIMUM_ANGLE_DEG - 0.01, "shrank to {angle} deg");
        assert!(out.pixels.0 >= 160 && out.pixels.1 >= 120);
    }

    #[test]
    fn a_window_cannot_be_dragged_over_the_whole_sky() {
        let start = placement(0.0);
        let out = resize(Edge::Right, &start, PIXELS, 100.0, 0.0);
        let angle = 2.0 * (out.placement.width / 2.0 / out.placement.radius).atan().to_degrees();
        assert!(angle <= MAXIMUM_ANGLE_DEG + 0.01, "grew to {angle} deg");
    }

    #[test]
    fn a_window_held_at_its_limit_stops_moving_too() {
        // The centre follows how much the window *actually* grew, not how far the drag went.
        // Following the drag instead leaves a window pinned at its minimum sliding sideways
        // for as long as you keep pulling.
        let start = placement(0.0);
        let far = resize(Edge::Right, &start, PIXELS, -100.0, 0.0);
        let further = resize(Edge::Right, &start, PIXELS, -200.0, 0.0);
        assert_eq!(far.placement.yaw, further.placement.yaw);
    }

    #[test]
    fn a_resize_holds_its_distance() {
        // Resizing is not the depth drag. Changing radius here would make the window appear to
        // resize while actually flying towards you.
        let start = placement(0.3);
        for edge in [Edge::Left, Edge::Right, Edge::Bottom, Edge::BottomLeft, Edge::BottomRight] {
            let out = resize(edge, &start, PIXELS, 0.1, 0.1);
            assert_eq!(out.placement.radius, start.radius, "{edge:?}");
        }
    }

    #[test]
    fn the_title_bar_is_a_reachable_target() {
        // One degree is roughly 2% of a window's height at this distance, and the ray is
        // head-anchored. A desktop-proportioned bar would be about one degree tall.
        let p = placement(0.0);
        let content_height = p.width / (PIXELS.0 as f64 / PIXELS.1 as f64);
        let bar_height = content_height * Frame::of(PIXELS).bar;
        let angular = 2.0 * (bar_height / 2.0 / p.radius).atan().to_degrees();
        assert!(angular > 2.0, "title bar is only {angular} deg tall");
    }
}
