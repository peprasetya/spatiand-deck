//! The 360° environment the spatial desktop sits inside.
//!
//! One type describes both the background and, later, a video frame: a VR180 clip and a 360°
//! photograph differ only in how much of the sphere they cover and whether they carry a second
//! eye's view. Modelling that as [`SkySource`] rather than as two code paths is what lets the
//! media player take over the environment during playback and hand it back afterwards — the
//! requirement that motivated this shape in the first place.
//!
//! The sampling maths lives here, away from GL, because it is where the sign errors are. A
//! flipped longitude puts the world mirror-imaged, which is surprisingly hard to notice on an
//! abstract background and immediately obvious once there is text in it.
//!
//! Frame convention, matching the tracker: **+X forward, +Y left, +Z up**.

use std::f32::consts::PI;

/// How much of the sphere the image covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkyProjection {
    /// Full wrap-around. `u` spans 360° of longitude.
    Equirect360,
    /// Front hemisphere only — the usual VR180 layout. Behind the viewer there is no image,
    /// and [`SkySource::sample_uv`] says so rather than wrapping the picture round twice.
    Equirect180,
}

/// How a stereo pair is packed into one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkyStereo {
    /// One image for both eyes. Correct for a photograph of a distant scene, and the only
    /// sensible choice for a synthesised background.
    Mono,
    /// Left eye in the top half, right in the bottom. What almost every stereo 360 photo and
    /// VR180 video uses, because it keeps full horizontal resolution.
    OverUnder,
    /// Left eye in the left half. Rarer for source material, but it is the layout the glasses
    /// themselves consume, so it turns up.
    SideBySide,
}

/// Which eye is being drawn. Duplicated from [`crate::EyeSide`] only in spirit — this module
/// stays free of the camera model so it can be reasoned about on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkyEye {
    Left,
    Right,
}

/// A complete description of what to wrap around the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkySource {
    pub projection: SkyProjection,
    pub stereo: SkyStereo,
    /// Rotation applied about the vertical axis, radians, so the interesting part of a
    /// panorama can be brought round to face the wearer's forward direction.
    pub yaw_offset_millideg: i32,
}

impl Default for SkySource {
    fn default() -> Self {
        Self {
            projection: SkyProjection::Equirect360,
            stereo: SkyStereo::Mono,
            yaw_offset_millideg: 0,
        }
    }
}

/// The sub-rectangle of the texture belonging to one eye, as `(u0, v0, u1, v1)`.
pub type UvRect = (f32, f32, f32, f32);

impl SkySource {
    pub fn mono_360() -> Self {
        Self::default()
    }

    pub fn stereo_360() -> Self {
        Self {
            stereo: SkyStereo::OverUnder,
            ..Self::default()
        }
    }

    pub fn yaw_offset_radians(&self) -> f32 {
        (self.yaw_offset_millideg as f32 / 1000.0).to_radians()
    }

    /// Which part of the texture this eye reads.
    ///
    /// Handed to the shader as a scale and bias rather than being baked into the UV maths, so
    /// switching a running session between mono and stereo is a uniform update and not a
    /// different shader.
    pub fn eye_rect(&self, eye: SkyEye) -> UvRect {
        match (self.stereo, eye) {
            (SkyStereo::Mono, _) => (0.0, 0.0, 1.0, 1.0),
            (SkyStereo::OverUnder, SkyEye::Left) => (0.0, 0.0, 1.0, 0.5),
            (SkyStereo::OverUnder, SkyEye::Right) => (0.0, 0.5, 1.0, 1.0),
            (SkyStereo::SideBySide, SkyEye::Left) => (0.0, 0.0, 0.5, 1.0),
            (SkyStereo::SideBySide, SkyEye::Right) => (0.5, 0.0, 1.0, 1.0),
        }
    }

    /// Texture coordinates for a viewing direction, or `None` where the source has no image.
    ///
    /// `direction` need not be normalised. The result is the 0..1 coordinate **within one
    /// eye's view**; which half of the texture that lands in is [`SkySource::eye_rect`]'s job.
    /// The split is deliberately not folded in here — the shader applies the rect as a scale
    /// and bias, so switching a running session between mono and stereo is a uniform update
    /// rather than a different code path.
    pub fn sample_uv(&self, direction: [f32; 3]) -> Option<(f32, f32)> {
        let [x, y, z] = direction;
        let len = (x * x + y * y + z * z).sqrt();
        if len < 1e-9 {
            return None;
        }
        let (x, y, z) = (x / len, y / len, z / len);

        // +Y is left, so -y is to the viewer's right. Measuring azimuth from +X towards -Y
        // makes it increase rightwards, which is the direction a panorama unrolls.
        let azimuth = wrap_pi((-y).atan2(x) - self.yaw_offset_radians());
        // v runs top to bottom: 0 at the zenith, 1 at nadir.
        let v = 0.5 - z.clamp(-1.0, 1.0).asin() / PI;

        let u = match self.projection {
            SkyProjection::Equirect360 => 0.5 + azimuth / (2.0 * PI),
            SkyProjection::Equirect180 => {
                // Outside the front hemisphere there is genuinely nothing to show. Returning a
                // clamped edge pixel instead would smear the rim of the image all the way
                // round behind you, which looks like a rendering fault.
                if azimuth.abs() > PI / 2.0 {
                    return None;
                }
                0.5 + azimuth / PI
            }
        };
        Some((u, v.clamp(0.0, 1.0)))
    }

    /// Inverse of [`SkySource::sample_uv`], for tests and for placing things against the sky.
    pub fn direction_for_uv(&self, u: f32, v: f32) -> [f32; 3] {
        let azimuth = match self.projection {
            SkyProjection::Equirect360 => (u - 0.5) * 2.0 * PI,
            SkyProjection::Equirect180 => (u - 0.5) * PI,
        } + self.yaw_offset_radians();
        let elevation = (0.5 - v) * PI;
        let (ce, se) = (elevation.cos(), elevation.sin());
        [ce * azimuth.cos(), -ce * azimuth.sin(), se]
    }
}

/// An equirectangular RGBA image.
#[derive(Debug, Clone, PartialEq)]
pub struct Sky {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub source: SkySource,
}

impl Sky {
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>, source: SkySource) -> Option<Self> {
        if width == 0 || height == 0 || rgba.len() != (width as usize * height as usize * 4) {
            return None;
        }
        Some(Self {
            width,
            height,
            rgba,
            source,
        })
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y.min(self.height - 1) * self.width + x.min(self.width - 1)) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }

    /// Nothing at all.
    ///
    /// A deliberate choice rather than a failure state: with a window in front of you and
    /// black behind it there is nothing competing for the eye, which is what you want when the
    /// point of the session is the work and not the room. It is also the only environment that
    /// costs no fill rate worth measuring.
    ///
    /// The consequence is honest and worth knowing before choosing it: the environment is also
    /// what the glass bubbles refract, so with nothing to refract they read as flat dark discs.
    /// Emptiness is the thing being asked for, and that is what emptiness looks like.
    ///
    /// Four pixels is enough — every direction samples the same colour, and a full-size black
    /// image would be 8 MB of zeroes.
    pub fn blank() -> Self {
        let mut rgba = vec![0u8; 2 * 2 * 4];
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Self {
            width: 2,
            height: 2,
            rgba,
            source: SkySource::mono_360(),
        }
    }

    /// The default environment, generated rather than downloaded.
    ///
    /// Spatiand has to look like something the first time it runs, on a machine with no assets
    /// fetched and possibly no network. Synthesising the background solves that, and sidesteps
    /// the licensing question entirely — there is nothing to redistribute.
    ///
    /// It is not decoration only. The same texture is the environment map the glass bubbles
    /// refract, so it deliberately carries a bright key light and a clear horizon: a uniform
    /// gradient would leave the bubbles looking like flat grey discs.
    pub fn studio(width: u32, height: u32) -> Self {
        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        // Key light: high and to the left of forward, so bubbles catch a highlight slightly
        // off-centre. Dead ahead would put the highlight behind whatever you are looking at.
        let key_dir = normalize([0.55, 0.6, 0.58]);

        for py in 0..height {
            // v = 0 at the zenith.
            let v = (py as f32 + 0.5) / height as f32;
            let elevation = (0.5 - v) * PI;
            let (ce, se) = (elevation.cos(), elevation.sin());

            for px in 0..width {
                let u = (px as f32 + 0.5) / width as f32;
                let azimuth = (u - 0.5) * 2.0 * PI;
                let dir = [ce * azimuth.cos(), -ce * azimuth.sin(), se];

                // Vertical grade. Deliberately dark and high-contrast: a pale, even sky
                // gives the eye nothing to converge on and the whole world reads as flat fog,
                // which is exactly how the first version looked. Depth here comes from the
                // floor's perspective and from what is drawn against it, so the background's
                // job is to stay out of the way and give those something to sit against.
                let horizon = (1.0 - (se.abs() * 4.0).min(1.0)).powf(3.0);
                let sky = (se.max(0.0)).powf(0.7);
                let ground = (-se).max(0.0).powf(0.5);

                let mut r = 0.008 + sky * 0.020 + horizon * 0.085 - ground * 0.004;
                let mut g = 0.011 + sky * 0.034 + horizon * 0.070 - ground * 0.006;
                let mut b = 0.024 + sky * 0.085 + horizon * 0.062 - ground * 0.012;

                // Key light: a soft, wide glow rather than a disc, so it reads as studio
                // lighting instead of as a sun someone forgot to draw.
                let cos_key = dot(dir, key_dir).max(0.0);
                let glow = cos_key.powf(24.0) * 0.42 + cos_key.powf(4.0) * 0.055;
                r += glow * 1.00;
                g += glow * 0.94;
                b += glow * 0.84;

                // The floor grid is what actually carries the depth cue. Concentric rings
                // converging towards the horizon give the eye a perspective reference that no
                // amount of gradient can, and it is the one part of the background that says
                // "this is a space" rather than "this is a backdrop".
                if se < -0.015 {
                    let fade = ((-se - 0.015) * 2.6).min(1.0).powf(0.55);
                    let radius = ce / (-se).max(1e-3);
                    if radius < 60.0 {
                        let distance_fade = (1.0 - radius / 60.0).max(0.0).powf(1.4);
                        let ring = grid_line(radius * 0.5, 1.0);
                        let spoke = grid_line(azimuth * 16.0 / PI, 1.0);
                        let line = ring.max(spoke * 0.8) * fade * distance_fade;
                        r += line * 0.16;
                        g += line * 0.26;
                        b += line * 0.38;
                    }
                }

                // A thin bright horizon. Gives a level reference, which matters more for
                // comfort than for looks -- without one the eye has nothing to tell it where
                // upright is when the world is otherwise featureless.
                let horizon_line = (1.0 - (se.abs() * 90.0).min(1.0)).powf(2.0);
                r += horizon_line * 0.10;
                g += horizon_line * 0.16;
                b += horizon_line * 0.24;

                let i = ((py * width + px) * 4) as usize;
                rgba[i] = to_srgb_byte(r);
                rgba[i + 1] = to_srgb_byte(g);
                rgba[i + 2] = to_srgb_byte(b);
                rgba[i + 3] = 255;
            }
        }

        Self {
            width,
            height,
            rgba,
            source: SkySource::mono_360(),
        }
    }
}

/// Triangular ramp peaking at integer positions — a cheap anti-aliased grid line.
fn grid_line(coordinate: f32, width: f32) -> f32 {
    let fract = coordinate - coordinate.floor();
    let distance = (fract - 0.5).abs() * 2.0;
    ((distance - (1.0 - width * 0.06)) / (width * 0.06)).clamp(0.0, 1.0)
}

fn to_srgb_byte(linear: f32) -> u8 {
    // The texture is sampled as plain RGBA, so bake the transfer curve in here rather than
    // relying on an sRGB texture format the GLES path may or may not give us.
    let c = linear.clamp(0.0, 1.0);
    let s = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt().max(1e-9);
    [v[0] / len, v[1] / len, v[2] / len]
}

fn wrap_pi(angle: f32) -> f32 {
    let two_pi = 2.0 * PI;
    let mut a = (angle + PI) % two_pi;
    if a < 0.0 {
        a += two_pi;
    }
    a - PI
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn forward_lands_in_the_middle_of_the_image() {
        let s = SkySource::mono_360();
        let (u, v) = s.sample_uv([1.0, 0.0, 0.0]).expect("forward is visible");
        assert!(close(u, 0.5, 1e-5), "u = {u}");
        assert!(close(v, 0.5, 1e-5), "v = {v}");
    }

    #[test]
    fn up_is_the_top_of_the_image() {
        // v = 0 at the zenith. Getting this inverted turns the world upside down, which reads
        // as a tracker fault rather than a texture one.
        let s = SkySource::mono_360();
        let (_, v) = s.sample_uv([0.0, 0.0, 1.0]).unwrap();
        assert!(close(v, 0.0, 1e-5), "zenith should be v=0, got {v}");
        let (_, v) = s.sample_uv([0.0, 0.0, -1.0]).unwrap();
        assert!(close(v, 1.0, 1e-5), "nadir should be v=1, got {v}");
    }

    #[test]
    fn longitude_increases_to_the_right() {
        // The mirror-image test. +Y is left, so looking left must give a SMALLER u than
        // looking forward — a panorama unrolls left to right.
        let s = SkySource::mono_360();
        let (left, _) = s.sample_uv([0.0, 1.0, 0.0]).unwrap();
        let (fwd, _) = s.sample_uv([1.0, 0.0, 0.0]).unwrap();
        let (right, _) = s.sample_uv([0.0, -1.0, 0.0]).unwrap();
        assert!(close(left, 0.25, 1e-5), "left = {left}");
        assert!(close(fwd, 0.5, 1e-5));
        assert!(close(right, 0.75, 1e-5), "right = {right}");
    }

    #[test]
    fn uv_and_direction_round_trip() {
        let s = SkySource::mono_360();
        for &(u, v) in &[(0.1, 0.2), (0.5, 0.5), (0.9, 0.8), (0.33, 0.05)] {
            let dir = s.direction_for_uv(u, v);
            let (u2, v2) = s.sample_uv(dir).unwrap();
            assert!(close(u, u2, 1e-4), "u {u} -> {u2}");
            assert!(close(v, v2, 1e-4), "v {v} -> {v2}");
        }
    }

    #[test]
    fn vr180_has_no_image_behind_you() {
        let s = SkySource {
            projection: SkyProjection::Equirect180,
            ..Default::default()
        };
        assert!(s.sample_uv([1.0, 0.0, 0.0]).is_some(), "forward");
        assert!(s.sample_uv([-1.0, 0.0, 0.0]).is_none(), "behind");
        // The seam: exactly 90° off-axis is the edge of the image.
        let (u, _) = s.sample_uv([0.0, -1.0, 0.001]).expect("right edge");
        assert!(close(u, 1.0, 1e-3), "u = {u}");
    }

    #[test]
    fn stereo_layouts_split_the_texture_without_overlapping() {
        for stereo in [SkyStereo::OverUnder, SkyStereo::SideBySide] {
            let s = SkySource {
                stereo,
                ..Default::default()
            };
            let (lu0, lv0, lu1, lv1) = s.eye_rect(SkyEye::Left);
            let (ru0, rv0, ru1, rv1) = s.eye_rect(SkyEye::Right);
            assert_ne!(
                (lu0, lv0, lu1, lv1),
                (ru0, rv0, ru1, rv1),
                "{stereo:?} gave both eyes the same half"
            );
            let area = |(u0, v0, u1, v1): UvRect| (u1 - u0) * (v1 - v0);
            assert!(close(area((lu0, lv0, lu1, lv1)), 0.5, 1e-6));
            assert!(close(area((ru0, rv0, ru1, rv1)), 0.5, 1e-6));
        }
        // Mono must hand both eyes the whole thing, or a plain background renders at half size.
        let m = SkySource::mono_360();
        assert_eq!(m.eye_rect(SkyEye::Left), m.eye_rect(SkyEye::Right));
        assert_eq!(m.eye_rect(SkyEye::Left), (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn over_under_gives_the_left_eye_the_top_half() {
        // The convention almost every stereo 360 photo uses. Swapping these makes the world
        // hurt to look at without looking obviously wrong in a screenshot.
        let s = SkySource::stereo_360();
        let (_, v0, _, v1) = s.eye_rect(SkyEye::Left);
        assert_eq!((v0, v1), (0.0, 0.5));
    }

    #[test]
    fn yaw_offset_turns_the_panorama_not_the_viewer() {
        let s = SkySource {
            yaw_offset_millideg: 90_000,
            ..Default::default()
        };
        // With the panorama turned 90° the pixel that was at u=0.75 is now straight ahead.
        let (u, _) = s.sample_uv([1.0, 0.0, 0.0]).unwrap();
        assert!(close(u, 0.25, 1e-4), "u = {u}");
    }

    #[test]
    fn the_generated_sky_is_a_well_formed_image() {
        let sky = Sky::studio(64, 32);
        assert_eq!(sky.rgba.len(), 64 * 32 * 4);
        assert!(
            sky.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "must be opaque"
        );
    }

    #[test]
    fn the_generated_sky_is_brighter_above_the_horizon_than_below() {
        // Guards the thing that makes it read as a room rather than as noise, and would catch
        // an elevation sign flip that the round-trip test cannot see.
        let sky = Sky::studio(128, 64);
        let luma = |y: u32| {
            let mut total = 0u32;
            for x in 0..sky.width {
                let p = sky.pixel(x, y);
                total += p[0] as u32 + p[1] as u32 + p[2] as u32;
            }
            total / sky.width
        };
        assert!(
            luma(8) > luma(56),
            "zenith {} vs nadir {}",
            luma(8),
            luma(56)
        );
    }

    #[test]
    fn the_blank_environment_is_black_and_opaque_everywhere() {
        // Opaque matters: the sky is drawn first and everything else over it, so a
        // transparent "black" would show whatever the framebuffer happened to hold.
        let sky = Sky::blank();
        assert_eq!(sky.rgba.len(), (sky.width * sky.height * 4) as usize);
        for pixel in sky.rgba.chunks_exact(4) {
            assert_eq!(pixel, [0, 0, 0, 255]);
        }
        // And it must still be a well-formed 360 source, or the shader samples a rect that
        // does not exist.
        assert_eq!(sky.source, SkySource::mono_360());
        assert!(sky.source.sample_uv([1.0, 0.0, 0.0]).is_some());
    }

    #[test]
    fn rejecting_a_malformed_image_beats_indexing_past_the_end() {
        assert!(Sky::from_rgba(4, 4, vec![0; 10], SkySource::default()).is_none());
        assert!(Sky::from_rgba(0, 4, vec![], SkySource::default()).is_none());
        assert!(Sky::from_rgba(2, 2, vec![0; 16], SkySource::default()).is_some());
    }
}
