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
//! ## Why the pointer is a reticle and not a beam
//!
//! A laser drawn from between the eyes is foreshortened to a dot: it starts at the viewpoint,
//! so there is nothing of it to see. With no tracked hand to emit from, the honest rendering
//! of a head-anchored ray is its *intersection* — a cursor. The aiming metaphor is still the
//! laser one; only the visible part differs.

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

    /// One per app in the launcher, in the same order.
    app_labels: Vec<Texture>,
    /// The label the bubbles show inside the glass — currently the app's initial.
    app_glyphs: Vec<Texture>,
    /// Rebuilt only when the app list changes; rasterising twenty labels a frame would cost
    /// more than everything else here put together.
    labels_built_for: usize,

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
}

impl Scene {
    pub fn new(
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        sky_image: &spatiand_render::Sky,
    ) -> Result<Self, String> {
        let quads = QuadPipeline::new(renderer)?;
        let sky_pipeline = SkyPipeline::new(renderer, &quads)?;
        let bubbles = BubblePipeline::new(renderer, &quads)?;

        let (sky, white, reticle) = renderer
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
            app_labels: Vec::new(),
            app_glyphs: Vec::new(),
            labels_built_for: usize::MAX,
            menu: None,
            menu_text: String::new(),
            anchor_yaw: 0.0,
            anchored_for: None,
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
        }
    }

    pub fn forget_anchor(&mut self) {
        self.anchored_for = None;
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
        let apps = shell.launcher().apps();
        // Identity by length and first name is enough: the list only changes on a rescan.
        let fingerprint = apps.len();
        if fingerprint == self.labels_built_for {
            return Ok(());
        }

        let labels: Vec<TextImage> = apps
            .iter()
            .map(|a| text.render(&a.name, px_per_degree * 0.75, 512, [232, 238, 255, 255]))
            .collect();
        // The icon inside the glass: the system's own, so an app looks the same here as it
        // does on the desktop. Falling back to the initial rather than to a blank or a
        // question mark — plenty of entries name an icon that is not installed, and a letter
        // is at least identifiable.
        let mut resolved = 0usize;
        let glyphs: Vec<TextImage> = apps
            .iter()
            .map(|a| {
                let from_theme = a
                    .icon
                    .as_deref()
                    .and_then(|name| spatiand_platform::resolve_icon(name))
                    .and_then(|path| crate::icon::load(&path, ICON_TEXTURE_PX));
                match from_theme {
                    Some(image) => {
                        resolved += 1;
                        image
                    }
                    None => {
                        let initial =
                            a.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                        text.render(&initial, px_per_degree * 4.0, 256, [255, 255, 255, 235])
                    }
                }
            })
            .collect();
        log::info!("launcher icons: {resolved} of {} from the icon theme", apps.len());

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

    /// Draw whichever menu is open.
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn draw_menu(&self, gl: &ffi::Gles2, eye: &Eye, shell: &Shell, fov: (f64, f64)) {
        match shell.mode() {
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
            let size = BUBBLE_DIAMETER_M * placement.scale;
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
                },
            );
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
            let alpha = if placement.scale > 1.0 { 1.0 } else { 0.55 };
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
    pub unsafe fn draw_pointer(&self, gl: &ffi::Gles2, eye: &Eye, ray: &Ray, hit: Option<Hit>) {
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

        let tint = if hit.is_some() {
            [0.65, 0.85, 1.0, 0.95]
        } else {
            [0.85, 0.88, 0.95, 0.55]
        };
        self.quads.draw(
            gl,
            self.reticle,
            &(eye.view_projection() * model),
            tint,
            (0.0, 1.0),
        );
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
