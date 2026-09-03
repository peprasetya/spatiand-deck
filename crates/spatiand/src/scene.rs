//! Drawing the spatial world.
//!
//! One place that knows how to render an eye: environment, windows, glass bubbles, menus and
//! the pointer, in that order. Both backends call it, so the DRM path and the nested winit
//! path cannot drift apart in what they show.
//!
//! ## Why there is no depth buffer
//!
//! Everything drawn is a flat quad at a known distance, and the sky is at infinity. Sorting
//! back to front is exact for that, costs nothing, and keeps the offscreen target to a single
//! colour attachment — which matters because that target is 3840x1080 and is reallocated on
//! every hotplug.
//!
//! ## The laser beams
//!
//! A ray cast from between the eyes is foreshortened to a dot — it starts at the viewpoint, so
//! there is nothing of it to see. That was the reason the pointer began as a bare reticle.
//!
//! The fix is not to move the *aim*, which has to stay head-relative because the pads are
//! absolute in head space, but to move where the beam is *drawn from*: an anchor roughly where
//! the Deck is being held, below and in front of the eyes and offset to the correct side. The
//! beam then runs from your hands to the cursor, which is what a laser pointer looks like and
//! what every VR headset does, while the aiming maths is untouched.

use glam::{Mat3, Mat4, Quat, Vec3, Vec4};
use spatiand_render::sky::{SkyEye, SkyProjection, SkySource};
use spatiand_render::{Eye, EyeSide, Hit, Quad, Ray, TextImage, TextRenderer};
use spatiand_shell::{Mode, Shell};

use smithay::backend::renderer::gles::ffi;

use crate::gl::{
    upload_raw, upload_rgba, BubbleParams, BubblePipeline, QuadPipeline, RoundedPipeline,
    SkyPipeline,
};

/// Angular size of the pointer reticle, degrees. Constant in *angle*, not in metres, so it
/// stays the same size on screen wherever it lands.
const RETICLE_DEG: f32 = 1.1;
/// Diameter of a launcher bubble, metres, at the arc radius.
///
/// 0.16 m at 2 m is about 4.6°. That sounds small and is not: it is roughly a thumbnail at
/// arm's length, and at 48 px per degree it still gets ~220 px of icon.
///
/// The size is forced by the vertical field. Three rows of bubbles *plus their labels* have to
/// fit inside 23°, and at the 0.30 m this started with, the top and bottom rows were simply
/// outside the field — visible only by tilting your head.
const BUBBLE_DIAMETER_M: f32 = 0.16;
/// Where the key light sits, matching the one baked into the generated environment so the
/// specular highlights agree with the background.
const LIGHT_DIR: Vec3 = Vec3::new(0.55, 0.6, 0.58);
/// Resolution icons are rasterised at.
///
/// A bubble is ~4.6° across and the display resolves ~48 px per degree, so the icon inside it
/// occupies roughly 140 px. 256 leaves room for the focused bubble's scale-up and for a
/// headset with better optics, without being wasteful across forty apps.
const ICON_TEXTURE_PX: u32 = 256;
/// Resolution a window's title-bar icon is loaded at.
///
/// Far smaller than a launcher bubble's: the bar is about 2 degrees tall and the icon a little
/// under that, so it lands on roughly 80 display pixels. 128 leaves room for a window dragged
/// close to the face without being forty icons' worth of memory.
const WINDOW_ICON_PX: u32 = 128;
/// How far below level the keyboard hangs, radians, and how far away.
///
/// The first attempt put it where a real keyboard sits -- 30 degrees down and close -- which
/// is right for a desk and wrong here: the vertical field is 23 degrees, so at 30 degrees the
/// keyboard was entirely outside it. You could point at it, but not see what you were
/// pointing at.
///
/// The budget is unforgiving. A window is about 17 degrees tall against 23 total, so the
/// keyboard gets half the vertical field and hangs just below the eye line.
const KEYBOARD_GAP: f32 = 0.026;
const KEYBOARD_DISTANCE: f32 = 1.3;
/// Width the keyboard's face is rasterised at.
///
/// Fixed rather than derived from the wearer's pixel density, because the keyboard is
/// resizable: deriving it would rebuild sixty labels on every frame of a resize drag. At a
/// keyboard about 26 degrees across on a 48 px/degree panel this is comfortably oversampled.
const KEYBOARD_FACE_PX: u32 = 1792;
/// How far a hovered key stands off the face, metres.
///
/// Small: at a keyboard about 1.3 m away this is a couple of millimetres, which is enough for
/// the cap to cast itself clear of its neighbours and be seen as raised without the letters
/// swimming as the pointer moves between keys.
const HOVER_LIFT_M: f32 = 0.006;
/// How far a popup stands off the window it belongs to, metres, per level of nesting.
///
/// There is no depth buffer — the scene is flat quads sorted back to front — so a menu drawn
/// exactly on its window's plane is two coplanar surfaces arguing over every pixel, which
/// reads as the menu flickering whenever the head moves. This is a millimetre: invisible as a
/// gap at arm's length, decisive as a separation.
const POPUP_LIFT_M: f32 = 0.001;
/// Width one raised keycap is rasterised at.
///
/// Several times the ~120 px a cell gets on the baked face. This is the one key being looked at
/// directly, it costs a texture a few hundred pixels across, and it is only built when the
/// pointer first crosses onto that key.
const KEY_CAP_PX: u32 = 384;
/// Half the width of a full launcher row, in degrees, for placing the page dots just outside
/// it. Mirrors `spatiand_shell::launcher::COLUMN_SPACING_DEG` and is kept here so the scene
/// does not have to reach into the shell's layout arithmetic.
const COLUMN_SPACING_DEG_LOCAL: f32 = 9.0;

/// One window, ready to draw: its imported texture and where it sits.
pub struct WindowQuad {
    /// The window itself, not an index into anything.
    ///
    /// Positional indices into `Space::elements()` are **not stable**: raising a window on
    /// click reorders that iterator, so the index the pointer was holding then refers to a
    /// different window. The symptom was clicking inside an application and having focus jump
    /// to another one, which reads as the pointer being broken rather than as identity being
    /// wrong.
    pub window: smithay::desktop::Window,
    pub surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    pub texture: u32,
    /// Surface size in pixels, for the aspect ratio.
    pub pixels: (u32, u32),
    pub placement: crate::window::Placement,
    pub focused: bool,
    /// Rasterised title, if one has been built for this window.
    pub title: Option<TitleTexture>,
    /// The application's own icon, for the left of the title bar.
    pub icon: Option<TitleTexture>,
    /// True while a pointer is over the close button, which is the only thing that colours it.
    pub close_hot: bool,
    /// What this window's sound is doing, if it has any.
    ///
    /// `None` means the window has never made a sound, and it then has no speaker on its bar
    /// at all — a mute button on a window that cannot make a noise is a control that does
    /// nothing, and there would be one on every window in the room.
    pub sound: Option<WindowSound>,
    /// True while a pointer is over the mute button.
    pub mute_hot: bool,
    /// Menus and dropdowns this window has open, innermost last.
    pub popups: Vec<PopupQuad>,
}

/// What a window is playing, as far as its title bar is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSound {
    pub muted: bool,
    /// What the app is actually using, rather than how wide its connection is. `None` while
    /// it is silent, which is what makes the speaker fade out when a video ends.
    pub sounding: Option<spatiand_audio::Layout>,
    /// Loudest sample in the last block, for how brightly the speaker is lit.
    pub peak: f32,
}

/// A popup, already imported, placed in its parent window's own pixels.
///
/// Deliberately *not* a quad in its own right. A popup is positioned by the client against the
/// surface it belongs to and has no independent existence in the room — giving it its own
/// placement would mean a menu that could drift away from its window. Keeping it in the
/// parent's pixel space means it is drawn, hit-tested and dismissed as part of that window,
/// and one ray test serves both because they are coplanar.
#[derive(Clone)]
pub struct PopupQuad {
    pub surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    pub texture: u32,
    /// Top-left corner in the parent surface's pixels. May be negative: a menu is allowed to
    /// hang off the edge of the window that opened it, and frequently does.
    pub offset: (i32, i32),
    pub pixels: (u32, u32),
}

/// A window title, already on the GPU.
#[derive(Clone, Copy)]
pub struct TitleTexture {
    pub id: u32,
    pub aspect: f32,
}

/// A texture and the aspect ratio of the image in it.
#[derive(Clone, Copy)]
struct Texture {
    id: u32,
    aspect: f32,
}

/// One row of a menu, rasterised.
struct RowTextures {
    label: Texture,
    /// The status at the right of the row, where there is one.
    trailing: Option<Texture>,
}

/// Everything a menu needs on the GPU.
///
/// All of it is rasterised **white** and coloured by the tint at draw time, so which row is
/// selected — and therefore which row is bright — is decided while drawing rather than while
/// rasterising. That is what makes moving the cursor free.
struct MenuTextures {
    title: Texture,
    rows: Vec<RowTextures>,
    detail: Option<Texture>,
    /// The explanation's height in logical panel pixels, which the layout needs and only the
    /// rasteriser knows, since it depends on how many lines the text wrapped onto.
    detail_height: f32,
    footer: Option<Texture>,
    /// Device pixels per logical panel pixel when this was built. A different headset, or a
    /// different eye resolution, means rasterising again rather than magnifying.
    scale: f32,
}

impl MenuTextures {
    /// # Safety
    /// Context must be current.
    unsafe fn destroy(&self, gl: &ffi::Gles2) {
        let delete = |t: &Texture| gl.DeleteTextures(1, &t.id);
        delete(&self.title);
        for row in &self.rows {
            delete(&row.label);
            if let Some(t) = &row.trailing {
                delete(t);
            }
        }
        for t in [&self.detail, &self.footer].into_iter().flatten() {
            delete(t);
        }
    }
}

/// The menu card's colours.
///
/// The same language as the sidecar: a near-black ground, one accent, and text in two weights.
/// A card has to be legible against a bright panorama without becoming a solid rectangle
/// hanging in the room, which is what sets the ground's alpha.
const CARD_GROUND: [f32; 4] = [0.043, 0.05, 0.072, 0.93];
/// A hairline of accent just outside the card, so it ends somewhere definite. Without it the
/// card's edge is wherever the background happens to be dark, which reads as a smudge.
const CARD_RIM: [f32; 4] = [0.55, 0.70, 1.0, 0.16];
const ROW_SELECTED: [f32; 4] = [0.42, 0.68, 1.0, 0.20];
/// The bar at the selected row's left edge. The wash alone is not enough against a bright
/// environment; a saturated shape at full alpha is legible whatever is behind the card.
const ROW_MARK: [f32; 4] = [0.45, 0.72, 1.0, 1.0];
const INK: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const INK_ROW: [f32; 4] = [0.78, 0.82, 0.90, 1.0];
const INK_TRAILING: [f32; 4] = [0.50, 0.70, 1.0, 1.0];
const INK_DETAIL: [f32; 4] = [0.70, 0.76, 0.87, 1.0];
const INK_HINT: [f32; 4] = [0.50, 0.55, 0.66, 1.0];
const SEPARATOR: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
const SCROLL_TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.08];
const SCROLL_THUMB: [f32; 4] = [0.42, 0.68, 1.0, 0.70];

/// Em size of a row's trailing status, in logical panel pixels. Smaller than the label it sits
/// beside: it qualifies the row rather than naming it.
const TRAILING_EM: f32 = 23.0;

/// How much of the horizontal field the card spans, and how much of the vertical it may use.
///
/// The horizontal fraction is the card's *identity* — it is the same width in every menu, so
/// moving between settings and the environment list does not resize the thing you are reading.
/// The vertical fraction is a budget, not a size: the card is as tall as its contents need up
/// to this, and rows past it scroll.
const CARD_FOV_FRACTION: f64 = 0.62;
const CARD_HEIGHT_FRACTION: f64 = 0.68;
/// How far in front of the wearer a menu hangs, metres.
const MENU_DISTANCE: f32 = 1.6;

/// Which of the three pointers this is.
///
/// Two thumbs and a mouse. They have to be told apart at a glance — a two-handed gesture is
/// impossible to aim otherwise — and colour reads more easily out of the corner of an eye than
/// shape does. Warm for the right hand, cool for the left, and a pale green for the mouse,
/// which is nobody's hand and should not look like one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointing {
    RightThumb,
    LeftThumb,
    Mouse,
}

impl Pointing {
    fn hue(self) -> [f32; 3] {
        match self {
            Pointing::RightThumb => [1.0, 0.78, 0.42],
            Pointing::LeftThumb => [0.52, 0.82, 1.0],
            Pointing::Mouse => [0.62, 1.0, 0.72],
        }
    }
}

/// Textures and pipelines that live for the session.
pub struct Scene {
    quads: QuadPipeline,
    rounded: RoundedPipeline,
    sky_pipeline: SkyPipeline,
    bubbles: BubblePipeline,

    sky: u32,
    sky_source: SkySource,
    white: u32,
    reticle: u32,
    /// A double-headed arrow, turned in the plane to suit whichever edge is under the pointer.
    resize_cursor: u32,
    /// The cross on a window's close button.
    close_glyph: u32,
    /// The speaker on a window's mute button, sounding and silenced.
    speaker_glyph: u32,
    speaker_off_glyph: u32,
    /// A second cursor shape for the left pad. Different in outline as well as colour, so the
    /// two are distinguishable to someone who cannot rely on hue.
    reticle_left: u32,
    /// Rounded glass, for window frames and title bars.
    glass: u32,

    /// One per app in the launcher, in the same order.
    app_labels: Vec<Texture>,
    /// The label the bubbles show inside the glass — currently the app's initial.
    app_glyphs: Vec<Texture>,
    /// Rebuilt only when the app list changes; rasterising twenty labels a frame would cost
    /// more than everything else here put together.
    labels_built_for: usize,

    /// Rasterised window titles, keyed by the text itself.
    ///
    /// Keyed on the string rather than on a window handle because titles change while a window
    /// lives (a browser tab, a file being edited) and two windows often share one. Bounded
    /// below so a client that rewrites its title every frame cannot grow this without limit.
    titles: std::collections::HashMap<String, Texture>,
    /// Application icons for window title bars, by app id. `None` records "looked, found
    /// nothing", so a window without an icon costs one lookup rather than one per frame.
    window_icons: std::collections::HashMap<String, Option<Texture>>,

    /// One key drawn on its own and raised, for standing it off the face under the pointer.
    ///
    /// Separate from the baked face because hover changes as fast as the pointer moves, and
    /// rebuilding a 1792-pixel face with sixty labels on it every time the ray crosses a key
    /// would be a visible hitch for something that has to feel immediate. Keyed by
    /// [`crate::keyboard_face::cap_id`], so the whole board costs at most a few dozen small
    /// textures and shift's alternates are simply more of them.
    key_caps: std::collections::HashMap<String, Texture>,
    /// The keyboard face, rebuilt when anything drawn on it changes.
    keys: Option<Texture>,
    /// What the face was drawn for, as `(shift, ctrl, alt, click)`. A modifier that is on and
    /// drawn as off types the wrong character and reads as a broken keymap; a sound toggle
    /// drawn the wrong way round reads as a control that does not work.
    keys_latches: (bool, bool, bool, bool),
    keys_built: bool,

    /// The status bar, rebuilt when its text changes.
    status: Option<Texture>,
    status_text: String,

    /// The open menu, rasterised a piece at a time.
    ///
    /// A piece at a time rather than as one image, because the pieces have different lives.
    /// The rows change when the *list* changes, which is rarely; the selection changes
    /// constantly and is drawn as a shape, so it costs nothing; the explanation changes with
    /// the cursor and is one small upload. Rendering the menu as a single block of text tied
    /// all three together, so moving one row down re-rasterised and re-uploaded the lot.
    menu: Option<MenuTextures>,
    /// What was last rasterised, to tell a changed list from a moved cursor.
    menu_model: crate::menu::MenuModel,
    /// Where the list is scrolled to. Carried between frames so walking the rows moves the
    /// list as little as possible — see [`spatiand_render::panel::Layout::new`].
    menu_first: usize,
    menu_layout: Option<spatiand_render::panel::Layout>,

    /// Yaw the open menu is pinned to.
    ///
    /// Menus are *body-locked*: placed in front of you when they open, then left in the world
    /// so you can look around them. Head-locking a list you are trying to read makes it
    /// impossible to look at anything else; world-locking it from a fixed origin means opening
    /// it while facing away puts it behind you.
    anchor_yaw: f32,
    anchored_for: Option<Mode>,
    /// When the current menu opened, for the arrival animation.
    anchored_at: std::time::Instant,
    /// A menu that has just closed, still playing its exit. Kept so the launcher can be drawn
    /// for a moment after the shell has already moved on -- without it, closing is a hard cut,
    /// which in a 3D space reads as a glitch rather than as a dismissal.
    closing: Option<(Mode, std::time::Instant)>,
}

/// How long bubbles take to arrive, and to leave.
///
/// Short. This is a menu someone opens dozens of times a session, and anything that reads as
/// "an animation" rather than as "the thing appearing" becomes an obstacle by the tenth time.
const APPEAR_SECONDS: f32 = 0.28;
const DISMISS_SECONDS: f32 = 0.16;
/// Delay between one bubble arriving and the next, seconds.
///
/// The stagger is what makes it read as a group of objects rather than one fading rectangle.
/// Small enough that the whole grid is still settled well inside half a second.
const STAGGER_SECONDS: f32 = 0.022;

impl Scene {
    pub fn new(
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        sky_image: &spatiand_render::Sky,
    ) -> Result<Self, String> {
        let quads = QuadPipeline::new(renderer)?;
        let sky_pipeline = SkyPipeline::new(renderer, &quads)?;
        let bubbles = BubblePipeline::new(renderer, &quads)?;
        let rounded = RoundedPipeline::new(renderer, &quads)?;

        let (
            sky,
            white,
            reticle,
            reticle_left,
            resize_cursor,
            close_glyph,
            speaker_glyph,
            speaker_off_glyph,
            glass,
        ) = renderer
            .with_context(|gl| unsafe {
                (
                    // Wrapping horizontally: an equirectangular image joins itself, and
                    // clamping leaves a seam straight down the world behind you.
                    upload_raw(gl, sky_image.width, sky_image.height, &sky_image.rgba, true),
                    crate::gl::white_texture(gl),
                    {
                        let r = reticle_image(64);
                        upload_raw(gl, 64, 64, &r, false)
                    },
                    {
                        let r = reticle_image_left(64);
                        upload_raw(gl, 64, 64, &r, false)
                    },
                    {
                        let r = resize_cursor_image(64);
                        upload_raw(gl, 64, 64, &r, false)
                    },
                    {
                        let c = close_glyph_image(64);
                        upload_raw(gl, 64, 64, &c, false)
                    },
                    {
                        let g = speaker_glyph_image(64, false);
                        upload_raw(gl, 64, 64, &g, false)
                    },
                    {
                        let g = speaker_glyph_image(64, true);
                        upload_raw(gl, 64, 64, &g, false)
                    },
                    {
                        // Wide and short: it is stretched across frames of every proportion,
                        // and the corner radius is what has to survive that, not the pixels.
                        let g = glass_panel_image(256, 96, 14.0);
                        upload_raw(gl, 256, 96, &g, false)
                    },
                )
            })
            .map_err(|e| format!("no GL context: {e}"))?;

        Ok(Self {
            quads,
            rounded,
            sky_pipeline,
            bubbles,
            sky,
            sky_source: sky_image.source,
            white,
            reticle,
            reticle_left,
            resize_cursor,
            close_glyph,
            speaker_glyph,
            speaker_off_glyph,
            glass,
            app_labels: Vec::new(),
            app_glyphs: Vec::new(),
            labels_built_for: usize::MAX,
            titles: std::collections::HashMap::new(),
            window_icons: std::collections::HashMap::new(),
            key_caps: std::collections::HashMap::new(),
            keys: None,
            keys_latches: (false, false, false, false),
            keys_built: false,
            status: None,
            status_text: String::new(),
            menu: None,
            menu_model: Default::default(),
            menu_first: 0,
            menu_layout: None,
            anchor_yaw: 0.0,
            anchored_for: None,
            anchored_at: std::time::Instant::now(),
            closing: None,
        })
    }

    pub fn quads(&self) -> &QuadPipeline {
        &self.quads
    }

    /// Rounded rectangles and circles, from a distance field rather than a texture.
    /// The one-pixel white texture, for anything that wants a flat fill.
    pub fn white(&self) -> u32 {
        self.white
    }

    pub fn rounded(&self) -> &RoundedPipeline {
        &self.rounded
    }

    /// A 1x1 white texture, for drawing solid shapes.
    pub fn white_texture(&self) -> u32 {
        self.white
    }

    /// Replace the environment.
    ///
    /// This is the seam the media player will use: playing a 360 video is exactly "swap the
    /// sky texture every frame and put the projection back afterwards".
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn set_sky(&mut self, gl: &ffi::Gles2, image: &spatiand_render::Sky) {
        gl.DeleteTextures(1, &self.sky);
        self.sky = upload_raw(gl, image.width, image.height, &image.rgba, true);
        self.sky_source = image.source;
    }

    /// Pin an opening menu to the direction the wearer is currently facing.
    pub fn anchor_menu(&mut self, mode: Mode, current_yaw: f32) {
        if self.anchored_for != Some(mode) {
            self.anchor_yaw = current_yaw;
            self.anchored_for = Some(mode);
            self.anchored_at = std::time::Instant::now();
            self.closing = None;
        }
    }

    pub fn forget_anchor(&mut self) {
        if let Some(mode) = self.anchored_for.take() {
            self.closing = Some((mode, std::time::Instant::now()));
        }
    }

    /// Arrival progress for one bubble, 0..1, or the exit if the menu is closing.
    ///
    /// Eased with a smoothstep and overshoot-free: a bubble that springs past its size and
    /// settles back looks lively on a monitor and reads as wobbling in stereo, where the eyes
    /// are tracking its actual distance.
    fn appear_progress(&self, index: usize) -> f32 {
        let ease = |t: f32| {
            let t = t.clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        if let Some((_, since)) = self.closing {
            let t = since.elapsed().as_secs_f32() / DISMISS_SECONDS;
            // Leaving is not staggered. On the way out the wearer has already decided, and a
            // ripple just delays getting back to the world.
            return 1.0 - ease(t);
        }
        let elapsed = self.anchored_at.elapsed().as_secs_f32() - index as f32 * STAGGER_SECONDS;
        ease(elapsed / APPEAR_SECONDS)
    }

    /// True while a closing menu is still worth drawing.
    pub fn is_dismissing(&self) -> bool {
        self.closing
            .map(|(_, since)| since.elapsed().as_secs_f32() < DISMISS_SECONDS)
            .unwrap_or(false)
    }

    /// The menu still being drawn on the way out, if any.
    pub fn dismissing_mode(&self) -> Option<Mode> {
        self.is_dismissing().then(|| self.closing.map(|(m, _)| m))?
    }

    fn eye_rect(&self, side: EyeSide) -> (f32, f32, f32, f32) {
        self.sky_source.eye_rect(match side {
            EyeSide::Left => SkyEye::Left,
            EyeSide::Right => SkyEye::Right,
        })
    }

    /// Draw the environment behind everything else.
    ///
    /// # Safety
    /// Context must be current, and the target framebuffer bound.
    pub unsafe fn draw_sky(&self, gl: &ffi::Gles2, eye: &Eye) {
        // Translation removed: the sky is at infinity, and letting the neck model's few
        // centimetres through makes the background slide as you turn, which reads as the whole
        // world being loose.
        let mut view = eye.view;
        view.w_axis = Vec4::new(0.0, 0.0, 0.0, 1.0);
        let inv = (eye.projection * view).inverse();
        self.sky_pipeline.draw(
            gl,
            self.sky,
            &inv,
            self.eye_rect(eye.side),
            self.sky_source.yaw_offset_radians(),
            self.sky_source.projection == SkyProjection::Equirect180,
            1.0,
        );
    }

    /// Rebuild the per-app textures if the app list has changed.
    pub fn sync_apps(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        shell: &Shell,
        px_per_degree: f32,
    ) -> Result<(), String> {
        // Whatever the launcher is currently showing: groups at the top level, applications
        // inside one. Both are bubbles with a name and an icon, so the rest is identical.
        let launcher = shell.launcher();
        let entries: Vec<(String, Option<String>)> = match launcher.level() {
            spatiand_shell::Level::Groups => launcher
                .groups()
                .iter()
                .map(|g| (g.label.to_string(), Some(g.icon.to_string())))
                .collect(),
            spatiand_shell::Level::Apps(_) => launcher
                .apps_in_level()
                .iter()
                .map(|a| (a.name.clone(), a.icon.clone()))
                .collect(),
        };
        // Level and count together: entering a group of the same size as the group list would
        // otherwise reuse the wrong textures.
        let fingerprint = entries.len()
            + match launcher.level() {
                spatiand_shell::Level::Groups => 0,
                spatiand_shell::Level::Apps(_) => 10_000,
            };
        if fingerprint == self.labels_built_for {
            return Ok(());
        }

        let labels: Vec<TextImage> = entries
            .iter()
            .map(|(name, _)| text.render(name, px_per_degree * 0.75, 512, [232, 238, 255, 255]))
            .collect();
        // The icon inside the glass: the system's own, so an app looks the same here as it
        // does on the desktop. Falling back to the initial rather than to a blank or a
        // question mark — plenty of entries name an icon that is not installed, and a letter
        // is at least identifiable.
        let mut resolved = 0usize;
        let glyphs: Vec<TextImage> = entries
            .iter()
            .map(|(name, icon)| {
                let from_theme = icon
                    .as_deref()
                    .and_then(spatiand_platform::resolve_icon)
                    .and_then(|path| crate::icon::load(&path, ICON_TEXTURE_PX));
                match from_theme {
                    Some(image) => {
                        resolved += 1;
                        image
                    }
                    None => {
                        let initial = name
                            .chars()
                            .next()
                            .unwrap_or('?')
                            .to_uppercase()
                            .to_string();
                        text.render(&initial, px_per_degree * 4.0, 256, [255, 255, 255, 235])
                    }
                }
            })
            .collect();
        log::info!(
            "launcher icons: {resolved} of {} from the icon theme",
            entries.len()
        );

        let old: Vec<u32> = self
            .app_labels
            .iter()
            .chain(self.app_glyphs.iter())
            .map(|t| t.id)
            .collect();

        let (new_labels, new_glyphs) = renderer
            .with_context(|gl| unsafe {
                for id in &old {
                    gl.DeleteTextures(1, id);
                }
                let up = |images: &[TextImage]| -> Vec<Texture> {
                    images
                        .iter()
                        .map(|i| Texture {
                            id: upload_rgba(gl, i),
                            aspect: i.width as f32 / i.height.max(1) as f32,
                        })
                        .collect()
                };
                (up(&labels), up(&glyphs))
            })
            .map_err(|e| format!("no GL context: {e}"))?;

        self.app_labels = new_labels;
        self.app_glyphs = new_glyphs;
        self.labels_built_for = fingerprint;
        Ok(())
    }

    /// A texture for a window's application icon, loading it on first sight.
    ///
    /// Cached by application id and never evicted, unlike the title cache: there are as many
    /// entries as there are distinct applications ever opened in a session, which is a number
    /// bounded by patience. Titles need eviction because a clock in a title bar would add one
    /// per second.
    ///
    /// A `None` answer is cached too, as an absent texture. Plenty of windows have an app id
    /// that matches no desktop entry — a dialog, something started from a terminal — and
    /// re-reading the icon theme every frame to fail again would be the expensive way to draw
    /// nothing.
    pub fn window_icon(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        app_id: &str,
    ) -> Option<TitleTexture> {
        if app_id.is_empty() {
            return None;
        }
        if let Some(cached) = self.window_icons.get(app_id) {
            return cached.map(|t| TitleTexture {
                id: t.id,
                aspect: t.aspect,
            });
        }
        let loaded = spatiand_platform::icon_for_app(app_id)
            .and_then(|path| crate::icon::load(&path, WINDOW_ICON_PX))
            .and_then(|image| {
                let aspect = image.width as f32 / image.height.max(1) as f32;
                renderer
                    .with_context(|gl| unsafe { upload_rgba(gl, &image) })
                    .ok()
                    .map(|id| Texture { id, aspect })
            });
        self.window_icons.insert(app_id.to_string(), loaded);
        loaded.map(|t| TitleTexture {
            id: t.id,
            aspect: t.aspect,
        })
    }

    /// A texture for a window title, rasterising it on first sight.
    pub fn title_texture(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        title: &str,
        px_per_degree: f32,
    ) -> Option<TitleTexture> {
        if title.is_empty() {
            return None;
        }
        if let Some(existing) = self.titles.get(title) {
            return Some(TitleTexture {
                id: existing.id,
                aspect: existing.aspect,
            });
        }
        // A title that rewrites itself constantly -- a clock, a progress percentage -- would
        // otherwise add a texture per frame. Dropping the cache is cheaper than tracking ages,
        // and the visible cost is one rebuild of each title still on screen.
        if self.titles.len() > 64 {
            let ids: Vec<u32> = self.titles.values().map(|t| t.id).collect();
            let _ = renderer.with_context(|gl| unsafe {
                for id in ids {
                    gl.DeleteTextures(1, &id);
                }
            });
            self.titles.clear();
        }

        let image = text.render(title, px_per_degree * 0.8, 1024, [226, 234, 250, 255]);
        let aspect = image.width as f32 / image.height.max(1) as f32;
        let id = renderer
            .with_context(|gl| unsafe { upload_rgba(gl, &image) })
            .ok()?;
        self.titles
            .insert(title.to_string(), Texture { id, aspect });
        Some(TitleTexture { id, aspect })
    }

    /// Rebuild the keyboard face if anything drawn on it changed.
    ///
    /// Rasterised at a fixed width rather than at the wearer's pixel density: the keyboard can
    /// be resized, and rebuilding the texture on every frame of a resize drag would rasterise
    /// sixty labels per frame for a difference nobody can see at a degree per key.
    pub fn sync_keyboard(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        keyboard: &spatiand_shell::Keyboard,
        _px_per_degree: f32,
    ) -> Result<(), String> {
        // The sound toggle is drawn on the face too, so it belongs in what the cache is
        // keyed on. Left out, the speaker would keep the picture it had when the face was last
        // rebuilt and only change the next time a modifier happened to be pressed.
        let latches = (keyboard.shift, keyboard.ctrl, keyboard.alt, keyboard.click);
        if self.keys_built && self.keys_latches == latches {
            return Ok(());
        }
        self.keys_latches = latches;
        self.keys_built = true;

        let image = crate::keyboard_face::face(text, keyboard, KEYBOARD_FACE_PX);
        let old = self.keys.take();
        self.keys = Some(
            renderer
                .with_context(|gl| unsafe {
                    if let Some(t) = old {
                        gl.DeleteTextures(1, &t.id);
                    }
                    Texture {
                        id: upload_rgba(gl, &image),
                        aspect: image.width as f32 / image.height.max(1) as f32,
                    }
                })
                .map_err(|e| format!("no GL context: {e}"))?,
        );
        Ok(())
    }

    /// Make sure a raised key's picture exists as a texture of its own.
    ///
    /// Called for whatever the pointers are over, before the frame is drawn — building a
    /// texture needs the renderer, and the draw closure holds the GL context.
    pub fn sync_key_cap(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        keyboard: &spatiand_shell::keyboard::Keyboard,
        key: &spatiand_shell::keyboard::Key,
    ) -> Result<(), String> {
        let id = crate::keyboard_face::cap_id(keyboard, key);
        if self.key_caps.contains_key(&id) {
            return Ok(());
        }
        // Drawn well above the size it appears at: this is the one key being looked at
        // directly, and the face behind it only affords a hundred-odd pixels per cell.
        let image = crate::keyboard_face::cap(text, keyboard, key, KEY_CAP_PX);
        if image.is_empty() {
            return Ok(());
        }
        let texture = renderer
            .with_context(|gl| unsafe {
                Texture {
                    id: upload_rgba(gl, &image),
                    aspect: image.width as f32 / image.height.max(1) as f32,
                }
            })
            .map_err(|e| format!("no GL context: {e}"))?;
        self.key_caps.insert(id, texture);
        Ok(())
    }

    /// Where the keyboard sits in the world, as the **whole plate** — border included.
    ///
    /// Hung under whatever window has focus, at that window's own distance and facing, so it
    /// reads as belonging to the thing being typed into. Move the window and the keyboard goes
    /// with it; focus another and the keyboard is already there when you look. A keyboard fixed
    /// to the head instead stays put while the window it is feeding slides away, and you end up
    /// typing into one place while looking at another.
    ///
    /// With nothing focused there is no window to hang from, so it falls back to just under the
    /// eye line — which is also where it sits before the first application is opened.
    pub fn keyboard_placement(
        &self,
        focus: Option<&crate::window::Placement>,
        pixels: (u32, u32),
        orientation: glam::DQuat,
        fov: (f64, f64),
        scale: f32,
    ) -> (Vec3, Quat, f32, f32) {
        let outer_aspect = spatiand_shell::keyboard::outer_aspect() as f32;

        let Some(window) = focus else {
            return self.floating_keyboard(outer_aspect, orientation, fov, scale);
        };

        // As wide as the window, within reason. A keyboard matched exactly to a narrow window
        // is unusable and one matched to a very wide one runs past the edges of the field, so
        // the window sets the intent and the field sets the limits.
        let (fit_width, _) = fit_to_fov(outer_aspect, fov.0, fov.1, window.radius as f32);
        let width = ((window.width as f32) * scale).clamp(fit_width * 0.55, fit_width);
        let height = width / outer_aspect;

        // Directly below the window's lower edge, with a gap. Both extents are angles about the
        // viewer rather than metres, because that is what "below" means on a sphere.
        let aspect = pixels.0 as f64 / pixels.1.max(1) as f64;
        let window_height = window.width / aspect.max(0.01);
        // The window's chrome hangs below its content by the frame's thickness.
        let below = window_height * (0.5 + crate::pointer::BORDER_FRACTION);
        let drop =
            (below / window.radius) + KEYBOARD_GAP as f64 + (height as f64 * 0.5 / window.radius);

        let placement = crate::window::Placement {
            yaw: window.yaw,
            pitch: window.pitch - drop,
            radius: window.radius,
            width: width as f64,
        };
        let o = placement.orientation();
        let facing = Quat::from_xyzw(o.x as f32, o.y as f32, o.z as f32, o.w as f32);
        (placement.position().as_vec3(), facing, width, height)
    }

    /// The keyboard with no window to hang from: just under the eye line, body-locked.
    fn floating_keyboard(
        &self,
        aspect: f32,
        orientation: glam::DQuat,
        fov: (f64, f64),
        scale: f32,
    ) -> (Vec3, Quat, f32, f32) {
        let head = Quat::from_xyzw(
            orientation.x as f32,
            orientation.y as f32,
            orientation.z as f32,
            orientation.w as f32,
        );
        // Yaw follows the head; pitch is fixed downward so looking up does not drag it away.
        let (yaw, _, _) = head.to_euler(glam::EulerRot::ZYX);
        // Half the vertical field, so there is still room for something above it.
        let (fit, _) = fit_to_fov(aspect, fov.0, fov.1 * 0.5, KEYBOARD_DISTANCE);
        let width = (fit * scale).min(fit * spatiand_shell::keyboard::MAX_SCALE);
        let height = width / aspect;

        let half_height = (height / 2.0 / KEYBOARD_DISTANCE).atan();
        let pitch = half_height + KEYBOARD_GAP;
        // POSITIVE rotation about +Y pitches DOWN in this frame, because +Y is left. Negating
        // it put the keyboard above the eye line, overlapping the status bar -- which looked
        // like a placement choice rather than a sign error.
        let facing = Quat::from_rotation_z(yaw) * Quat::from_rotation_y(pitch);
        let cfg = spatiand_render::StereoConfig::default();
        let centre = Quat::from_rotation_z(yaw)
            * Vec3::new(cfg.neck_forward_m as f32, 0.0, cfg.neck_up_m as f32)
            + facing * Vec3::X * KEYBOARD_DISTANCE;
        (centre, facing, width, height)
    }

    /// Draw the keyboard: a thin pane of glass with the face of keys on it.
    ///
    /// # Safety
    /// Context must be current.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw_keyboard(
        &self,
        gl: &ffi::Gles2,
        eye: &Eye,
        focus: Option<&crate::window::Placement>,
        pixels: (u32, u32),
        orientation: glam::DQuat,
        fov: (f64, f64),
        keyboard: &spatiand_shell::keyboard::Keyboard,
        border_hot: bool,
        hovered: &[&'static spatiand_shell::keyboard::Key],
    ) {
        let Some(face) = self.keys else {
            return;
        };
        let (centre, facing, width, height) =
            self.keyboard_placement(focus, pixels, orientation, fov, keyboard.scale);

        // The plate is the border: the face sits inside it, and the margin left over is the
        // thing you grab to resize. Drawn with the same glass as a window's frame so the two
        // read as the same material, but far thinner -- see `keyboard::BORDER_FRACTION`.
        let tint = if border_hot {
            [0.72, 0.85, 1.0, 0.95]
        } else {
            [0.46, 0.56, 0.76, 0.85]
        };
        self.quads.draw(
            gl,
            self.glass,
            &(eye.view_projection() * self.panel_model(centre, facing, width, height)),
            tint,
            (0.0, 1.0),
        );

        let (fw, fh) = spatiand_shell::keyboard::face_fraction();
        let (face_w, face_h) = (width * fw as f32, height * fh as f32);
        let model = self.panel_model(centre, facing, face_w, face_h);
        self.quads.draw(
            gl,
            face.id,
            &(eye.view_projection() * model),
            [1.0, 1.0, 1.0, 1.0],
            (0.0, 1.0),
        );

        if hovered.is_empty() {
            return;
        }
        // The key under a pointer, lifted off the face.
        //
        // Lifted *in the world*, along the face's own normal, rather than merely tinted: this
        // is a 3D keyboard and the obvious way for a key to say "you are about to press me" is
        // to stand proud of the ones around it. A flat highlight has to be read; a raised cap
        // is seen.
        //
        // One quad, carrying a picture of that key drawn brighter and with a shadow under it —
        // the same cap in the same place, not a slab laid over it. The picture is exactly its
        // own cell plus the margin its shadow needs, so nothing spills across the keys beside
        // it, and everything outside the cap's rounded outline is transparent.
        let normal = facing * Vec3::X;
        let grow = 1.0 + 2.0 * crate::keyboard_face::CAP_MARGIN;
        for (key, rect) in spatiand_shell::keyboard::layout() {
            if !hovered.iter().any(|k| k.code == key.code) {
                continue;
            }
            let Some(cap) = self
                .key_caps
                .get(&crate::keyboard_face::cap_id(keyboard, key))
            else {
                continue;
            };
            // The cell's middle, in the face's plane. `v` runs down and the world's z runs up.
            let offset = facing
                * Vec3::new(
                    0.0,
                    -((rect.u as f32 - 0.5) * face_w),
                    (0.5 - rect.v as f32) * face_h,
                );
            let at = centre + offset + normal * HOVER_LIFT_M;
            let cap_w = (rect.half_u * 2.0) as f32 * face_w * grow;
            let cap_h = (rect.half_v * 2.0) as f32 * face_h * grow;
            self.quads.draw(
                gl,
                cap.id,
                &(eye.view_projection() * self.panel_model(at, facing, cap_w, cap_h)),
                [1.0, 1.0, 1.0, 1.0],
                (0.0, 1.0),
            );
        }
    }

    /// Rebuild the status bar if its text changed.
    ///
    /// Called every frame, but the text only changes once a minute or when a window opens, so
    /// this is a string comparison almost always.
    pub fn sync_status(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        wanted: &str,
        px_per_degree: f32,
    ) -> Result<(), String> {
        if wanted == self.status_text && self.status.is_some() {
            return Ok(());
        }
        self.status_text = wanted.to_string();
        let image = text.render(wanted, px_per_degree * 0.8, 1400, [214, 226, 248, 255]);
        let old = self.status.take();
        self.status = Some(
            renderer
                .with_context(|gl| unsafe {
                    if let Some(t) = old {
                        gl.DeleteTextures(1, &t.id);
                    }
                    Texture {
                        id: upload_rgba(gl, &image),
                        aspect: image.width as f32 / image.height.max(1) as f32,
                    }
                })
                .map_err(|e| format!("no GL context: {e}"))?,
        );
        Ok(())
    }

    /// Draw the status bar in the upper left, locked to the head.
    ///
    /// Head-locked rather than body-locked: the whole point is that it is there whenever you
    /// glance for it, without having to remember which way you were facing when it appeared.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_status(&self, gl: &ffi::Gles2, eye: &Eye, orientation: glam::DQuat) {
        let Some(bar) = self.status else {
            return;
        };
        // Sized by WIDTH, not height. Sizing by height and letting the aspect decide the
        // width meant a long line ran 25 degrees across and off the side of the field -- the
        // string length silently controlled the layout.
        let distance = 1.5f32;
        let half_width_deg = 7.5f32;
        let width = 2.0 * distance * half_width_deg.to_radians().tan();
        let height = width / bar.aspect.max(0.01);

        // Anchored by its top-left corner rather than its centre, so the bar stays put in the
        // corner whatever it happens to say.
        let left_edge_deg = 16.0f32;
        let yaw = (left_edge_deg - half_width_deg).to_radians();
        // Above a centred window rather than beside it. A default window's top edge reaches
        // about 8.5 degrees, and the bar is 15 degrees wide -- so there is no horizontal
        // position that clears it. Going over the top is the only placement that works, and it
        // leaves the bar inside the 11.57 degree half-field with a little to spare.
        let pitch = 10.2f32.to_radians();
        let head = Quat::from_xyzw(
            orientation.x as f32,
            orientation.y as f32,
            orientation.z as f32,
            orientation.w as f32,
        );
        let direction = head * (Quat::from_rotation_z(yaw) * Quat::from_rotation_y(-pitch));
        let cfg = spatiand_render::StereoConfig::default();
        let centre = head * Vec3::new(cfg.neck_forward_m as f32, 0.0, cfg.neck_up_m as f32)
            + direction * Vec3::X * distance;

        // A plate behind it, or the text is unreadable over a bright environment.
        let backdrop = self.panel_model(centre, direction, width * 1.12, height * 2.0);
        self.quads.draw(
            gl,
            self.white,
            &(eye.view_projection() * backdrop),
            [0.02, 0.03, 0.06, 0.55],
            (0.0, 1.0),
        );
        let model = self.panel_model(centre, direction, width, height);
        self.quads.draw(
            gl,
            bar.id,
            &(eye.view_projection() * model),
            [1.0, 1.0, 1.0, 0.95],
            (0.0, 1.0),
        );
    }

    /// Rasterise the open menu, and work out where its pieces go.
    ///
    /// `model` is `None` in the world and in a launcher that has bubbles to show; the textures
    /// are kept rather than dropped, because a menu that has just been dismissed carries on
    /// being drawn for a moment after the shell has moved on.
    ///
    /// Nothing here depends on which row is selected. That is the point: the selection is a
    /// shape drawn over the rows, so walking the list uploads at most the one line of
    /// explanation that genuinely changed.
    pub fn sync_menu(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        model: Option<&crate::menu::MenuModel>,
        px_per_degree: f32,
        fov: (f64, f64),
    ) -> Result<(), String> {
        use spatiand_render::panel;

        let Some(model) = model else {
            return Ok(());
        };

        // Device pixels per logical panel pixel. Everything is laid out in logical pixels and
        // rasterised at the resolution the wearer's eye actually gets, so the type is sharp
        // rather than a small bitmap scaled up to fill the card.
        let card_deg = (fov.0 * CARD_FOV_FRACTION) as f32;
        let scale = (card_deg * px_per_degree / panel::WIDTH).max(0.05);

        let stale = self
            .menu
            .as_ref()
            .map(|m| (m.scale - scale).abs() > scale * 0.02)
            .unwrap_or(true);
        if stale || model.differs_from(&self.menu_model) {
            let white = [255u8; 4];
            let device = |logical: f32| (logical * scale).max(1.0);
            let content_px = (panel::TEXT_WIDTH * scale) as u32;
            // One line, cropped to its ink, and given far more width than it can use so it
            // never wraps. A row that wrapped would be drawn as two lines squeezed into the
            // height of one; the renderer shrinks an overlong label instead, which keeps a long
            // filename on its own line and readable.
            let line = |t: &mut TextRenderer, s: &str, em: f32| {
                t.render(s, device(em), content_px * 4, white)
            };

            let title = line(text, &model.title, panel::TITLE_EM);
            let rows: Vec<(TextImage, Option<TextImage>)> = model
                .rows
                .iter()
                .map(|row| {
                    (
                        line(text, &row.label, panel::ROW_EM),
                        row.trailing.as_deref().map(|t| line(text, t, TRAILING_EM)),
                    )
                })
                .collect();
            // Set left, under rows that are also set left. Centred text here reads as a
            // caption floating under the card rather than as part of it.
            let detail = (!model.detail.is_empty()).then(|| {
                // The one thing that *should* wrap, so it gets exactly the content width and
                // keeps it: the layout places it at that width, and its height is however
                // many lines it took.
                text.render_aligned(
                    &model.detail,
                    device(panel::DETAIL_EM),
                    content_px,
                    white,
                    spatiand_render::TextAlign::Left,
                )
            });
            let footer =
                (!model.footer.is_empty()).then(|| line(text, &model.footer, panel::FOOTER_EM));

            let old = self.menu.take();
            let built = renderer
                .with_context(|gl| unsafe {
                    if let Some(old) = old {
                        old.destroy(gl);
                    }
                    let upload = |image: &TextImage| Texture {
                        id: upload_rgba(gl, image),
                        aspect: image.width as f32 / image.height.max(1) as f32,
                    };
                    MenuTextures {
                        title: upload(&title),
                        rows: rows
                            .iter()
                            .map(|(label, trailing)| RowTextures {
                                label: upload(label),
                                trailing: trailing.as_ref().map(&upload),
                            })
                            .collect(),
                        // The explanation is padded to the content width, so its height in
                        // logical pixels follows from the image and is the one thing the
                        // layout cannot work out for itself.
                        detail_height: detail
                            .as_ref()
                            .map(|i| i.height as f32 / scale)
                            .unwrap_or(0.0),
                        detail: detail.as_ref().map(&upload),
                        footer: footer.as_ref().map(&upload),
                        scale,
                    }
                })
                .map_err(|e| format!("no GL context: {e}"))?;
            self.menu = Some(built);
            self.menu_model = model.clone();
        } else {
            // The cursor moved and nothing else. Keep the textures, take the new position.
            self.menu_model = model.clone();
        }

        // The card is as tall as its contents up to this, and scrolls beyond it.
        let budget = (fov.1 * CARD_HEIGHT_FRACTION) as f32 * (panel::WIDTH / card_deg);
        let layout = panel::Layout::new(
            &panel::Menu {
                rows: model.rows.len(),
                cursor: model.cursor,
                detail_height: self.menu.as_ref().map(|m| m.detail_height).unwrap_or(0.0),
                footer: self.menu.as_ref().is_some_and(|m| m.footer.is_some()),
                budget_height: budget,
            },
            self.menu_first,
        );
        self.menu_first = layout.first;
        self.menu_layout = Some(layout);
        Ok(())
    }

    /// Draw the application windows.
    ///
    /// Windows are quads on a cylinder around the wearer, drawn back to front so the nearest
    /// is on top. Each carries a thin frame: without one a window with a dark background has
    /// no visible edge against a dark environment, and a window you cannot see the extent of
    /// is very hard to aim a pointer at.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_windows(&self, gl: &ffi::Gles2, eye: &Eye, windows: &[WindowQuad]) {
        let bar_fraction = crate::pointer::TITLE_BAR_FRACTION as f32;
        let mut order: Vec<&WindowQuad> = windows.iter().collect();
        // Furthest first. Everything here is a flat quad at a known distance, so a plain sort
        // is exact and costs nothing -- see the note at the top about there being no depth
        // buffer.
        order.sort_by(|a, b| b.placement.radius.total_cmp(&a.placement.radius));

        for window in order {
            let aspect = window.pixels.0 as f32 / window.pixels.1.max(1) as f32;
            let width = window.placement.width as f32;
            let height = width / aspect.max(0.01);
            let centre = window.placement.position().as_vec3();
            // The placement's own orientation, so the window tips to face the viewer when it
            // is above or below the horizon. Rebuilding it from yaw alone here would quietly
            // undo that for the drawing while leaving the hit-test spherical.
            let o = window.placement.orientation();
            let orientation = Quat::from_xyzw(o.x as f32, o.y as f32, o.z as f32, o.w as f32);
            let model = self.panel_model(centre, orientation, width, height);
            let mvp = eye.view_projection() * model;

            // Frame and title bar are ONE pane of glass behind everything, not a border with
            // a bar resting on it. Two rectangles with different fills read as two objects
            // stuck together; a single rounded sheet reads as the window's chrome.
            // Thick enough to grab. This is the resize target, and it has to be the same
            // number the hit test uses or the wearer aims at a frame that is not where the
            // pointer thinks it is -- which reads as a tracking fault, not a layout one.
            let border = height * crate::pointer::BORDER_FRACTION as f32;
            let bar_height = height * bar_fraction / (1.0 - bar_fraction);
            let chrome_height = height + bar_height + border * 2.0;
            // The sheet covers the content and the bar, so its centre sits above the content's.
            let chrome_centre = centre + (orientation * Vec3::Z) * (bar_height * 0.5);
            let chrome = self.panel_model(
                chrome_centre,
                orientation,
                width + border * 2.0,
                chrome_height,
            );
            let chrome_tint = if window.focused {
                [0.62, 0.76, 1.0, 0.92]
            } else {
                [0.42, 0.47, 0.60, 0.60]
            };
            self.quads.draw(
                gl,
                self.glass,
                &(eye.view_projection() * chrome),
                chrome_tint,
                (0.0, 1.0),
            );

            let bar_centre = centre + (orientation * Vec3::Z) * (height + bar_height) * 0.5;
            if let Some(title) = window.title {
                let label_height = bar_height * 0.62;
                let label_width = label_height * title.aspect.max(0.01);
                let label = self.panel_model(bar_centre, orientation, label_width, label_height);
                self.quads.draw(
                    gl,
                    title.id,
                    &(eye.view_projection() * label),
                    [1.0, 1.0, 1.0, if window.focused { 1.0 } else { 0.7 }],
                    (0.0, 1.0),
                );
            }

            // The bar's furniture: the application's icon at one end, the close button at the
            // other. Both are placed from `Frame`, in the quad's own coordinates, which is the
            // same arithmetic the ray is tested against — so what is drawn and what can be
            // pressed cannot drift apart.
            let frame = crate::pointer::Frame::of(window.pixels);
            let content_height = height;
            let quad = (
                content_height * frame.width() as f32,
                content_height * frame.height() as f32,
            );
            // A box on the chrome, as a model matrix. `v` runs down from the top, the world's
            // z runs up, hence the sign.
            let furniture = |b: crate::pointer::Box2| {
                let offset = orientation
                    * Vec3::new(
                        0.0,
                        -((b.u as f32 - 0.5) * quad.0),
                        (0.5 - b.v as f32) * quad.1,
                    );
                self.panel_model(
                    chrome_centre + offset,
                    orientation,
                    b.half_u as f32 * 2.0 * quad.0,
                    b.half_v as f32 * 2.0 * quad.1,
                )
            };

            if let Some(icon) = window.icon {
                // Inset a little inside its box: an icon drawn to the full square touches the
                // glass around it, and the whole point of the furniture being smaller than the
                // bar is that it reads as sitting *in* the bar.
                let mut box2 = frame.icon();
                box2.half_u *= 0.82;
                box2.half_v *= 0.82;
                self.quads.draw(
                    gl,
                    icon.id,
                    &(eye.view_projection() * furniture(box2)),
                    [1.0, 1.0, 1.0, if window.focused { 1.0 } else { 0.72 }],
                    (0.0, 1.0),
                );
            }

            // The close button. A disc of brighter glass with a cross on it, rather than a
            // bare glyph: a cross alone on a transparent bar is hard to find and impossible to
            // judge the extent of, and the extent is what has to be aimed at.
            //
            // Drawn from the distance field rather than from the glass texture, which is a
            // wide rounded panel and comes out as a squashed rectangle when asked to be a
            // circle. A radius of half the side is a circle by construction, at any size.
            let close = frame.close();
            let hot = window.close_hot;
            let disc = if hot {
                // Red only under the pointer. A window permanently wearing a red button reads
                // as an error, and there are several of them in the room at once.
                [0.98, 0.42, 0.40, 0.92]
            } else if window.focused {
                [0.86, 0.92, 1.0, 0.28]
            } else {
                [0.80, 0.86, 1.0, 0.15]
            };
            let disc_px = 64.0f32;
            self.rounded.draw(
                gl,
                &(eye.view_projection() * furniture(close)),
                disc,
                (disc_px, disc_px),
                disc_px * 0.5,
            );
            let mut cross = close;
            cross.half_u *= 0.46;
            cross.half_v *= 0.46;
            self.quads.draw(
                gl,
                self.close_glyph,
                &(eye.view_projection() * furniture(cross)),
                if hot {
                    [1.0, 1.0, 1.0, 1.0]
                } else {
                    [0.92, 0.95, 1.0, if window.focused { 0.90 } else { 0.55 }]
                },
                (0.0, 1.0),
            );

            // The speaker, and only on windows that make a sound. A mute button on a text
            // editor is a control that does nothing, and there would be one on every window
            // in the room -- so the bar stays as bare as the window's behaviour allows.
            if let Some(sound) = window.sound {
                let mute = frame.mute();
                let hot = window.mute_hot;
                // Lit by how loud it currently is, so a glance across the room says which
                // window the sound is coming from without reading anything.
                let live = sound.peak.clamp(0.0, 1.0).sqrt();
                let disc = if sound.muted {
                    // Plainly off rather than merely dim: a muted window is a state someone
                    // has chosen and has to be able to see they chose.
                    [1.0, 0.55, 0.45, if hot { 0.85 } else { 0.55 }]
                } else if hot {
                    [0.86, 0.94, 1.0, 0.60]
                } else {
                    [0.80, 0.90, 1.0, 0.16 + 0.34 * live]
                };
                self.rounded.draw(
                    gl,
                    &(eye.view_projection() * furniture(mute)),
                    disc,
                    (disc_px, disc_px),
                    disc_px * 0.5,
                );
                let mut glyph = mute;
                glyph.half_u *= 0.46;
                glyph.half_v *= 0.46;
                self.quads.draw(
                    gl,
                    if sound.muted {
                        self.speaker_off_glyph
                    } else {
                        self.speaker_glyph
                    },
                    &(eye.view_projection() * furniture(glyph)),
                    if hot || sound.muted {
                        [1.0, 1.0, 1.0, 1.0]
                    } else {
                        [0.92, 0.95, 1.0, if window.focused { 0.90 } else { 0.55 }]
                    },
                    (0.0, 1.0),
                );
            }

            // Client textures arrive with GL's *default* sampler state, which is
            // NEAREST_MIPMAP_LINEAR. A texture with no mipmaps and a mipmap filter is
            // incomplete, and an incomplete texture samples as opaque black -- so the window
            // draws as a perfect black rectangle while the import is entirely correct. That is
            // a genuinely nasty failure: nothing errors, and reading the texture back shows
            // full content.
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, window.texture);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );

            // The surface itself. Fully opaque: a client's own transparency would otherwise
            // let the environment through, and a half-transparent terminal floating in a room
            // is unreadable.
            self.quads
                .draw(gl, window.texture, &mvp, [1.0, 1.0, 1.0, 1.0], (0.0, 1.0));

            // Menus and dropdowns, on the window's own plane and a hair in front of it.
            //
            // There is no depth buffer here -- everything is a flat quad sorted by distance --
            // so "in front" has to be a real displacement along the normal rather than a
            // depth test. A millimetre is far too little to see as a gap at arm's length and
            // far more than enough to stop the two coplanar quads fighting over which pixel
            // belongs to whom, which shows up as the menu flickering as the head moves.
            let normal = orientation * Vec3::X;
            let per_pixel = (
                width / window.pixels.0.max(1) as f32,
                height / window.pixels.1.max(1) as f32,
            );
            for (depth, popup) in window.popups.iter().enumerate() {
                let popup_w = popup.pixels.0 as f32 * per_pixel.0;
                let popup_h = popup.pixels.1 as f32 * per_pixel.1;
                // Centre of the popup in the parent's pixels, as a fraction across it. `v`
                // runs down from the top and the world's z runs up, hence the sign -- the same
                // arithmetic as the title bar's furniture, and for the same reason: what is
                // drawn and what can be pressed must not be able to drift apart.
                let u = (popup.offset.0 as f32 + popup.pixels.0 as f32 * 0.5)
                    / window.pixels.0.max(1) as f32;
                let v = (popup.offset.1 as f32 + popup.pixels.1 as f32 * 0.5)
                    / window.pixels.1.max(1) as f32;
                let offset = orientation * Vec3::new(0.0, -(u - 0.5) * width, (0.5 - v) * height);
                // Submenus stack, so each one steps a little further forward than the last.
                let lift = normal * (POPUP_LIFT_M * (depth as f32 + 1.0));
                let model = self.panel_model(centre + offset + lift, orientation, popup_w, popup_h);
                gl.BindTexture(ffi::TEXTURE_2D, popup.texture);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                self.quads.draw(
                    gl,
                    popup.texture,
                    &(eye.view_projection() * model),
                    [1.0, 1.0, 1.0, 1.0],
                    (0.0, 1.0),
                );
            }
        }
    }

    /// Draw whichever menu is open.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_menu(&self, gl: &ffi::Gles2, eye: &Eye, shell: &Shell, fov: (f64, f64)) {
        // While a menu is leaving, the shell has already returned to the world -- so the mode
        // to draw comes from the dismissal, not from the shell.
        let mode = match shell.mode() {
            Mode::World => match self.dismissing_mode() {
                Some(mode) => mode,
                None => return,
            },
            other => other,
        };
        match mode {
            Mode::World => {}
            // All three are the same thing to draw: a card of rows with one selected. The
            // launcher is the odd one out because it is bubbles in space, not a list.
            Mode::Hud | Mode::Environment | Mode::Files => self.draw_card(gl, eye, fov),
            Mode::Launcher => self.draw_launcher(gl, eye, shell, fov),
        }
    }

    /// Draw a menu as a card: ground, selection, rows, explanation, hints.
    ///
    /// Every rectangle comes from the layout, in logical panel pixels, and is placed on the
    /// card's plane by `place` below. Nothing here decides where anything goes — that is
    /// deliberate, because layout arithmetic is testable and drawing is not.
    unsafe fn draw_card(&self, gl: &ffi::Gles2, eye: &Eye, fov: (f64, f64)) {
        use spatiand_render::panel;

        let (Some(menu), Some(layout)) = (self.menu.as_ref(), self.menu_layout.as_ref()) else {
            return;
        };
        let appear = self.appear_progress(0).clamp(0.0, 1.0);
        if appear <= 0.001 {
            return;
        }

        // Metres per logical pixel. The card grows the last three percent as it arrives, which
        // reads as it coming towards you rather than fading up out of nothing — small enough
        // that nobody watching it a hundredth time has to wait for it.
        let card_width =
            2.0 * MENU_DISTANCE * ((fov.0 * CARD_FOV_FRACTION / 2.0).to_radians().tan() as f32);
        let metres = (card_width / panel::WIDTH) * (0.97 + 0.03 * appear);

        let centre = self.menu_centre(MENU_DISTANCE);
        let quat = self.anchor_quat();
        let right = quat * -Vec3::Y;
        let up = quat * Vec3::Z;
        let vp = eye.view_projection();

        // A logical rectangle, as a model matrix on the card's plane.
        let place = |r: panel::Rect| {
            let dx = (r.x + r.w * 0.5 - panel::WIDTH * 0.5) * metres;
            let dy = (layout.height * 0.5 - (r.y + r.h * 0.5)) * metres;
            self.panel_model(
                centre + right * dx + up * dy,
                quat,
                r.w * metres,
                r.h * metres,
            )
        };
        let fade = |c: [f32; 4]| [c[0], c[1], c[2], c[3] * appear];
        // Sizes go to the shader in logical pixels, so the corner radius and the one pixel of
        // antialiasing along the edge are both in the units the layout is written in.
        let panel_rect = |r: panel::Rect, tint: [f32; 4], radius: f32| {
            self.rounded
                .draw(gl, &(vp * place(r)), fade(tint), (r.w, r.h), radius);
        };
        let card = panel::Rect {
            x: 0.0,
            y: 0.0,
            w: panel::WIDTH,
            h: layout.height,
        };

        panel_rect(
            panel::Rect {
                x: -panel::RIM,
                y: -panel::RIM,
                w: card.w + panel::RIM * 2.0,
                h: card.h + panel::RIM * 2.0,
            },
            CARD_RIM,
            panel::CARD_RADIUS + panel::RIM,
        );
        panel_rect(card, CARD_GROUND, panel::CARD_RADIUS);

        // One piece of text inside a band, at its left or right edge, vertically centred.
        //
        // A label wider than its band is scaled down rather than clipped or ellipsised. A long
        // filename is the case: shrinking one row is ugly, and cutting a name off in the middle
        // is worse, because the end of a filename is the part that says what it is.
        let text_in =
            |band: panel::Rect, tex: &Texture, em: f32, tint: [f32; 4], right_edge: bool| {
                let mut h = em * 1.4;
                let mut w = h * tex.aspect.max(0.01);
                if w > band.w {
                    h *= band.w / w;
                    w = band.w;
                }
                let r = panel::Rect {
                    x: if right_edge {
                        band.x + band.w - w
                    } else {
                        band.x
                    },
                    y: band.y + (band.h - h) * 0.5,
                    w,
                    h,
                };
                self.quads
                    .draw(gl, tex.id, &(vp * place(r)), fade(tint), (0.0, 1.0));
            };

        text_in(layout.title, &menu.title, panel::TITLE_EM, INK, false);
        if let (Some(band), Some(tex)) = (layout.footer, menu.footer.as_ref()) {
            text_in(band, tex, panel::FOOTER_EM, INK_HINT, true);
        }

        for (offset, rect) in layout.rows.iter().enumerate() {
            let index = layout.first + offset;
            let Some(row) = menu.rows.get(index) else {
                continue;
            };
            let selected = index == self.menu_model.cursor;
            if selected {
                panel_rect(*rect, ROW_SELECTED, panel::ROW_RADIUS);
                panel_rect(
                    panel::Rect {
                        x: rect.x + 9.0,
                        y: rect.y + rect.h * 0.26,
                        w: 5.0,
                        h: rect.h * 0.48,
                    },
                    ROW_MARK,
                    2.5,
                );
            }

            // The trailing status is placed first, because how much room it takes is what the
            // label has left.
            let mut label_width = rect.w - panel::ROW_INSET * 2.0;
            if let Some(tex) = row.trailing.as_ref() {
                let band = panel::Rect {
                    x: rect.x + panel::ROW_INSET,
                    w: rect.w - panel::ROW_INSET * 2.0,
                    ..*rect
                };
                text_in(band, tex, TRAILING_EM, INK_TRAILING, true);
                let taken = TRAILING_EM * 1.4 * tex.aspect.max(0.01);
                label_width = (label_width - taken - panel::ROW_INSET).max(panel::ROW_INSET);
            }
            text_in(
                panel::Rect {
                    x: rect.x + panel::ROW_INSET,
                    w: label_width,
                    ..*rect
                },
                &row.label,
                panel::ROW_EM,
                if selected { INK } else { INK_ROW },
                false,
            );
        }

        if let Some(track) = layout.scroll_track {
            panel_rect(track, SCROLL_TRACK, track.w * 0.5);
        }
        if let Some(thumb) = layout.scroll_thumb {
            panel_rect(thumb, SCROLL_THUMB, thumb.w * 0.5);
        }

        if let Some(sep) = layout.separator {
            panel_rect(sep, SEPARATOR, sep.h * 0.5);
        }
        if let (Some(rect), Some(tex)) = (layout.detail, menu.detail.as_ref()) {
            // Already padded to the content width, so it goes exactly where the layout says.
            self.quads.draw(
                gl,
                tex.id,
                &(vp * place(rect)),
                fade(INK_DETAIL),
                (0.0, 1.0),
            );
        }
    }

    unsafe fn draw_launcher(&self, gl: &ffi::Gles2, eye: &Eye, shell: &Shell, fov: (f64, f64)) {
        let launcher = shell.launcher();
        if launcher.is_empty() {
            // Say so, rather than showing an empty sky that looks like a failure to open.
            self.draw_card(gl, eye, fov);
            return;
        }

        self.bubbles.begin(
            gl,
            self.sky,
            self.eye_rect(eye.side),
            self.sky_source.yaw_offset_radians(),
            self.sky_source.projection == SkyProjection::Equirect180,
        );

        for (index, placement) in launcher.placements() {
            let yaw = placement.yaw + self.anchor_yaw;
            let orientation = Quat::from_rotation_z(yaw) * Quat::from_rotation_y(-placement.pitch);
            let centre = self.menu_origin() + orientation * Vec3::X * placement.radius;
            let appear = self.appear_progress(index);
            if appear <= 0.001 {
                continue;
            }
            // Arriving bubbles are smaller and closer to their final place rather than flying
            // in from somewhere: a bubble that travels has to be tracked by the eye, and there
            // are twelve of them.
            let size = BUBBLE_DIAMETER_M * placement.scale * (0.72 + 0.28 * appear);
            let focus = if placement.scale > 1.0 { 1.0 } else { 0.0 };

            let model = self.panel_model(centre, orientation, size, size);
            let basis = Mat3::from_quat(orientation) * Mat3::from_cols(-Vec3::Y, Vec3::Z, Vec3::X);
            let eye_pos = eye.position.as_vec3();
            self.bubbles.draw(
                gl,
                &BubbleParams {
                    mvp: &(eye.view_projection() * model),
                    basis,
                    view_dir: (centre - eye_pos).normalize_or_zero(),
                    light_dir: LIGHT_DIR.normalize(),
                    focus,
                    icon: self.app_glyphs.get(index).map(|t| t.id),
                    accent: [0.62, 0.78, 1.0, 1.0],
                    appear,
                },
            );
        }

        // Page dots down the right-hand side, so it is obvious there is more above or below.
        // Without them a paged grid is indistinguishable from a short one, and nobody presses
        // down past the last visible row to find out.
        let pages = launcher.pages();
        if pages > 1 {
            let current = launcher.page();
            let spacing = 3.2f32.to_radians();
            let side = (COLUMN_SPACING_DEG_LOCAL * 2.2).to_radians();
            for page in 0..pages {
                let offset = page as f32 - (pages as f32 - 1.0) * 0.5;
                let orientation = Quat::from_rotation_z(self.anchor_yaw - side)
                    * Quat::from_rotation_y(offset * spacing);
                let centre = self.menu_origin()
                    + orientation * Vec3::X * spatiand_shell::launcher::ARC_RADIUS_M;
                let size = if page == current { 0.020f32 } else { 0.012 };
                let alpha = if page == current { 0.95f32 } else { 0.40 };
                let model = self.panel_model(centre, orientation, size, size);
                self.quads.draw(
                    gl,
                    self.reticle,
                    &(eye.view_projection() * model),
                    [0.78, 0.86, 1.0, alpha],
                    (0.0, 1.0),
                );
            }
        }

        // Labels last, so they are never occluded by a neighbouring bubble's glass.
        for (index, placement) in launcher.placements() {
            let Some(label) = self.app_labels.get(index) else {
                continue;
            };
            let yaw = placement.yaw + self.anchor_yaw;
            let orientation = Quat::from_rotation_z(yaw) * Quat::from_rotation_y(-placement.pitch);
            // Clear of the glass even when the bubble is the focused one and 18% larger --
            // measured against that, not against the resting size, or the label rides up onto
            // the icon of whichever bubble you are actually looking at.
            let drop = BUBBLE_DIAMETER_M * 0.5 * placement.scale + 0.022;
            let centre =
                self.menu_origin() + orientation * Vec3::X * placement.radius - Vec3::Z * drop;
            let height = 0.022f32;
            let width = height * label.aspect.max(0.01);
            let model = self.panel_model(centre, orientation, width, height);
            let alpha =
                if placement.scale > 1.0 { 1.0 } else { 0.55 } * self.appear_progress(index);
            self.quads.draw(
                gl,
                label.id,
                &(eye.view_projection() * model),
                [1.0, 1.0, 1.0, alpha],
                (0.0, 1.0),
            );
        }
    }

    /// Draw the pointer where its ray lands.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_pointer(
        &self,
        gl: &ffi::Gles2,
        eye: &Eye,
        ray: &Ray,
        hit: Option<Hit>,
        aimed_by: Pointing,
        cursor: Cursor,
        fade: f32,
    ) {
        // With nothing under the pointer, park the reticle at a fixed distance so it is still
        // visible. A cursor that vanishes whenever it leaves a window is impossible to aim.
        let distance = hit.map(|h| h.distance as f32).unwrap_or(2.5);
        let point = (ray.origin + ray.direction * distance as f64).as_vec3();
        // Constant angular size: scale with distance so it neither shrinks into nothing far
        // away nor swallows the view up close.
        let size = 2.0 * distance * (RETICLE_DEG * 0.5).to_radians().tan();

        let eye_pos = eye.position.as_vec3();
        let to_eye = (eye_pos - point).normalize_or_zero();
        // Face the eye. Build the quad's own basis directly rather than solving for a
        // quaternion: the reticle has no meaningful roll, so any up vector not parallel to
        // the view will do, and world up is the one that keeps it steady.
        let right = Vec3::Z.cross(to_eye).normalize_or(Vec3::Y);
        let up = to_eye.cross(right).normalize_or(Vec3::Z);
        // A resize arrow is turned within that basis to lie along the edge it will drag, and
        // is drawn a little larger -- it has to say which way as well as where, and a shape
        // the size of the aiming reticle cannot show an angle.
        let (angle, scale) = match cursor {
            Cursor::Point => (0.0f32, 1.0f32),
            Cursor::Resize { angle_deg } => (angle_deg.to_radians(), 1.5),
        };
        let (sin, cos) = angle.sin_cos();
        // Both axes from the *original* basis. Rotating one and then using it to rotate the
        // other is not a rotation at all -- it skews the quad and the arrow stops being
        // straight.
        let (turned_right, turned_up) = (right * cos + up * sin, up * cos - right * sin);
        let (right, up) = (turned_right, turned_up);
        let size = size * scale;
        let model = Mat4::from_cols(
            (right * size).extend(0.0),
            (up * size).extend(0.0),
            to_eye.extend(0.0),
            point.extend(1.0),
        );

        let hue = aimed_by.hue();
        let alpha = if hit.is_some() { 0.95 } else { 0.5 } * fade;
        let tint = [hue[0], hue[1], hue[2], alpha];
        self.quads.draw(
            gl,
            match cursor {
                Cursor::Resize { .. } => self.resize_cursor,
                Cursor::Point if aimed_by == Pointing::LeftThumb => self.reticle_left,
                Cursor::Point => self.reticle,
            },
            &(eye.view_projection() * model),
            tint,
            (0.0, 1.0),
        );
    }

    /// Draw a pointer complete with its beam.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_pointer_ray(
        &self,
        gl: &ffi::Gles2,
        eye: &Eye,
        orientation: glam::DQuat,
        ray: &Ray,
        hit: Option<Hit>,
        aimed_by: Pointing,
        cursor: Cursor,
        fade: f32,
    ) {
        // A beam comes from a hand. A mouse is on a table and has none, and drawing one from
        // the middle of the wearer's chest to their cursor looks like a fault rather than a
        // pointer -- so the mouse gets the reticle and nothing else.
        if let Some(anchor) = Self::pointer_anchor(orientation, aimed_by) {
            let distance = hit.map(|h| h.distance as f32).unwrap_or(2.5);
            let point = (ray.origin + ray.direction * distance as f64).as_vec3();
            let hue = aimed_by.hue();
            let alpha = if hit.is_some() { 0.55 } else { 0.28 } * fade;
            self.draw_beam(gl, eye, anchor, point, [hue[0], hue[1], hue[2], alpha]);
        }
        self.draw_pointer(gl, eye, ray, hit, aimed_by, cursor, fade);
    }

    /// Where a pointer's beam starts, or `None` for one that has no hand behind it.
    fn pointer_anchor(orientation: glam::DQuat, aimed_by: Pointing) -> Option<Vec3> {
        match aimed_by {
            Pointing::RightThumb => Some(Self::hand_anchor(orientation, true)),
            Pointing::LeftThumb => Some(Self::hand_anchor(orientation, false)),
            Pointing::Mouse => None,
        }
    }

    /// Draw a beam between two points, turned to face the eye.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_beam(
        &self,
        gl: &ffi::Gles2,
        eye: &Eye,
        from: Vec3,
        to: Vec3,
        colour: [f32; 4],
    ) {
        let along = to - from;
        let length = along.length();
        if length < 1e-4 {
            return;
        }
        let direction = along / length;
        let middle = (from + to) * 0.5;
        let to_eye = (eye.position.as_vec3() - middle).normalize_or(Vec3::X);
        // Billboard: the quad's width runs perpendicular to both the beam and the view, so it
        // stays visible however the beam is angled. A beam viewed exactly end-on collapses,
        // which is correct -- it is pointing at your eye.
        let side = direction.cross(to_eye).normalize_or_zero();
        if side.length_squared() < 1e-6 {
            return;
        }
        // Thin, and slightly thicker further away so it does not vanish at distance.
        let width = 0.004 + length * 0.0016;
        let model = Mat4::from_cols(
            (side * width).extend(0.0),
            (direction * length).extend(0.0),
            to_eye.extend(0.0),
            middle.extend(1.0),
        );
        self.quads.draw(
            gl,
            self.white,
            &(eye.view_projection() * model),
            colour,
            (0.0, 1.0),
        );
    }

    /// Where a beam should appear to come from, for one hand.
    ///
    /// Roughly where the Deck is held: below the eyes, a little forward, offset to the correct
    /// side. Not a tracked position -- there is nothing tracking the hands -- but a plausible
    /// one, and plausible is all a beam's origin has to be for the gesture to read correctly.
    pub fn hand_anchor(orientation: glam::DQuat, right_hand: bool) -> Vec3 {
        let head = Quat::from_xyzw(
            orientation.x as f32,
            orientation.y as f32,
            orientation.z as f32,
            orientation.w as f32,
        );
        let lateral = if right_hand { -0.13 } else { 0.13 };
        head * Vec3::new(0.16, lateral, -0.30)
    }

    /// Model matrix for a quad of `width` x `height` metres centred at `centre`.
    ///
    /// The quad's local axes follow the world's: its +x runs along −Y (rightwards, since +Y is
    /// left), its +y along +Z, and its normal along +X.
    fn panel_model(&self, centre: Vec3, orientation: Quat, width: f32, height: f32) -> Mat4 {
        let basis = Mat4::from_cols(
            (-Vec3::Y * width).extend(0.0),
            (Vec3::Z * height).extend(0.0),
            Vec3::X.extend(0.0),
            Vec4::W,
        );
        Mat4::from_translation(centre) * Mat4::from_quat(orientation) * basis
    }

    fn anchor_quat(&self) -> Quat {
        Quat::from_rotation_z(self.anchor_yaw)
    }

    /// Centre of a body-locked menu, in world space.
    ///
    /// Measured from the eye rather than the pivot: the neck model lifts the eyes about 7.5 cm,
    /// and a menu centred on the origin therefore hangs a few degrees low - which costs it its
    /// bottom row off the edge of the field.
    fn menu_centre(&self, distance: f32) -> Vec3 {
        self.menu_origin() + self.anchor_quat() * Vec3::X * distance
    }

    /// Where the eyes are when facing the menu's anchor.
    ///
    /// Everything body-locked hangs off this rather than off the world origin. The neck model
    /// puts the eyes ~7.5 cm above the pivot, which at a 2 m radius is 2.15° - small enough to
    /// sound ignorable and large enough to push the bottom row of a three-row grid off the
    /// edge of a 23° field, which is exactly what it did.
    fn menu_origin(&self) -> Vec3 {
        let cfg = spatiand_render::StereoConfig::default();
        self.anchor_quat() * Vec3::new(cfg.neck_forward_m as f32, 0.0, cfg.neck_up_m as f32)
    }
}

/// Largest quad of a given aspect ratio that fits inside a field of view at `distance`.
///
/// Shared by everything that has to sit in front of the wearer. The margin is not politeness:
/// the glasses' usable area is meaningfully smaller than their nominal field, and content that
/// reaches the nominal edge is already unreadable.
fn fit_to_fov(aspect: f32, h_fov_deg: f64, v_fov_deg: f64, distance: f32) -> (f32, f32) {
    let usable = 0.68;
    let extent = |fov: f64| 2.0 * distance * ((fov * usable / 2.0).to_radians().tan() as f32);
    let aspect = aspect.max(0.01);
    let width = extent(h_fov_deg).min(extent(v_fov_deg) * aspect);
    (width, width / aspect)
}

/// A pane of glass: rounded, with a lit top edge and a soft rim.
///
/// Used for the window frame and title bar, drawn as one piece so they read as a single object
/// rather than a bar sitting on a border. A flat quad cannot do that -- it has hard corners
/// and a uniform fill, and next to the refracting bubbles it looks like a different program.
///
/// Generated rather than shipped, like everything else here: it is a gradient and a rounded
/// rectangle, and an asset would be one more thing to install and license.
fn glass_panel_image(width: u32, height: u32, corner: f32) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    let (w, h) = (width as f32, height as f32);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance outside a rounded rectangle, in pixels. Negative inside.
            let dx = (corner - fx).max(fx - (w - corner)).max(0.0);
            let dy = (corner - fy).max(fy - (h - corner)).max(0.0);
            let outside = (dx * dx + dy * dy).sqrt() - corner;
            // One pixel of feathering: enough to kill the jaggies, little enough that the edge
            // still reads as an edge at this angular size.
            let coverage = (1.0 - (outside + corner.min(1.5))).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }

            // Vertical gradient, lighter at the top, the way a sheet of glass catches a room.
            let t = fy / h;
            let body = 0.24 - 0.14 * t;
            // A bright line just inside the top edge, and a dimmer one at the bottom.
            let from_top = fy / h.max(1.0);
            let lip = (1.0 - (from_top * 26.0).min(1.0)).powf(1.6) * 0.55;
            let base = (1.0 - ((1.0 - from_top) * 34.0).min(1.0)).powf(2.0) * 0.16;
            // Rim: brighter within a couple of pixels of the outline, all the way round.
            let rim = (1.0 - ((-outside) / 2.5).clamp(0.0, 1.0)).powf(1.5) * 0.45;

            let light = (body + lip + base + rim).clamp(0.0, 1.0);
            let i = ((y * width + x) * 4) as usize;
            out[i] = (light * 255.0) as u8;
            out[i + 1] = (light * 255.0) as u8;
            out[i + 2] = (light * 255.0) as u8;
            out[i + 3] = ((0.30 + light * 0.72).min(1.0) * coverage * 255.0) as u8;
        }
    }
    out
}

/// Which cursor to draw, and which way round.
///
/// Deliberately not `pointer::Zone`: the scene draws things and should not have to know what a
/// window frame is. The mapping between the two lives at the one call site that knows both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cursor {
    /// The ordinary aiming reticle.
    Point,
    /// A double arrow, lying at this angle in the plane facing the eye. 0° is horizontal and
    /// positive turns anticlockwise, so a bottom-left corner is +45°.
    Resize { angle_deg: f32 },
}

impl Cursor {
    /// The angle an edge's arrow lies at.
    pub fn for_edge(edge: crate::pointer::Edge) -> Self {
        use crate::pointer::Edge;
        Cursor::Resize {
            angle_deg: match edge {
                Edge::Left | Edge::Right => 0.0,
                Edge::Bottom => 90.0,
                // Along the diagonal the corner sits on: a bottom-left corner is dragged
                // down-and-left, so its arrow runs from lower-left to upper-right.
                Edge::BottomLeft => 45.0,
                Edge::BottomRight => -45.0,
            },
        }
    }
}

/// The resize cursor: a double-headed arrow, drawn along the texture's horizontal.
///
/// One texture for all five edges, turned in the plane of the quad when it is drawn. Four
/// separate images would be four chances for one of them to be a degree off the axis it
/// claims — and this way the arrow is guaranteed to line up with the edge it belongs to,
/// because the same angle places both.
fn resize_cursor_image(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let centre = (size as f32 - 1.0) * 0.5;
    let radius = centre;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - centre) / radius;
            let dy = (y as f32 - centre) / radius;
            // The shaft: a thin horizontal bar stopping short of the ends.
            let shaft = if dx.abs() < 0.62 && dy.abs() < 0.07 {
                1.0 - (dy.abs() / 0.07)
            } else {
                0.0
            };
            // A head at each end. Written as a triangle that narrows towards the tip, so the
            // two arrows read as pointing outwards rather than as a barbell.
            let head = |tip: f32| {
                let along = (dx - tip).abs();
                let depth = 0.30;
                if along > depth {
                    return 0.0;
                }
                let half = 0.26 * (1.0 - along / depth);
                if dy.abs() > half {
                    0.0
                } else {
                    1.0
                }
            };
            let a = shaft.max(head(-0.70)).max(head(0.70)).clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let i = ((y * size + x) * 4) as usize;
            out[i] = 255;
            out[i + 1] = 255;
            out[i + 2] = 255;
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    out
}

/// The cross on a window's close button.
///
/// Generated like every other glyph here rather than shipped or shaped from a font: it is two
/// lines, and a font would make the one mark on a window that everybody recognises depend on
/// which fonts happen to be installed.
///
/// The strokes are drawn as a distance to the diagonal rather than by walking pixels, so the
/// edges are antialiased. A hard-edged cross a degree across, seen through optics, reads as a
/// smudge — the softness is what makes it look like a drawn mark at this size.
/// A speaker, with or without a line through it.
///
/// Drawn rather than shaped from a font, for the same reason the close cross is: the one
/// character that means this is an emoji, whose colour and metrics vary by whichever font
/// happens to be installed, and a control has to look the same on every machine.
///
/// Supersampled rather than distance-fielded. The shape is a handful of half-plane and
/// circle tests, and counting how many of a grid of samples land inside is both shorter than
/// the equivalent distance field and exactly as smooth at this size.
fn speaker_glyph_image(size: u32, muted: bool) -> Vec<u8> {
    /// How far out the cone flares, and where the body ends.
    const BODY: (f32, f32) = (-0.62, -0.28);
    const CONE_END: f32 = 0.10;
    const BODY_HALF: f32 = 0.26;
    const CONE_HALF: f32 = 0.62;

    let inside = |x: f32, y: f32| -> bool {
        // The box the diaphragm sits in.
        if x >= BODY.0 && x <= BODY.1 && y.abs() <= BODY_HALF {
            return true;
        }
        // The cone, widening linearly to its mouth.
        if x > BODY.1 && x <= CONE_END {
            let t = (x - BODY.1) / (CONE_END - BODY.1);
            if y.abs() <= BODY_HALF + t * (CONE_HALF - BODY_HALF) {
                return true;
            }
        }
        let (dx, dy) = (x - CONE_END, y);
        if muted {
            // A cross where the waves would be. A single slash was tried first and read as
            // one more wave at the size this is actually drawn -- the two strokes are what
            // make it unmistakably "not sounding" rather than "sounding a bit".
            let (ax, ay) = (x - 0.52, y);
            let arm = |across: f32, along: f32| {
                across.abs() * std::f32::consts::FRAC_1_SQRT_2 <= 0.075
                    && along.abs() * std::f32::consts::FRAC_1_SQRT_2 <= 0.30
            };
            return arm(ax - ay, ax + ay) || arm(ax + ay, ax - ay);
        }
        // Two arcs in front of it. Bounded by angle as well as radius, so they are arcs
        // rather than rings drawn round the back of the speaker.
        if dx <= 0.0 {
            return false;
        }
        let r = (dx * dx + dy * dy).sqrt();
        if dy.abs() > dx * 1.30 {
            return false;
        }
        [0.36f32, 0.60].iter().any(|ring| (r - ring).abs() <= 0.065)
    };

    let mut out = vec![0u8; (size * size * 4) as usize];
    let centre = (size as f32 - 1.0) * 0.5;
    let radius = centre;
    // A three-by-three grid inside each pixel, which is enough at this size and costs nothing
    // for a texture built once per session.
    const GRID: i32 = 3;
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0;
            for sy in 0..GRID {
                for sx in 0..GRID {
                    let ox = (sx as f32 + 0.5) / GRID as f32 - 0.5;
                    let oy = (sy as f32 + 0.5) / GRID as f32 - 0.5;
                    let px = (x as f32 + ox - centre) / radius;
                    let py = (y as f32 + oy - centre) / radius;
                    if inside(px, py) {
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let a = (hits as f32 / (GRID * GRID) as f32 * 255.0) as u8;
            let i = ((y * size + x) * 4) as usize;
            // White, with the coverage in alpha: the drawing tints it.
            out[i] = 255;
            out[i + 1] = 255;
            out[i + 2] = 255;
            out[i + 3] = a;
        }
    }
    out
}

#[cfg(test)]
mod glyph_tests {
    use super::*;

    /// Coverage of the glyph inside a box, as a fraction, for asking where the ink is.
    fn ink(image: &[u8], size: u32, x0: f32, x1: f32, y0: f32, y1: f32) -> f32 {
        let mut sum = 0.0;
        let mut count = 0.0;
        for y in 0..size {
            for x in 0..size {
                let px = x as f32 / size as f32;
                let py = y as f32 / size as f32;
                if px >= x0 && px < x1 && py >= y0 && py < y1 {
                    sum += image[((y * size + x) * 4 + 3) as usize] as f32 / 255.0;
                    count += 1.0;
                }
            }
        }
        if count > 0.0 {
            sum / count
        } else {
            0.0
        }
    }

    #[test]
    fn the_speaker_has_a_body_on_the_left_and_waves_on_the_right() {
        // Not a picture test -- it asserts the thing is the shape of a speaker rather than a
        // blob, which is what would be left if the arithmetic were wrong in a way that still
        // produced ink.
        let g = speaker_glyph_image(64, false);
        let body = ink(&g, 64, 0.20, 0.36, 0.42, 0.58);
        let cone = ink(&g, 64, 0.40, 0.52, 0.30, 0.70);
        let waves = ink(&g, 64, 0.62, 0.90, 0.35, 0.65);
        let above = ink(&g, 64, 0.20, 0.36, 0.02, 0.18);
        assert!(body > 0.9, "the body is not solid: {body}");
        assert!(cone > 0.5, "the cone is missing: {cone}");
        assert!(waves > 0.05, "there are no waves: {waves}");
        assert!(above < 0.02, "there is ink above the body: {above}");
    }

    #[test]
    fn the_muted_speaker_keeps_its_body_and_loses_its_waves() {
        // The two have to read as the same object in two states, or the button appears to
        // change into something else when pressed.
        let on = speaker_glyph_image(64, false);
        let off = speaker_glyph_image(64, true);
        let body = |g: &[u8]| ink(g, 64, 0.20, 0.36, 0.42, 0.58);
        assert!((body(&on) - body(&off)).abs() < 0.02, "the body moved");
        // The cross sits where the near wave was, so compare the far one, which only the
        // sounding glyph reaches.
        let far = |g: &[u8]| ink(g, 64, 0.80, 0.95, 0.36, 0.64);
        assert!(
            far(&on) > far(&off) + 0.03,
            "muting did not remove the waves"
        );
    }

    #[test]
    fn the_glyph_stays_inside_its_own_square() {
        // It is drawn into a round disc, so anything reaching the corners is clipped by the
        // glass rather than by the design.
        for muted in [false, true] {
            let g = speaker_glyph_image(64, muted);
            for edge in [
                ink(&g, 64, 0.0, 0.04, 0.0, 1.0),
                ink(&g, 64, 0.96, 1.0, 0.0, 1.0),
                ink(&g, 64, 0.0, 1.0, 0.0, 0.04),
                ink(&g, 64, 0.0, 1.0, 0.96, 1.0),
            ] {
                assert!(edge < 0.01, "muted={muted}: ink at the very edge: {edge}");
            }
        }
    }

    #[test]
    #[ignore = "writes a picture to look at rather than asserting anything"]
    fn draw_the_speaker_glyphs() {
        for (name, muted) in [("speaker-on", false), ("speaker-off", true)] {
            let g = speaker_glyph_image(256, muted);
            // On a mid grey, because the glyph is white and its alpha is the whole shape.
            let mut flat = vec![0u8; 256 * 256 * 3];
            for i in 0..256 * 256 {
                let a = g[i * 4 + 3] as f32 / 255.0;
                for c in 0..3 {
                    flat[i * 3 + c] = (60.0 + a * 195.0) as u8;
                }
            }
            let path = format!("/tmp/{name}.png");
            image::save_buffer(&path, &flat, 256, 256, image::ColorType::Rgb8).unwrap();
            println!("wrote {path}");
        }
    }
}

fn close_glyph_image(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let centre = (size as f32 - 1.0) * 0.5;
    let radius = centre;
    // Half the stroke width, and how far each arm reaches, both as a fraction of the radius.
    let half_stroke = 0.085f32;
    let reach = 0.90f32;
    let feather = 1.5 / radius;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - centre) / radius;
            let dy = (y as f32 - centre) / radius;
            // Distance to each of the two diagonals, and how far along it the point lies.
            let arm = |across: f32, along: f32| {
                if along.abs() > reach {
                    return 0.0;
                }
                let d = across.abs() * std::f32::consts::FRAC_1_SQRT_2;
                (1.0 - (d - half_stroke) / feather).clamp(0.0, 1.0)
            };
            let a = arm(dx - dy, dx + dy).max(arm(dx + dy, dx - dy));
            if a <= 0.0 {
                continue;
            }
            let i = ((y * size + x) * 4) as usize;
            out[i] = 255;
            out[i + 1] = 255;
            out[i + 2] = 255;
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    out
}

/// The left pad's cursor: a ring with a cross rather than a dot.
///
/// Deliberately a different *shape* as well as a different colour. Two cursors that differ
/// only in hue are hard to tell apart at the edge of vision, which is exactly where the
/// non-dominant one usually is.
fn reticle_image_left(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let centre = (size as f32 - 1.0) * 0.5;
    let radius = centre;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - centre) / radius;
            let dy = (y as f32 - centre) / radius;
            let r = (dx * dx + dy * dy).sqrt();
            let ring = 1.0 - ((r - 0.78).abs() / 0.12).min(1.0);
            // A cross through the middle, stopping short of the ring so the centre stays open.
            let arm = |along: f32, across: f32| {
                if along.abs() > 0.52 || across.abs() > 0.06 {
                    0.0
                } else {
                    1.0 - (across.abs() / 0.06)
                }
            };
            let cross = arm(dx, dy).max(arm(dy, dx));
            let a = ring.max(cross).clamp(0.0, 1.0).powf(0.8);
            let i = ((y * size + x) * 4) as usize;
            out[i] = 255;
            out[i + 1] = 255;
            out[i + 2] = 255;
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    out
}

/// Build the pointer reticle: a bright dot inside a thin ring.
///
/// A ring rather than a filled disc, so that a small target underneath stays visible through
/// the middle of the cursor. Generated rather than shipped as an asset for the same reason the
/// environment is — nothing to install, nothing to license.
fn reticle_image(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let centre = (size as f32 - 1.0) * 0.5;
    let radius = centre;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - centre) / radius;
            let dy = (y as f32 - centre) / radius;
            let r = (dx * dx + dy * dy).sqrt();
            // Ring at ~0.8 of the radius, dot inside 0.22. Both edges are feathered, because
            // a hard edge on something this small crawls badly as the head moves.
            let ring = 1.0 - ((r - 0.78).abs() / 0.12).min(1.0);
            let dot = 1.0 - ((r - 0.0) / 0.24).min(1.0);
            let a = (ring.max(dot)).clamp(0.0, 1.0).powf(0.8);
            let i = ((y * size + x) * 4) as usize;
            out[i] = 255;
            out[i + 1] = 255;
            out[i + 2] = 255;
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    out
}

/// Where the eyes sit for a given head pose.
///
/// The neck model puts them ~10 cm forward and ~7.5 cm above the pivot. Anything cast *from*
/// the head -- the pointer ray above all -- has to start here, or its whole reachable area is
/// offset a couple of degrees and the top of the view becomes unreachable.
pub fn eye_centre(orientation: glam::DQuat, cfg: &spatiand_render::StereoConfig) -> glam::DVec3 {
    orientation * glam::DVec3::new(cfg.neck_forward_m, 0.0, cfg.neck_up_m)
}

/// Import every mapped window's buffer and collect what is needed to draw it.
///
/// Import has to happen outside the draw closure: it needs `&mut renderer`, while drawing
/// holds the GL context. Splitting it this way also means a client that has not committed a
/// buffer yet is simply absent from the list rather than drawn as a black rectangle.
pub fn collect_windows(
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    state: &crate::state::Spatiand,
) -> Vec<WindowQuad> {
    use smithay::backend::renderer::utils::{import_surface_tree, with_renderer_surface_state};
    use smithay::backend::renderer::{Renderer, Texture};

    let mut out = Vec::new();
    let windows: Vec<smithay::desktop::Window> = state.space.elements().cloned().collect();
    for window in windows {
        // Whichever protocol the window speaks. An X11 window has no xdg toplevel, and asking
        // only for one silently skipped every X11 window: they launched, appeared in the
        // window count, took a place in the room, and were never drawn. `WaylandFocus` is the
        // accessor that does not care -- XWayland gives each of its windows a wl_surface like
        // anything else, which is the whole reason the rest of the compositor can ignore the
        // difference.
        use smithay::wayland::seat::WaylandFocus;
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            // An X11 window whose surface has not been associated yet. It arrives a moment
            // later, and the next frame will draw it.
            continue;
        };
        if let Err(e) = import_surface_tree(renderer, &surface) {
            // Reported once per window rather than once per frame. A window that never draws
            // is one of the hardest things to diagnose from the outside -- it runs, it counts,
            // it takes a place in the room, and nothing says why it is not there -- so the
            // reason is worth saying out loud the first time.
            if note_import_failure(&surface) {
                log::warn!(
                    "a window has a surface but nothing to draw from it: {e} ({})",
                    state.title_of(&window).unwrap_or_else(|| "untitled".into())
                );
            }
            continue;
        }
        let context = renderer.context_id();
        let imported = with_renderer_surface_state(&surface, |st| {
            st.texture::<smithay::backend::renderer::gles::GlesTexture>(context)
                .map(|t| (t.tex_id(), t.width(), t.height()))
        })
        .flatten();
        let Some((texture, width, height)) = imported else {
            // Mapped but nothing committed yet. Normal for the first frames after a launch --
            // and not normal at all if it never stops, which is why it is said once.
            if note_import_failure(&surface) {
                log::info!(
                    "a window has a surface but has committed nothing to draw yet ({})",
                    state.title_of(&window).unwrap_or_else(|| "untitled".into())
                );
            }
            continue;
        };
        let Some(placement) = state.layout.get(&window) else {
            // In the space but with nowhere to be. An X11 window adopted before its placement
            // existed would sit here silently for the rest of the session.
            if note_import_failure(&surface) {
                log::warn!(
                    "a window has no place in the room, so it is not drawn ({})",
                    state.title_of(&window).unwrap_or_else(|| "untitled".into())
                );
            }
            continue;
        };

        // Menus, dropdowns and submenus, in the order the client stacked them. Each is its own
        // surface with its own buffer -- importing the toplevel's tree does not reach them,
        // which is why a window whose menu was open still drew as if it were not.
        let mut popups = Vec::new();
        for (popup, offset) in smithay::desktop::PopupManager::popups_for_surface(&surface) {
            let popup_surface = popup.wl_surface().clone();
            if import_surface_tree(renderer, &popup_surface).is_err() {
                continue;
            }
            let imported = with_renderer_surface_state(&popup_surface, |st| {
                st.texture::<smithay::backend::renderer::gles::GlesTexture>(renderer.context_id())
                    .map(|t| (t.tex_id(), t.width(), t.height()))
            })
            .flatten();
            let Some((texture, pw, ph)) = imported else {
                continue;
            };
            popups.push(PopupQuad {
                surface: popup_surface,
                texture,
                offset: (offset.x, offset.y),
                pixels: (pw, ph),
            });
        }

        // What the client actually committed, which need not be what it was offered: a
        // fullscreen-shaped application sizes itself from the output it can see, and the
        // difference between the two is exactly the kind of thing that shows up as a window
        // whose contents do not fit it.
        if note_surface_size(&surface, (width, height)) {
            log::info!(
                "window surface is {width}x{height} ({})",
                state.title_of(&window).unwrap_or_else(|| "untitled".into())
            );
        }
        out.push(WindowQuad {
            window: window.clone(),
            surface: surface.clone(),
            texture,
            pixels: (width, height),
            placement,
            focused: state.layout.is_focused(&window),
            // Both are filled in by the backend once it has a renderer to build textures with
            // and a pointer to test against.
            title: None,
            icon: None,
            close_hot: false,
            // Both filled in by the backend, which is where the audio engine lives.
            sound: None,
            mute_hot: false,
            popups,
        });
    }
    out
}

/// Say whether this surface's failure to draw is worth mentioning yet.
///
/// Once per surface, because the alternative is seventy-two identical lines a second and a log
/// nobody can read.
fn note_import_failure(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> bool {
    use smithay::reexports::wayland_server::Resource;
    thread_local! {
        static TOLD: std::cell::RefCell<std::collections::HashSet<
            smithay::reexports::wayland_server::backend::ObjectId,
        >> = std::cell::RefCell::new(std::collections::HashSet::new());
    }
    TOLD.with(|told| told.borrow_mut().insert(surface.id()))
}

/// Remember a window's surface size, and say whether it has just changed.
///
/// A memo for the log and nothing else, which is why it lives here rather than in
/// `WindowLayout`: nothing reads it, and putting it in the layout would suggest something does.
/// Without it this logs seventy-two times a second.
fn note_surface_size(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    pixels: (u32, u32),
) -> bool {
    use smithay::reexports::wayland_server::Resource;
    thread_local! {
        static SEEN: std::cell::RefCell<std::collections::HashMap<
            smithay::reexports::wayland_server::backend::ObjectId,
            (u32, u32),
        >> = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    SEEN.with(|seen| seen.borrow_mut().insert(surface.id(), pixels) != Some(pixels))
}

/// A window's quad in the world, for ray-casting against.
///
/// Not called yet: windows are tracked by `WindowLayout` but not drawn, so there is nothing
/// for the pointer to hit. It lives here — with its tests — because the aspect-ratio trap it
/// guards against is the kind that is much cheaper to get right before there is a picture to
/// misread.
#[allow(dead_code)]
pub fn window_quad(placement: &crate::window::Placement, aspect: f64) -> Quad {
    Quad {
        centre: placement.position(),
        orientation: placement.orientation(),
        width: placement.width,
        height: placement.width / aspect.max(0.01),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fitted_panel_stays_inside_both_axes_of_the_field() {
        // The bug this guards: a panel correctly sized for its width but taller than the
        // vertical field, which loses its first and last lines with no other symptom.
        for aspect in [0.4f32, 1.0, 1.73, 4.0] {
            let (w, h) = fit_to_fov(aspect, 40.0, 23.14, 1.6);
            let angular = |extent: f32| 2.0 * (extent / 2.0 / 1.6).atan().to_degrees();
            assert!(
                angular(w) <= 40.0,
                "aspect {aspect}: {} deg wide",
                angular(w)
            );
            assert!(
                angular(h) <= 23.14,
                "aspect {aspect}: {} deg tall",
                angular(h)
            );
            assert!((w / h - aspect).abs() < 1e-4, "aspect not preserved");
        }
    }

    #[test]
    fn a_fitted_panel_leaves_a_margin() {
        // Filling the nominal field exactly is already too much: the optics are worst there.
        let (_, h) = fit_to_fov(1.0, 40.0, 23.14, 1.6);
        let angular = 2.0 * (h / 2.0 / 1.6).atan().to_degrees();
        assert!(
            angular < 23.14 * 0.8,
            "no margin left: {angular} deg of 23.14"
        );
    }

    #[test]
    fn the_reticle_is_transparent_at_its_corners_and_solid_at_its_centre() {
        // A reticle that is opaque everywhere is a white square, which is exactly what a
        // botched alpha ramp produces and exactly what it looks like in the headset.
        let size = 64;
        let img = reticle_image(size);
        let alpha_at = |x: u32, y: u32| img[((y * size + x) * 4 + 3) as usize];
        assert_eq!(alpha_at(0, 0), 0, "corners must be clear");
        assert!(
            alpha_at(size / 2, size / 2) > 200,
            "centre dot must be solid"
        );
    }

    #[test]
    fn the_reticle_has_a_gap_between_its_dot_and_its_ring() {
        // The gap is the point: it lets a small target stay visible through the cursor.
        let size = 64;
        let img = reticle_image(size);
        let centre = size / 2;
        let alpha_at = |x: u32| img[((centre * size + x) * 4 + 3) as usize] as u32;
        let gap = alpha_at(centre + size / 6);
        assert!(gap < 60, "expected a gap, got alpha {gap}");
        assert!(alpha_at(centre) > 200);
    }

    #[test]
    fn a_window_quad_keeps_the_surfaces_aspect_ratio() {
        let placement = crate::window::Placement {
            width: 1.6,
            ..Default::default()
        };
        let quad = window_quad(&placement, 16.0 / 9.0);
        assert!((quad.width / quad.height - 16.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn a_degenerate_aspect_does_not_produce_an_infinite_quad() {
        // A surface that has committed no buffer yet reports zero size, and dividing by it
        // yields a quad with infinite height that swallows every ray cast at it.
        let placement = crate::window::Placement::default();
        let quad = window_quad(&placement, 0.0);
        assert!(quad.height.is_finite() && quad.height > 0.0);
    }
}
