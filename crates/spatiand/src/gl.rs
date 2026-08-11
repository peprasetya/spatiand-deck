//! The GLES 3.2 drawing layer.
//!
//! Deliberately small: one shader, one quad, one texture binding point. Everything Spatiand
//! draws — window surfaces, glass bubbles, text panels, the skybox — is a textured quad with
//! a transform, so a single pipeline covers the lot until there is a measured reason for more.
//!
//! This lives in the `spatiand` crate rather than `spatiand-render` because it needs
//! Smithay's GL context. `spatiand-render` stays GPU-free and therefore testable anywhere.

use smithay::backend::renderer::gles::{ffi, GlesRenderer};

use glam::Mat4;
use spatiand_render::text::TextImage;

/// Vertex shader: an untransformed unit quad, positioned entirely by the MVP.
const VERT: &str = r#"#version 320 es
precision highp float;
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

const FRAG: &str = r#"#version 320 es
precision highp float;
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

pub struct QuadPipeline {
    program: u32,
    vbo: u32,
    vao: u32,
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
        let program = link(gl, VERT, FRAG)?;

        // A unit quad centred on the origin, two triangles.
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

        let name = |s: &str| std::ffi::CString::new(s).unwrap();
        Ok(Self {
            program,
            vbo,
            vao,
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
        gl.BindVertexArray(self.vao);

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
    pub unsafe fn destroy(&self, gl: &ffi::Gles2) {
        gl.DeleteProgram(self.program);
        gl.DeleteBuffers(1, &self.vbo);
        gl.DeleteVertexArrays(1, &self.vao);
    }
}

/// Upload an RGBA image as a texture.
///
/// # Safety
/// Context must be current.
pub unsafe fn upload_rgba(gl: &ffi::Gles2, image: &TextImage) -> u32 {
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
        image.width as i32,
        image.height as i32,
        0,
        ffi::RGBA,
        ffi::UNSIGNED_BYTE,
        image.rgba.as_ptr() as *const _,
    );
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
    tex
}

unsafe fn link(gl: &ffi::Gles2, vert: &str, frag: &str) -> Result<u32, String> {
    let vs = compile(gl, ffi::VERTEX_SHADER, vert)?;
    let fs = compile(gl, ffi::FRAGMENT_SHADER, frag)?;
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
