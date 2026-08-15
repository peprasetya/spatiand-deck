//! The GLES 3.2 drawing layer.
//!
//! Three pipelines, all sharing one unit-quad vertex buffer:
//!
//! * [`QuadPipeline`] — a textured quad with a transform. Window surfaces, text panels, the
//!   laser beam and its reticle are all this.
//! * [`SkyPipeline`] — the 360° environment, drawn as a full-screen pass that turns each
//!   fragment's clip position back into a viewing direction. No sphere geometry, no cubemap
//!   conversion, and it costs one texture fetch per pixel.
//! * [`BubblePipeline`] — the glass icon bubbles. The skybox doubles as the environment map,
//!   so refraction and reflection are single lookups rather than a second render pass.
//!
//! This lives in the `spatiand` crate rather than `spatiand-render` because it needs
//! Smithay's GL context. `spatiand-render` stays GPU-free and therefore testable anywhere.

use smithay::backend::renderer::gles::{ffi, GlesRenderer};

use glam::{Mat3, Mat4, Vec3};
use spatiand_render::text::TextImage;

/// Shared preamble. GLES needs an explicit precision declaration and there is no reason for
/// the three shaders to disagree about it.
const PREAMBLE: &str = "#version 320 es\nprecision highp float;\n";

/// Vertex shader: an untransformed unit quad, positioned entirely by the MVP.
const QUAD_VERT: &str = r#"
layout(location = 0) in vec2 a_pos;
uniform mat4 u_mvp;
out vec2 v_uv;
void main() {
    // The quad spans -0.5..0.5 so that scaling by (width, height) gives a centred quad of
    // exactly that size — placement maths stays in metres with the origin in the middle.
    v_uv = vec2(a_pos.x + 0.5, 0.5 - a_pos.y);
    gl_Position = u_mvp * vec4(a_pos, 0.0, 1.0);
}
"#;

const QUAD_FRAG: &str = r#"
in vec2 v_uv;
uniform sampler2D u_tex;
uniform vec4 u_tint;
// Left/right halves of a side-by-side source, so one texture can feed both eyes with
// different content. (0,1) means "use the whole thing", which is the mono case.
uniform vec2 u_uv_range;
out vec4 f_color;
void main() {
    vec2 uv = vec2(u_uv_range.x + v_uv.x * (u_uv_range.y - u_uv_range.x), v_uv.y);
    f_color = texture(u_tex, uv) * u_tint;
}
"#;

/// Sky vertex shader: the same quad, but pushed straight to the far plane in clip space so it
/// covers the viewport whatever the camera is doing.
const SKY_VERT: &str = r#"
layout(location = 0) in vec2 a_pos;
out vec2 v_ndc;
void main() {
    v_ndc = a_pos * 2.0;
    gl_Position = vec4(v_ndc, 1.0, 1.0);
}
"#;

/// Equirectangular sampling, matching `spatiand_render::sky` exactly.
///
/// The conventions are duplicated here rather than shared, because one side has to run on the
/// GPU. `sky.rs` carries the tests; any change to the maths has to be made in both, and the
/// tests there are what say which one is right.
const SKY_FRAG: &str = r#"
in vec2 v_ndc;
uniform sampler2D u_sky;
// Inverse of projection * (view with translation removed): clip space back to a world ray.
uniform mat4 u_inv_vp;
// (u0, v0, u1, v1) — this eye's half of a stereo source, or the whole texture for mono.
uniform vec4 u_rect;
uniform float u_yaw;
// 0 = full 360, 1 = front hemisphere only.
uniform int u_hemisphere;
uniform float u_brightness;
out vec4 f_color;

const float PI = 3.14159265358979;

void main() {
    vec4 far = u_inv_vp * vec4(v_ndc, 1.0, 1.0);
    vec3 dir = normalize(far.xyz / far.w);

    // +Y is left, so -y is to the viewer's right; measuring azimuth towards -y makes it
    // increase rightwards, which is the direction a panorama unrolls.
    float azimuth = atan(-dir.y, dir.x) - u_yaw;
    azimuth = mod(azimuth + PI, 2.0 * PI) - PI;
    float v = 0.5 - asin(clamp(dir.z, -1.0, 1.0)) / PI;

    float u;
    if (u_hemisphere == 1) {
        // Nothing behind you. Clamping instead would smear the rim of the image all the way
        // round, which looks like a rendering fault rather than a missing half of the source.
        if (abs(azimuth) > PI * 0.5) {
            f_color = vec4(0.0, 0.0, 0.0, 1.0);
            return;
        }
        u = 0.5 + azimuth / PI;
    } else {
        u = 0.5 + azimuth / (2.0 * PI);
    }

    vec2 uv = vec2(mix(u_rect.x, u_rect.z, u), mix(u_rect.y, u_rect.w, v));
    f_color = vec4(texture(u_sky, uv).rgb * u_brightness, 1.0);
}
"#;

/// Glass bubbles.
///
/// The quad carries an implicit hemisphere: the fragment's distance from the centre gives a
/// surface normal without any geometry. That keeps a bubble to two triangles, which matters
/// when there are twenty of them on an 8-CU GPU.
const BUBBLE_FRAG: &str = r#"
in vec2 v_uv;
uniform sampler2D u_sky;
uniform sampler2D u_icon;
uniform vec4 u_rect;
uniform float u_yaw;
uniform int u_hemisphere;
// World-space basis of the quad: columns are right, up, and the outward normal.
uniform mat3 u_basis;
// Direction from the eye to the bubble's centre, world space, normalised.
uniform vec3 u_view_dir;
uniform vec3 u_light_dir;
// 0 for a resting bubble, 1 for the focused one.
uniform float u_focus;
uniform float u_has_icon;
uniform vec4 u_accent;
// 0 while a bubble is arriving or leaving, 1 once settled. Multiplies the whole bubble out
// rather than only its glass, so an appearing icon fades with the sphere around it.
uniform float u_appear;
out vec4 f_color;

const float PI = 3.14159265358979;

vec3 sample_sky(vec3 dir) {
    float azimuth = atan(-dir.y, dir.x) - u_yaw;
    azimuth = mod(azimuth + PI, 2.0 * PI) - PI;
    float v = 0.5 - asin(clamp(dir.z, -1.0, 1.0)) / PI;
    if (u_hemisphere == 1 && abs(azimuth) > PI * 0.5) {
        return vec3(0.0);
    }
    float u = (u_hemisphere == 1) ? 0.5 + azimuth / PI : 0.5 + azimuth / (2.0 * PI);
    return texture(u_sky, vec2(mix(u_rect.x, u_rect.z, u), mix(u_rect.y, u_rect.w, v))).rgb;
}

void main() {
    // -1..1 across the quad, with +y up.
    vec2 p = vec2(v_uv.x, 1.0 - v_uv.y) * 2.0 - 1.0;
    float r2 = dot(p, p);
    if (r2 > 1.0) {
        discard;
    }
    float r = sqrt(r2);
    // Implicit hemisphere: z falls to zero at the rim, giving a normal that turns away from
    // the viewer exactly as a sphere's would.
    float z = sqrt(max(1.0 - r2, 0.0));
    vec3 normal = normalize(u_basis * vec3(p, z));

    // Fresnel: glass is almost a mirror at grazing angles and almost clear head-on. This is
    // the single term that makes it read as glass rather than as a tinted ball.
    float facing = clamp(dot(normal, -u_view_dir), 0.0, 1.0);
    float fresnel = 0.04 + 0.96 * pow(1.0 - facing, 5.0);

    vec3 refracted = refract(u_view_dir, normal, 1.0 / 1.45);
    // Total internal reflection leaves refract() returning zero; fall back to the view
    // direction so the centre of a steeply-viewed bubble does not go black.
    if (dot(refracted, refracted) < 1e-6) {
        refracted = u_view_dir;
    }
    vec3 body = sample_sky(refracted);
    vec3 rim = sample_sky(reflect(u_view_dir, normal));
    vec3 colour = mix(body, rim, fresnel);

    // A soft specular, and a brighter ring right at the silhouette. Together they give the
    // bubble an edge without an outline, which is what stops it looking like a flat disc.
    float specular = pow(clamp(dot(normal, u_light_dir), 0.0, 1.0), 48.0);
    float edge = smoothstep(0.72, 1.0, r);
    colour += vec3(specular) * (0.5 + 0.5 * u_focus);
    colour += u_accent.rgb * edge * (0.10 + 0.55 * u_focus);
    // Darken the very bottom inside face, the way a real bead of glass shadows itself.
    colour *= 1.0 - 0.25 * smoothstep(0.0, -1.0, p.y) * (1.0 - edge);

    if (u_has_icon > 0.5) {
        // The icon sits inside the glass, so it is drawn a little smaller than the bubble and
        // picks up the same specular highlight.
        vec2 icon_uv = (v_uv - 0.5) / 0.62 + 0.5;
        if (all(greaterThanEqual(icon_uv, vec2(0.0))) && all(lessThanEqual(icon_uv, vec2(1.0)))) {
            vec4 icon = texture(u_icon, icon_uv);
            colour = mix(colour, icon.rgb + vec3(specular * 0.6), icon.a);
        }
    }

    // Anti-alias the silhouette. Without this the bubbles crawl badly as the head moves,
    // which at 640 usable pixels across is very visible.
    float alpha = (0.55 + 0.35 * u_focus) * (1.0 - smoothstep(0.985, 1.0, r));
    f_color = vec4(colour, clamp(alpha + fresnel * 0.35, 0.0, 1.0) * u_appear);
}
"#;

/// The shared unit-quad geometry.
///
/// Owned by [`QuadPipeline`] and referenced by the others as a plain id. Nothing is ever
/// destroyed during a session — the pipelines live as long as the process — so there is no
/// lifetime to model here, and modelling one would mean threading a borrow through every draw
/// call for no benefit.
#[derive(Clone, Copy)]
struct UnitQuad {
    vao: u32,
    #[allow(dead_code)]
    vbo: u32,
}

unsafe fn make_unit_quad(gl: &ffi::Gles2) -> UnitQuad {
    // Two triangles, centred on the origin.
    let verts: [f32; 12] = [
        -0.5, -0.5, 0.5, -0.5, 0.5, 0.5, //
        -0.5, -0.5, 0.5, 0.5, -0.5, 0.5,
    ];
    let mut vbo = 0;
    gl.GenBuffers(1, &mut vbo);
    gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
    gl.BufferData(
        ffi::ARRAY_BUFFER,
        std::mem::size_of_val(&verts) as isize,
        verts.as_ptr() as *const _,
        ffi::STATIC_DRAW,
    );

    let mut vao = 0;
    gl.GenVertexArrays(1, &mut vao);
    gl.BindVertexArray(vao);
    gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
    gl.EnableVertexAttribArray(0);
    gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE, 8, std::ptr::null());
    gl.BindVertexArray(0);
    UnitQuad { vao, vbo }
}

/// A rounded rectangle, drawn from a signed distance field rather than from a texture.
///
/// Untextured, so the shape comes out of arithmetic on the fragment's position: no atlas, no
/// nine-slice, and no distortion when a card is wide and short. Setting the radius to half the
/// shorter side gives a capsule, and to half of a square gives a circle — which is where the
/// touch dots and slider handles come from, rather than from a second asset that would have to
/// be generated, uploaded and kept in step.
///
/// The edge is antialiased across one pixel. On a panel held at arm's length that is the whole
/// difference between "drawn by a program" and "designed".
const ROUNDED_FRAG: &str = r#"
in vec2 v_uv;
uniform vec4 u_tint;
// Size of this quad in panel pixels, so the corner radius and the antialiased edge are both
// measured in the units the layout is written in.
uniform vec2 u_size;
uniform float u_radius;
out vec4 f_color;
void main() {
    vec2 half_size = u_size * 0.5;
    // Distance from the centre, in pixels, folded into one quadrant by the symmetry.
    vec2 d = abs((v_uv - 0.5) * u_size) - (half_size - vec2(u_radius));
    float dist = length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - u_radius;
    // One pixel of coverage either side of the boundary.
    float alpha = 1.0 - smoothstep(-0.7, 0.7, dist);
    f_color = vec4(u_tint.rgb, u_tint.a * alpha);
}
"#;

pub struct RoundedPipeline {
    program: u32,
    vao: u32,
    loc_mvp: i32,
    loc_tint: i32,
    loc_size: i32,
    loc_radius: i32,
}

impl RoundedPipeline {
    pub fn new(renderer: &mut GlesRenderer, quads: &QuadPipeline) -> Result<Self, String> {
        let vao = quads.quad.vao;
        renderer
            .with_context(|gl| unsafe {
                let program = link(gl, QUAD_VERT, ROUNDED_FRAG)?;
                let name = |s: &str| std::ffi::CString::new(s).unwrap();
                let at = |s: &str| gl.GetUniformLocation(program, name(s).as_ptr());
                Ok(Self {
                    program,
                    vao,
                    loc_mvp: at("u_mvp"),
                    loc_tint: at("u_tint"),
                    loc_size: at("u_size"),
                    loc_radius: at("u_radius"),
                })
            })
            .map_err(|e| format!("no GL context: {e}"))?
    }

    /// Draw one rounded rectangle. `size` is in the same pixels the radius is given in.
    ///
    /// # Safety
    /// Must be called with the GL context current.
    pub unsafe fn draw(
        &self,
        gl: &ffi::Gles2,
        mvp: &Mat4,
        tint: [f32; 4],
        size: (f32, f32),
        radius: f32,
    ) {
        gl.UseProgram(self.program);
        gl.BindVertexArray(self.vao);
        gl.Enable(ffi::BLEND);
        gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);
        gl.UniformMatrix4fv(self.loc_mvp, 1, ffi::FALSE, mvp.to_cols_array().as_ptr());
        gl.Uniform4f(self.loc_tint, tint[0], tint[1], tint[2], tint[3]);
        gl.Uniform2f(self.loc_size, size.0.abs(), size.1.abs());
        // A radius past half the shorter side would make the distance field fold back on
        // itself and pinch the shape; clamping means "very round" is expressible as a large
        // number rather than as an exact one the caller has to work out.
        gl.Uniform1f(self.loc_radius, radius.min(size.0.abs().min(size.1.abs()) * 0.5));
        gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        gl.BindVertexArray(0);
    }
}

pub struct QuadPipeline {
    program: u32,
    quad: UnitQuad,
    loc_mvp: i32,
    loc_tex: i32,
    loc_tint: i32,
    loc_uv_range: i32,
}

impl QuadPipeline {
    pub fn new(renderer: &mut GlesRenderer) -> Result<Self, String> {
        renderer
            .with_context(|gl| unsafe { Self::build(gl) })
            .map_err(|e| format!("no GL context: {e}"))?
    }

    unsafe fn build(gl: &ffi::Gles2) -> Result<Self, String> {
        let program = link(gl, QUAD_VERT, QUAD_FRAG)?;
        let quad = make_unit_quad(gl);
        let name = |s: &str| std::ffi::CString::new(s).unwrap();
        Ok(Self {
            program,
            quad,
            loc_mvp: gl.GetUniformLocation(program, name("u_mvp").as_ptr()),
            loc_tex: gl.GetUniformLocation(program, name("u_tex").as_ptr()),
            loc_tint: gl.GetUniformLocation(program, name("u_tint").as_ptr()),
            loc_uv_range: gl.GetUniformLocation(program, name("u_uv_range").as_ptr()),
        })
    }

    /// Draw one textured quad. `mvp` already contains the eye's view-projection.
    ///
    /// # Safety
    /// Must be called with the GL context current, i.e. inside `with_context`.
    pub unsafe fn draw(
        &self,
        gl: &ffi::Gles2,
        texture: u32,
        mvp: &Mat4,
        tint: [f32; 4],
        uv_range: (f32, f32),
    ) {
        gl.UseProgram(self.program);
        gl.BindVertexArray(self.quad.vao);

        gl.Enable(ffi::BLEND);
        // Straight (non-premultiplied) alpha, matching what the text rasteriser produces.
        gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);

        gl.ActiveTexture(ffi::TEXTURE0);
        gl.BindTexture(ffi::TEXTURE_2D, texture);
        gl.Uniform1i(self.loc_tex, 0);
        gl.UniformMatrix4fv(self.loc_mvp, 1, ffi::FALSE, mvp.to_cols_array().as_ptr());
        gl.Uniform4f(self.loc_tint, tint[0], tint[1], tint[2], tint[3]);
        gl.Uniform2f(self.loc_uv_range, uv_range.0, uv_range.1);

        gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        gl.BindVertexArray(0);
    }

    /// # Safety
    /// Context must be current.
    #[allow(dead_code)]
    pub unsafe fn destroy(&self, gl: &ffi::Gles2) {
        gl.DeleteProgram(self.program);
        gl.DeleteBuffers(1, &self.quad.vbo);
        gl.DeleteVertexArrays(1, &self.quad.vao);
    }
}

/// Where in a stereo source one eye's image lives, as `(u0, v0, u1, v1)`.
pub type UvRect = (f32, f32, f32, f32);

pub struct SkyPipeline {
    program: u32,
    vao: u32,
    loc_sky: i32,
    loc_inv_vp: i32,
    loc_rect: i32,
    loc_yaw: i32,
    loc_hemisphere: i32,
    loc_brightness: i32,
}

impl SkyPipeline {
    pub fn new(renderer: &mut GlesRenderer, quads: &QuadPipeline) -> Result<Self, String> {
        let vao = quads.quad.vao;
        renderer
            .with_context(|gl| unsafe {
                let program = link(gl, SKY_VERT, SKY_FRAG)?;
                let name = |s: &str| std::ffi::CString::new(s).unwrap();
                Ok(Self {
                    program,
                    vao,
                    loc_sky: gl.GetUniformLocation(program, name("u_sky").as_ptr()),
                    loc_inv_vp: gl.GetUniformLocation(program, name("u_inv_vp").as_ptr()),
                    loc_rect: gl.GetUniformLocation(program, name("u_rect").as_ptr()),
                    loc_yaw: gl.GetUniformLocation(program, name("u_yaw").as_ptr()),
                    loc_hemisphere: gl.GetUniformLocation(program, name("u_hemisphere").as_ptr()),
                    loc_brightness: gl.GetUniformLocation(program, name("u_brightness").as_ptr()),
                })
            })
            .map_err(|e| format!("no GL context: {e}"))?
    }

    /// Fill the viewport with the environment.
    ///
    /// `inv_view_projection` must be built from a view matrix with **translation removed** —
    /// the sky is at infinity, and letting the eye offset through makes the background shift
    /// with the neck model, which reads as the world sliding around you.
    ///
    /// # Safety
    /// Context must be current.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw(
        &self,
        gl: &ffi::Gles2,
        texture: u32,
        inv_view_projection: &Mat4,
        rect: UvRect,
        yaw_offset: f32,
        hemisphere: bool,
        brightness: f32,
    ) {
        gl.UseProgram(self.program);
        gl.BindVertexArray(self.vao);
        // Opaque and behind everything: no blending, and nothing has been drawn yet, so there
        // is nothing to blend against anyway.
        gl.Disable(ffi::BLEND);

        gl.ActiveTexture(ffi::TEXTURE0);
        gl.BindTexture(ffi::TEXTURE_2D, texture);
        gl.Uniform1i(self.loc_sky, 0);
        gl.UniformMatrix4fv(
            self.loc_inv_vp,
            1,
            ffi::FALSE,
            inv_view_projection.to_cols_array().as_ptr(),
        );
        gl.Uniform4f(self.loc_rect, rect.0, rect.1, rect.2, rect.3);
        gl.Uniform1f(self.loc_yaw, yaw_offset);
        gl.Uniform1i(self.loc_hemisphere, i32::from(hemisphere));
        gl.Uniform1f(self.loc_brightness, brightness);

        gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        gl.BindVertexArray(0);
    }
}

/// Everything a bubble needs that varies per bubble.
pub struct BubbleParams<'a> {
    pub mvp: &'a Mat4,
    /// Columns are the quad's world-space right, up and outward normal.
    pub basis: Mat3,
    /// Eye to bubble centre, normalised.
    pub view_dir: Vec3,
    pub light_dir: Vec3,
    /// 0 for resting, 1 for focused.
    pub focus: f32,
    pub icon: Option<u32>,
    pub accent: [f32; 4],
    /// 0..1 arrival progress.
    pub appear: f32,
}

pub struct BubblePipeline {
    program: u32,
    vao: u32,
    loc_mvp: i32,
    loc_sky: i32,
    loc_icon: i32,
    loc_rect: i32,
    loc_yaw: i32,
    loc_hemisphere: i32,
    loc_basis: i32,
    loc_view_dir: i32,
    loc_light_dir: i32,
    loc_focus: i32,
    loc_has_icon: i32,
    loc_accent: i32,
    loc_appear: i32,
}

impl BubblePipeline {
    pub fn new(renderer: &mut GlesRenderer, quads: &QuadPipeline) -> Result<Self, String> {
        let vao = quads.quad.vao;
        renderer
            .with_context(|gl| unsafe {
                let program = link(gl, QUAD_VERT, BUBBLE_FRAG)?;
                let name = |s: &str| std::ffi::CString::new(s).unwrap();
                let at = |s: &str| gl.GetUniformLocation(program, name(s).as_ptr());
                Ok(Self {
                    program,
                    vao,
                    loc_mvp: at("u_mvp"),
                    loc_sky: at("u_sky"),
                    loc_icon: at("u_icon"),
                    loc_rect: at("u_rect"),
                    loc_yaw: at("u_yaw"),
                    loc_hemisphere: at("u_hemisphere"),
                    loc_basis: at("u_basis"),
                    loc_view_dir: at("u_view_dir"),
                    loc_light_dir: at("u_light_dir"),
                    loc_focus: at("u_focus"),
                    loc_has_icon: at("u_has_icon"),
                    loc_accent: at("u_accent"),
                    loc_appear: at("u_appear"),
                })
            })
            .map_err(|e| format!("no GL context: {e}"))?
    }

    /// Bind the environment. Called once per eye; the per-bubble state is in [`Self::draw`].
    ///
    /// # Safety
    /// Context must be current.
    pub unsafe fn begin(
        &self,
        gl: &ffi::Gles2,
        sky: u32,
        rect: UvRect,
        yaw_offset: f32,
        hemisphere: bool,
    ) {
        gl.UseProgram(self.program);
        gl.Enable(ffi::BLEND);
        gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);
        gl.ActiveTexture(ffi::TEXTURE0);
        gl.BindTexture(ffi::TEXTURE_2D, sky);
        gl.Uniform1i(self.loc_sky, 0);
        gl.Uniform4f(self.loc_rect, rect.0, rect.1, rect.2, rect.3);
        gl.Uniform1f(self.loc_yaw, yaw_offset);
        gl.Uniform1i(self.loc_hemisphere, i32::from(hemisphere));
    }

    /// # Safety
    /// Context must be current, and [`Self::begin`] must have run for this eye.
    pub unsafe fn draw(&self, gl: &ffi::Gles2, params: &BubbleParams) {
        gl.UseProgram(self.program);
        gl.BindVertexArray(self.vao);

        gl.UniformMatrix4fv(
            self.loc_mvp,
            1,
            ffi::FALSE,
            params.mvp.to_cols_array().as_ptr(),
        );
        gl.UniformMatrix3fv(
            self.loc_basis,
            1,
            ffi::FALSE,
            params.basis.to_cols_array().as_ptr(),
        );
        gl.Uniform3f(
            self.loc_view_dir,
            params.view_dir.x,
            params.view_dir.y,
            params.view_dir.z,
        );
        gl.Uniform3f(
            self.loc_light_dir,
            params.light_dir.x,
            params.light_dir.y,
            params.light_dir.z,
        );
        gl.Uniform1f(self.loc_focus, params.focus);
        gl.Uniform1f(self.loc_appear, params.appear);
        gl.Uniform4f(
            self.loc_accent,
            params.accent[0],
            params.accent[1],
            params.accent[2],
            params.accent[3],
        );

        // Texture unit 1 for the icon, so binding it never disturbs the environment map on
        // unit 0 — which is shared by every bubble in the pass.
        gl.ActiveTexture(ffi::TEXTURE1);
        gl.BindTexture(ffi::TEXTURE_2D, params.icon.unwrap_or(0));
        gl.Uniform1i(self.loc_icon, 1);
        gl.Uniform1f(self.loc_has_icon, if params.icon.is_some() { 1.0 } else { 0.0 });
        gl.ActiveTexture(ffi::TEXTURE0);

        gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        gl.BindVertexArray(0);
    }
}

/// Upload an RGBA image as a texture.
///
/// # Safety
/// Context must be current.
pub unsafe fn upload_rgba(gl: &ffi::Gles2, image: &TextImage) -> u32 {
    upload_raw(gl, image.width, image.height, &image.rgba, false)
}

/// Upload raw RGBA bytes.
///
/// `wrap_horizontally` matters only for the environment map: an equirectangular image joins
/// itself at the seam, and clamping there leaves a visible vertical line down the world.
///
/// # Safety
/// Context must be current. `rgba` must be `width * height * 4` bytes.
pub unsafe fn upload_raw(
    gl: &ffi::Gles2,
    width: u32,
    height: u32,
    rgba: &[u8],
    wrap_horizontally: bool,
) -> u32 {
    let mut tex = 0;
    gl.GenTextures(1, &mut tex);
    gl.BindTexture(ffi::TEXTURE_2D, tex);
    // Text bitmaps are tightly packed and rarely a multiple of four bytes wide; the default
    // 4-byte unpack alignment shears them diagonally, which looks like a transform bug.
    gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);
    gl.TexImage2D(
        ffi::TEXTURE_2D,
        0,
        ffi::RGBA as i32,
        width as i32,
        height as i32,
        0,
        ffi::RGBA,
        ffi::UNSIGNED_BYTE,
        rgba.as_ptr() as *const _,
    );
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
    let wrap_s = if wrap_horizontally {
        ffi::REPEAT
    } else {
        ffi::CLAMP_TO_EDGE
    };
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, wrap_s as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
    tex
}

/// A 1x1 white texture, so the plain quad pipeline can draw untextured shapes — the laser
/// beam and its reticle — without a second shader.
///
/// # Safety
/// Context must be current.
pub unsafe fn white_texture(gl: &ffi::Gles2) -> u32 {
    upload_raw(gl, 1, 1, &[255, 255, 255, 255], false)
}

unsafe fn link(gl: &ffi::Gles2, vert: &str, frag: &str) -> Result<u32, String> {
    let vs = compile(gl, ffi::VERTEX_SHADER, &format!("{PREAMBLE}{vert}"))?;
    let fs = compile(gl, ffi::FRAGMENT_SHADER, &format!("{PREAMBLE}{frag}"))?;
    let program = gl.CreateProgram();
    gl.AttachShader(program, vs);
    gl.AttachShader(program, fs);
    gl.LinkProgram(program);

    let mut ok = 0;
    gl.GetProgramiv(program, ffi::LINK_STATUS, &mut ok);
    if ok == 0 {
        let mut len = 0;
        gl.GetProgramiv(program, ffi::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0u8; len.max(1) as usize];
        gl.GetProgramInfoLog(program, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
        return Err(format!("link failed: {}", String::from_utf8_lossy(&buf)));
    }
    gl.DeleteShader(vs);
    gl.DeleteShader(fs);
    Ok(program)
}

unsafe fn compile(gl: &ffi::Gles2, kind: u32, source: &str) -> Result<u32, String> {
    let shader = gl.CreateShader(kind);
    let ptr = source.as_ptr() as *const _;
    let len = source.len() as i32;
    gl.ShaderSource(shader, 1, &ptr, &len);
    gl.CompileShader(shader);

    let mut ok = 0;
    gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut ok);
    if ok == 0 {
        let mut len = 0;
        gl.GetShaderiv(shader, ffi::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0u8; len.max(1) as usize];
        gl.GetShaderInfoLog(shader, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
        return Err(format!("compile failed: {}", String::from_utf8_lossy(&buf)));
    }
    Ok(shader)
}
