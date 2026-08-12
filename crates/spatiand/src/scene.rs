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

use crate::gl::{upload_raw, upload_rgba, BubbleParams, BubblePipeline, QuadPipeline, SkyPipeline};

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
/// How far below level the keyboard hangs, radians, and how far away.
///
/// Down and close, the way a real keyboard sits: you glance down at it and back up at what you
/// are typing into. Putting it at window distance and window height means it covers whatever
/// you are working on, which is the one thing it must not do.
const KEYBOARD_PITCH: f32 = 0.52;
const KEYBOARD_DISTANCE: f32 = 0.85;
/// Half the width of a full launcher row, in degrees, for placing the page dots just outside
/// it. Mirrors `spatiand_shell::launcher::COLUMN_SPACING_DEG` and is kept here so the scene
/// does not have to reach into the shell's layout arithmetic.
const COLUMN_SPACING_DEG_LOCAL: f32 = 9.0;

/// One window, ready to draw: its imported texture and where it sits.
pub struct WindowQuad {
    pub texture: u32,
    /// Surface size in pixels, for the aspect ratio.
    pub pixels: (u32, u32),
    pub placement: crate::window::Placement,
    pub focused: bool,
    /// Rasterised title, if one has been built for this window.
    pub title: Option<TitleTexture>,
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

/// Textures and pipelines that live for the session.
pub struct Scene {
    quads: QuadPipeline,
    sky_pipeline: SkyPipeline,
    bubbles: BubblePipeline,

    sky: u32,
    sky_source: SkySource,
    white: u32,
    reticle: u32,
    /// A second cursor shape for the left pad. Different in outline as well as colour, so the
    /// two are distinguishable to someone who cannot rely on hue.
    reticle_left: u32,

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

    /// The keyboard face, rebuilt when shift changes.
    keys: Option<Texture>,
    keys_shifted: bool,
    keys_built: bool,

    /// The status bar, rebuilt when its text changes.
    status: Option<Texture>,
    status_text: String,

    /// The menu text, rebuilt when it changes.
    menu: Option<Texture>,
    menu_text: String,

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

        let (sky, white, reticle, reticle_left) = renderer
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
                )
            })
            .map_err(|e| format!("no GL context: {e}"))?;

        Ok(Self {
            quads,
            sky_pipeline,
            bubbles,
            sky,
            sky_source: sky_image.source,
            white,
            reticle,
            reticle_left,
            app_labels: Vec::new(),
            app_glyphs: Vec::new(),
            labels_built_for: usize::MAX,
            titles: std::collections::HashMap::new(),
            keys: None,
            keys_shifted: false,
            keys_built: false,
            status: None,
            status_text: String::new(),
            menu: None,
            menu_text: String::new(),
            anchor_yaw: 0.0,
            anchored_for: None,
            anchored_at: std::time::Instant::now(),
            closing: None,
        })
    }

    pub fn quads(&self) -> &QuadPipeline {
        &self.quads
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
                        let initial = name.chars().next().unwrap_or('?').to_uppercase().to_string();
                        text.render(&initial, px_per_degree * 4.0, 256, [255, 255, 255, 235])
                    }
                }
            })
            .collect();
        log::info!("launcher icons: {resolved} of {} from the icon theme", entries.len());

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
        self.titles.insert(title.to_string(), Texture { id, aspect });
        Some(TitleTexture { id, aspect })
    }

    /// Rebuild the keyboard face if the shift state changed.
    ///
    /// One texture for the whole keyboard rather than a quad per key. Eighty quads with eighty
    /// labels would be eighty uploads every time shift is pressed, and at the size a key is
    /// actually drawn the difference is invisible -- the hit-testing is arithmetic on the
    /// pointer's UV either way.
    pub fn sync_keyboard(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        keyboard: &spatiand_shell::Keyboard,
        px_per_degree: f32,
    ) -> Result<(), String> {
        if self.keys_built && self.keys_shifted == keyboard.shift {
            return Ok(());
        }
        self.keys_shifted = keyboard.shift;
        self.keys_built = true;

        // Rows padded so the columns line up under a monospaced rendering of the labels.
        let mut face = String::new();
        for row in spatiand_shell::keyboard::ROWS {
            for k in row.iter() {
                let label = keyboard.label(k);
                face.push_str(&format!(" {label} "));
            }
            face.push('\n');
        }
        let image = text.render(&face, px_per_degree * 0.9, 2048, [225, 233, 250, 255]);
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

    /// Where the keyboard sits in the world.
    ///
    /// Below the eye line and closer than a window, the way a real keyboard sits: you glance
    /// down at it and back up at what you are typing into, rather than having it cover the
    /// thing you are working on.
    pub fn keyboard_placement(&self, orientation: glam::DQuat) -> (Vec3, Quat, f32, f32) {
        let head = Quat::from_xyzw(
            orientation.x as f32,
            orientation.y as f32,
            orientation.z as f32,
            orientation.w as f32,
        );
        // Yaw follows the head; pitch is fixed downward so looking up does not drag it away.
        let (yaw, _, _) = head.to_euler(glam::EulerRot::ZYX);
        let facing = Quat::from_rotation_z(yaw) * Quat::from_rotation_y(-KEYBOARD_PITCH);
        let cfg = spatiand_render::StereoConfig::default();
        let centre = Quat::from_rotation_z(yaw)
            * Vec3::new(cfg.neck_forward_m as f32, 0.0, cfg.neck_up_m as f32)
            + facing * Vec3::X * KEYBOARD_DISTANCE;
        let width = 0.62f32;
        let height = width * 0.38;
        (centre, facing, width, height)
    }

    /// Draw the keyboard.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_keyboard(&self, gl: &ffi::Gles2, eye: &Eye, orientation: glam::DQuat) {
        let Some(face) = self.keys else {
            return;
        };
        let (centre, facing, width, height) = self.keyboard_placement(orientation);
        let plate = self.panel_model(centre, facing, width * 1.05, height * 1.15);
        self.quads.draw(
            gl,
            self.white,
            &(eye.view_projection() * plate),
            [0.03, 0.04, 0.08, 0.88],
            (0.0, 1.0),
        );
        let model = self.panel_model(centre, facing, width, height);
        self.quads.draw(
            gl,
            face.id,
            &(eye.view_projection() * model),
            [1.0, 1.0, 1.0, 1.0],
            (0.0, 1.0),
        );
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
        let left_edge_deg = 17.0f32;
        let yaw = (left_edge_deg - half_width_deg).to_radians();
        let pitch = 8.0f32.to_radians();
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

    /// Rebuild the menu panel if its text changed.
    pub fn sync_menu(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        text: &mut TextRenderer,
        wanted: &str,
        px_per_degree: f32,
        max_width: u32,
    ) -> Result<(), String> {
        if wanted == self.menu_text && self.menu.is_some() {
            return Ok(());
        }
        self.menu_text = wanted.to_string();
        if wanted.is_empty() {
            return Ok(());
        }
        let image = text.render(wanted, px_per_degree * 1.05, max_width, [236, 241, 255, 255]);
        let old = self.menu.take();
        self.menu = Some(
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
            let orientation = Quat::from_rotation_z(window.placement.yaw as f32);
            let model = self.panel_model(centre, orientation, width, height);
            let mvp = eye.view_projection() * model;

            // The frame is drawn first and slightly larger, so it reads as a border rather
            // than as something overlapping the content.
            let border = 0.012f32;
            let frame = self.panel_model(
                centre,
                orientation,
                width + border * 2.0,
                height + border * 2.0,
            );
            let frame_tint = if window.focused {
                [0.55, 0.72, 1.0, 0.85]
            } else {
                [0.30, 0.34, 0.45, 0.55]
            };
            self.quads
                .draw(gl, self.white, &(eye.view_projection() * frame), frame_tint, (0.0, 1.0));

            // The title bar sits above the content and is what you grab to move the window.
            // Drawn as part of the same column so it cannot drift away from its window.
            let bar_height = height * bar_fraction / (1.0 - bar_fraction);
            let bar_centre = centre + (orientation * Vec3::Z) * (height + bar_height) * 0.5;
            let bar = self.panel_model(bar_centre, orientation, width, bar_height);
            let bar_tint = if window.focused {
                [0.16, 0.22, 0.34, 0.95]
            } else {
                [0.10, 0.12, 0.17, 0.85]
            };
            self.quads
                .draw(gl, self.white, &(eye.view_projection() * bar), bar_tint, (0.0, 1.0));
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
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);

            // The surface itself. Fully opaque: a client's own transparency would otherwise
            // let the environment through, and a half-transparent terminal floating in a room
            // is unreadable.
            self.quads.draw(gl, window.texture, &mvp, [1.0, 1.0, 1.0, 1.0], (0.0, 1.0));
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
            Mode::Hud => self.draw_hud(gl, eye, fov),
            Mode::Launcher => self.draw_launcher(gl, eye, shell, fov),
        }
    }

    unsafe fn draw_hud(&self, gl: &ffi::Gles2, eye: &Eye, fov: (f64, f64)) {
        let Some(panel) = self.menu else {
            return;
        };
        let distance = 1.6f32;
        // Fitted to the field of view, not to a constant. A fixed 0.9 m tall panel at 1.6 m
        // subtends 31 degrees against a 23 degree vertical field -- so the first and last rows
        // of the settings list were always off screen, whatever the list contained.
        let (width, height) = fit_to_fov(panel.aspect, fov.0, fov.1, distance);
        let backdrop = self.panel_model(
            self.menu_centre(distance),
            self.anchor_quat(),
            width * 1.14,
            height * 1.18,
        );
        self.quads.draw(
            gl,
            self.white,
            &(eye.view_projection() * backdrop),
            [0.02, 0.03, 0.06, 0.72],
            (0.0, 1.0),
        );
        let model = self.panel_model(self.menu_centre(distance), self.anchor_quat(), width, height);
        self.quads.draw(
            gl,
            panel.id,
            &(eye.view_projection() * model),
            [1.0, 1.0, 1.0, 1.0],
            (0.0, 1.0),
        );
    }

    unsafe fn draw_launcher(&self, gl: &ffi::Gles2, eye: &Eye, shell: &Shell, fov: (f64, f64)) {
        let launcher = shell.launcher();
        if launcher.is_empty() {
            // Say so, rather than showing an empty sky that looks like a failure to open.
            self.draw_hud(gl, eye, fov);
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
            let alpha = if placement.scale > 1.0 { 1.0 } else { 0.55 } * self.appear_progress(index);
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
        right_hand: bool,
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
        let model = Mat4::from_cols(
            (right * size).extend(0.0),
            (up * size).extend(0.0),
            to_eye.extend(0.0),
            point.extend(1.0),
        );

        // Two pads means two cursors, and they have to be told apart at a glance -- otherwise
        // a two-handed gesture is impossible to aim. Warm for the right hand, cool for the
        // left, which is easier to read peripherally than two shapes would be.
        let hue = if right_hand {
            [1.0, 0.78, 0.42]
        } else {
            [0.52, 0.82, 1.0]
        };
        let alpha = if hit.is_some() { 0.95 } else { 0.5 };
        let tint = [hue[0], hue[1], hue[2], alpha];
        self.quads.draw(
            gl,
            if right_hand { self.reticle } else { self.reticle_left },
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
        right_hand: bool,
    ) {
        let distance = hit.map(|h| h.distance as f32).unwrap_or(2.5);
        let point = (ray.origin + ray.direction * distance as f64).as_vec3();
        let hue = if right_hand {
            [1.0, 0.78, 0.42]
        } else {
            [0.52, 0.82, 1.0]
        };
        let alpha = if hit.is_some() { 0.55 } else { 0.28 };
        self.draw_beam(
            gl,
            eye,
            Self::hand_anchor(orientation, right_hand),
            point,
            [hue[0], hue[1], hue[2], alpha],
        );
        self.draw_pointer(gl, eye, ray, hit, right_hand);
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
        self.quads
            .draw(gl, self.white, &(eye.view_projection() * model), colour, (0.0, 1.0));
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
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let surface = toplevel.wl_surface().clone();
        if let Err(e) = import_surface_tree(renderer, &surface) {
            log::debug!("could not import a surface: {e}");
            continue;
        }
        let context = renderer.context_id();
        let imported = with_renderer_surface_state(&surface, |st| {
            st.texture::<smithay::backend::renderer::gles::GlesTexture>(context)
                .map(|t| (t.tex_id(), t.width(), t.height()))
        })
        .flatten();
        let Some((texture, width, height)) = imported else {
            // Mapped but nothing committed yet. Normal for the first frames after a launch.
            continue;
        };
        let Some(placement) = state.layout.get(&window) else {
            continue;
        };
        out.push(WindowQuad {
            texture,
            pixels: (width, height),
            placement,
            focused: state.layout.is_focused(&window),
            title: None,
        });
    }
    out
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
            assert!(angular(w) <= 40.0, "aspect {aspect}: {} deg wide", angular(w));
            assert!(angular(h) <= 23.14, "aspect {aspect}: {} deg tall", angular(h));
            assert!((w / h - aspect).abs() < 1e-4, "aspect not preserved");
        }
    }

    #[test]
    fn a_fitted_panel_leaves_a_margin() {
        // Filling the nominal field exactly is already too much: the optics are worst there.
        let (_, h) = fit_to_fov(1.0, 40.0, 23.14, 1.6);
        let angular = 2.0 * (h / 2.0 / 1.6).atan().to_degrees();
        assert!(angular < 23.14 * 0.8, "no margin left: {angular} deg of 23.14");
    }

    #[test]
    fn the_reticle_is_transparent_at_its_corners_and_solid_at_its_centre() {
        // A reticle that is opaque everywhere is a white square, which is exactly what a
        // botched alpha ramp produces and exactly what it looks like in the headset.
        let size = 64;
        let img = reticle_image(size);
        let alpha_at = |x: u32, y: u32| img[((y * size + x) * 4 + 3) as usize];
        assert_eq!(alpha_at(0, 0), 0, "corners must be clear");
        assert!(alpha_at(size / 2, size / 2) > 200, "centre dot must be solid");
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
