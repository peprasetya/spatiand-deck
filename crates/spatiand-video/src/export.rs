//! Lending a VA-API surface to the compositor, by asking VA-API directly.
//!
//! libavutil has a way to do this — `av_hwframe_map` to `AV_PIX_FMT_DRM_PRIME` — and this asks
//! libva itself instead, with `SEPARATE_LAYERS`, the way mpv's `hwdec_vaapi.c` does. The
//! difference is that nothing here holds a libavutil mapping alive: the descriptors are owned
//! by the [`Exported`] and closed when it goes, and nothing pins a surface to keep them.
//!
//! It is one `vaExportSurfaceHandle` call. The descriptor below is `va/va_drmcommon.h`,
//! hand-declared for the same reason the DRM frame descriptor is: the bindings do not carry
//! that header.

use std::os::fd::RawFd;

/// `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2` — the descriptor form with modifiers in it. The
/// older `DRM_PRIME` has no modifier field at all, which on a tiled GPU means "guess".
const MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
const EXPORT_READ_ONLY: u32 = 0x0001;
/// Ask for each plane to be described separately rather than folded into one layer. This is
/// the flag that makes the compression planes visible.
const EXPORT_SEPARATE_LAYERS: u32 = 0x0004;

const MAX: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Object {
    fd: std::os::raw::c_int,
    size: u32,
    drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Layer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; MAX],
    offset: [u32; MAX],
    pitch: [u32; MAX],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [Object; MAX],
    num_layers: u32,
    layers: [Layer; MAX],
}

impl Default for Descriptor {
    fn default() -> Self {
        Descriptor {
            fourcc: 0,
            width: 0,
            height: 0,
            num_objects: 0,
            objects: [Object::default(); MAX],
            num_layers: 0,
            layers: [Layer::default(); MAX],
        }
    }
}

#[link(name = "va")]
extern "C" {
    fn vaExportSurfaceHandle(
        display: *mut std::ffi::c_void,
        surface: u32,
        mem_type: u32,
        flags: u32,
        descriptor: *mut std::ffi::c_void,
    ) -> i32;
}

/// One surface, described the way a compositor needs it.
///
/// The descriptors are owned: dropping this closes them. Nothing else holds them, which is
/// the other half of why this exists — the libavutil mapping owned its descriptors and had to
/// be kept alive, and keeping it alive pinned the surface so the pool could never reuse one.
pub struct Exported {
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    /// One per plane, in order: descriptor, offset, stride.
    pub planes: Vec<(RawFd, u32, u32)>,
    owned: Vec<RawFd>,
}

impl Drop for Exported {
    fn drop(&mut self) {
        for fd in self.owned.drain(..) {
            // SAFETY: these came from vaExportSurfaceHandle and are ours to close.
            unsafe { libc::close(fd) };
        }
    }
}

/// Export a surface as dmabuf planes.
///
/// `display` is the `VADisplay` out of libavutil's VA-API device context.
pub fn export(
    display: *mut std::ffi::c_void,
    surface: u32,
    width: u32,
    height: u32,
) -> Result<Exported, String> {
    let mut descriptor = Descriptor::default();
    let status = unsafe {
        vaExportSurfaceHandle(
            display,
            surface,
            MEM_TYPE_DRM_PRIME_2,
            EXPORT_READ_ONLY | EXPORT_SEPARATE_LAYERS,
            (&mut descriptor as *mut Descriptor).cast(),
        )
    };
    if status != 0 {
        return Err(format!("could not export a surface: VA status {status}"));
    }
    if descriptor.num_layers == 0 || descriptor.num_objects == 0 {
        return Err("a surface exported with nothing in it".into());
    }

    let owned: Vec<RawFd> = (0..descriptor.num_objects as usize)
        .map(|i| descriptor.objects[i].fd)
        .collect();

    // **Every plane of every layer, in order.** With separate layers a picture with
    // compression metadata comes back as more than one layer, and the compositor needs all of
    // them: the first is the colour, the rest say how to read it.
    let mut planes = Vec::new();
    for l in 0..descriptor.num_layers as usize {
        let layer = &descriptor.layers[l];
        for p in 0..layer.num_planes as usize {
            let object = layer.object_index[p] as usize;
            planes.push((
                descriptor.objects[object].fd,
                layer.offset[p],
                layer.pitch[p],
            ));
        }
    }

    Ok(Exported {
        // **The layer's format, not the descriptor's.** The descriptor's `fourcc` is a *VA*
        // fourcc — `BGRA` spelt out — and the layer's is a *DRM* one — `AR24`. They are
        // different namespaces that happen to both be four bytes, and handing a compositor
        // the VA spelling names a format no DRM driver has ever heard of.
        fourcc: descriptor.layers[0].drm_format,
        modifier: descriptor.objects[0].drm_format_modifier,
        width,
        height,
        planes,
        owned,
    })
}

/// What the driver actually said, for the log. Worth one line per stream: format, modifier
/// and stride are the first things to rule out when a picture arrives wrong.
pub fn describe(exported: &Exported) -> String {
    let fourcc = exported.fourcc.to_le_bytes();
    format!(
        "{}x{} {} modifier {:#x}, {} plane(s): {}",
        exported.width,
        exported.height,
        String::from_utf8_lossy(&fourcc),
        exported.modifier,
        exported.planes.len(),
        exported
            .planes
            .iter()
            .map(|(_, offset, pitch)| format!("+{offset}/{pitch}"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}
