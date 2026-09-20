//! Turning a decoded picture into something the compositor can draw.
//!
//! A decoder produces NV12 — luma and chroma in separate planes, which is what video is. The
//! compositor's shaders sample ordinary RGB textures, so somewhere between the two the colours
//! have to be converted. It happens here, on the GPU's video post-processor: the block exists,
//! it costs a fraction of a millisecond, and nothing in the compositor has to change to show a
//! remote window — it receives a buffer in a format it already knows.
//!
//! ## Why the output surfaces are ours, and linear
//!
//! This used to be libavfilter's `scale_vaapi`, which allocates its own output. Two things were
//! wrong with that, and neither was the flat grey window it was blamed for — that turned out to
//! be the session skipping compressed frames; see `remote/session.rs`.
//!
//! * **It leaked.** Keeping a surface's export alive pinned the surface, so the filter
//!   allocated a fresh four-megabyte one for the next frame, and the next.
//! * **Its surfaces are DCC-compressed** — modifier `0x200000008401b03`, `DCC` and
//!   `DCC_RETILE` set — yet the driver exports them as *one* plane, where a DCC_RETILE buffer
//!   is three. Whether a compositor can sample that correctly was never settled: by the time
//!   the real fault was found, this had already been replaced. Asking for a layout both ends
//!   plainly understand costs nothing and leaves nothing to find out.
//!
//! So the post-processor writes into surfaces allocated here:
//!
//! * **A fixed pool**, a few surfaces deep, **linear**, the modifier *asked for* rather than
//!   chosen by the driver.
//! * **Exported once each**, when the pool is made. A surface's dmabuf does not change.
//! * **Lent, not given.** A [`Converted`] holds its surface until it is dropped, and the remote
//!   client drops it only when the compositor releases the buffer — so the post-processor can
//!   never write into a picture that is still on screen.
//!
//! Linear costs something to sample compared with a tiled layout, and for a window-sized
//! picture that is nothing next to the decode. Choosing a tiled, *uncompressed* modifier from
//! the compositor's own list is the refinement, if it is ever worth it.
//!
//! The calls are libva's own: `vaCreateSurfaces` with a modifier list, then
//! `vaBeginPicture`/`vaRenderPicture`/`vaEndPicture` with one pipeline buffer. The few structures
//! and constants involved are declared by hand, and every size and value was measured from
//! libva 2.22's headers with a C program on the Deck; see [`va`].

use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};

use rsmpeg::ffi;

use crate::decode::{Picture, VideoError};
use crate::export::{self, Exported};

type Result<T> = std::result::Result<T, VideoError>;

/// libva, the parts used here. Values measured against libva 2.22 (`va.h`, `va_vpp.h`,
/// `va_drmcommon.h`) on the Deck, not copied from memory.
mod va {
    use std::ffi::c_void;

    pub type Display = *mut c_void;

    pub const PROFILE_NONE: i32 = -1;
    pub const ENTRYPOINT_VIDEO_PROC: i32 = 10;
    pub const PROC_PIPELINE_PARAMETER_BUFFER: i32 = 41;
    /// `sizeof(VAProcPipelineParameterBuffer)` on x86_64. Zero everywhere unset is "default
    /// colour handling, no filters". Three fields are set, at offsets measured from the libva
    /// 2.22 headers: the source surface, and the two regions.
    pub const PIPELINE_BUFFER_SIZE: usize = 224;
    /// `surface_region`, a `const VARectangle *`.
    pub const PIPELINE_SURFACE_REGION: usize = 8;
    /// `output_region`, the same.
    pub const PIPELINE_OUTPUT_REGION: usize = 24;

    /// `VARectangle`, 8 bytes.
    #[repr(C)]
    pub struct Rectangle {
        pub x: i16,
        pub y: i16,
        pub width: u16,
        pub height: u16,
    }

    pub const RT_FORMAT_RGB32: u32 = 0x0002_0000;
    pub const FOURCC_BGRA: u32 = 0x4152_4742;
    pub const PROGRESSIVE: i32 = 1;

    pub const ATTRIB_PIXEL_FORMAT: i32 = 1;
    pub const ATTRIB_USAGE_HINT: i32 = 8;
    pub const ATTRIB_DRM_FORMAT_MODIFIERS: i32 = 9;
    pub const ATTRIB_SETTABLE: u32 = 2;
    pub const VALUE_INTEGER: i32 = 1;
    pub const VALUE_POINTER: i32 = 3;
    pub const USAGE_VPP_WRITE: i32 = 0x8;
    pub const USAGE_EXPORT: i32 = 0x20;

    pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

    /// `VAGenericValue`: a type, then an 8-byte union at offset 8.
    #[repr(C)]
    pub struct GenericValue {
        pub kind: i32,
        pub value: u64,
    }

    /// `VASurfaceAttrib`, 24 bytes.
    #[repr(C)]
    pub struct SurfaceAttrib {
        pub kind: i32,
        pub flags: u32,
        pub value: GenericValue,
    }

    /// `VADRMFormatModifierList`, 16 bytes, the list at offset 8.
    #[repr(C)]
    pub struct ModifierList {
        pub num_modifiers: u32,
        pub modifiers: *const u64,
    }

    const _: () = assert!(std::mem::size_of::<SurfaceAttrib>() == 24);
    const _: () = assert!(std::mem::size_of::<ModifierList>() == 16);

    #[link(name = "va")]
    extern "C" {
        pub fn vaCreateConfig(
            dpy: Display,
            profile: i32,
            entrypoint: i32,
            attribs: *mut c_void,
            num_attribs: i32,
            config: *mut u32,
        ) -> i32;
        pub fn vaDestroyConfig(dpy: Display, config: u32) -> i32;
        pub fn vaCreateSurfaces(
            dpy: Display,
            format: u32,
            width: u32,
            height: u32,
            surfaces: *mut u32,
            num_surfaces: u32,
            attribs: *mut SurfaceAttrib,
            num_attribs: u32,
        ) -> i32;
        pub fn vaDestroySurfaces(dpy: Display, surfaces: *mut u32, num: i32) -> i32;
        pub fn vaCreateContext(
            dpy: Display,
            config: u32,
            width: i32,
            height: i32,
            flag: i32,
            targets: *mut u32,
            num_targets: i32,
            context: *mut u32,
        ) -> i32;
        pub fn vaDestroyContext(dpy: Display, context: u32) -> i32;
        pub fn vaCreateBuffer(
            dpy: Display,
            context: u32,
            kind: i32,
            size: u32,
            num_elements: u32,
            data: *mut c_void,
            buffer: *mut u32,
        ) -> i32;
        pub fn vaDestroyBuffer(dpy: Display, buffer: u32) -> i32;
        pub fn vaBeginPicture(dpy: Display, context: u32, target: u32) -> i32;
        pub fn vaRenderPicture(dpy: Display, context: u32, buffers: *mut u32, num: i32) -> i32;
        pub fn vaEndPicture(dpy: Display, context: u32) -> i32;
        pub fn vaSyncSurface(dpy: Display, surface: u32) -> i32;
    }
}

/// libavutil's VA-API device, declared here because the bindings do not carry
/// `hwcontext_vaapi.h`. Two fields, unchanged since the API appeared.
#[repr(C)]
struct VaapiDeviceContext {
    display: *mut std::ffi::c_void,
    driver_quirks: std::ffi::c_uint,
}

/// How many pictures can be out at once. The remote client keeps at most two with the
/// compositor plus the one on screen; one more is the one being written.
const POOL: usize = 5;

fn check(what: &str, status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(VideoError::new(format!("{what}: VA status {status}")))
    }
}

/// A picture in a format a compositor can import directly.
///
/// Holds its surface out of the pool until dropped.
pub struct Converted {
    pub width: u32,
    pub height: u32,
    /// A `DRM_FORMAT_*` fourcc: `AR24`.
    pub fourcc: u32,
    pub modifier: u64,
    /// One per plane: descriptor, offset, stride. Linear RGB, so one.
    pub planes: Vec<(RawFd, u32, u32)>,
    pub timestamp: i64,
    _lease: Lease,
}

impl Converted {
    /// Copy the picture into memory. For tests and for writing a file to look at.
    ///
    /// Reads the dmabuf directly, which a linear buffer allows: what is read is exactly what
    /// the compositor is given, which is the point of looking.
    pub fn to_bgra(&self) -> Result<Vec<u8>> {
        let (fd, offset, stride) = *self
            .planes
            .first()
            .ok_or_else(|| VideoError::new("a picture with no planes"))?;
        let length = offset as usize + stride as usize * self.height as usize;
        unsafe {
            let map = libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if map == libc::MAP_FAILED {
                return Err(VideoError::new("could not map a picture to read it"));
            }
            let base = (map as *const u8).add(offset as usize);
            let mut out = Vec::with_capacity((self.width * self.height * 4) as usize);
            for row in 0..self.height as usize {
                out.extend_from_slice(std::slice::from_raw_parts(
                    base.add(row * stride as usize),
                    self.width as usize * 4,
                ));
            }
            libc::munmap(map, length);
            Ok(out)
        }
    }
}

/// One surface out of the pool, returned when this is dropped.
struct Lease {
    index: usize,
    free: Arc<Mutex<Vec<usize>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut free) = self.free.lock() {
            free.push(self.index);
        }
    }
}

/// NV12 in, BGRA out, on the GPU, into surfaces this owns.
pub struct Converter {
    display: va::Display,
    config: u32,
    context: u32,
    surfaces: Vec<u32>,
    /// Each surface's dmabuf, made once. Closed when the converter goes; a compositor that
    /// imported one holds its own reference to the memory by then.
    exported: Vec<Exported>,
    free: Arc<Mutex<Vec<usize>>>,
    size: (u32, u32),
    /// Kept so the device outlives everything made on it.
    device: *mut ffi::AVBufferRef,
}

// SAFETY: as for the decoder — owned here, freed here, one thread.
unsafe impl Send for Converter {}

impl Converter {
    /// Build one for a stream of this size, on the decoder's own device.
    ///
    /// `_frames` is the decoder's frame pool; unused now that the output is allocated here, and
    /// kept so the caller did not have to change with it.
    pub fn new(
        device: *mut ffi::AVBufferRef,
        _frames: *mut ffi::AVBufferRef,
        size: (u32, u32),
    ) -> Result<Converter> {
        unsafe {
            let hw = (*device).data as *mut ffi::AVHWDeviceContext;
            let vaapi = (*hw).hwctx as *mut VaapiDeviceContext;
            if vaapi.is_null() || (*vaapi).display.is_null() {
                return Err(VideoError::new("the decoder has no VA display"));
            }
            let display = (*vaapi).display;

            let mut config = 0u32;
            check(
                "no video post-processor on this GPU",
                va::vaCreateConfig(
                    display,
                    va::PROFILE_NONE,
                    va::ENTRYPOINT_VIDEO_PROC,
                    std::ptr::null_mut(),
                    0,
                    &mut config,
                ),
            )?;

            // The whole point: asked for linear, not left to the driver.
            let modifiers = [va::DRM_FORMAT_MOD_LINEAR];
            let list = va::ModifierList {
                num_modifiers: modifiers.len() as u32,
                modifiers: modifiers.as_ptr(),
            };
            let mut attribs = [
                va::SurfaceAttrib {
                    kind: va::ATTRIB_PIXEL_FORMAT,
                    flags: va::ATTRIB_SETTABLE,
                    value: va::GenericValue {
                        kind: va::VALUE_INTEGER,
                        value: va::FOURCC_BGRA as u64,
                    },
                },
                va::SurfaceAttrib {
                    kind: va::ATTRIB_USAGE_HINT,
                    flags: va::ATTRIB_SETTABLE,
                    value: va::GenericValue {
                        kind: va::VALUE_INTEGER,
                        value: (va::USAGE_VPP_WRITE | va::USAGE_EXPORT) as u64,
                    },
                },
                va::SurfaceAttrib {
                    kind: va::ATTRIB_DRM_FORMAT_MODIFIERS,
                    flags: va::ATTRIB_SETTABLE,
                    value: va::GenericValue {
                        kind: va::VALUE_POINTER,
                        value: &list as *const va::ModifierList as u64,
                    },
                },
            ];
            let mut surfaces = vec![0u32; POOL];
            let status = va::vaCreateSurfaces(
                display,
                va::RT_FORMAT_RGB32,
                size.0,
                size.1,
                surfaces.as_mut_ptr(),
                POOL as u32,
                attribs.as_mut_ptr(),
                attribs.len() as u32,
            );
            if status != 0 {
                va::vaDestroyConfig(display, config);
                return Err(VideoError::new(format!(
                    "could not make linear output surfaces: VA status {status}"
                )));
            }

            let mut context = 0u32;
            let status = va::vaCreateContext(
                display,
                config,
                size.0 as i32,
                size.1 as i32,
                va::PROGRESSIVE,
                surfaces.as_mut_ptr(),
                POOL as i32,
                &mut context,
            );
            if status != 0 {
                va::vaDestroySurfaces(display, surfaces.as_mut_ptr(), POOL as i32);
                va::vaDestroyConfig(display, config);
                return Err(VideoError::new(format!(
                    "could not start the post-processor: VA status {status}"
                )));
            }

            let mut exported = Vec::with_capacity(POOL);
            for &surface in &surfaces {
                match export::export(display, surface, size.0, size.1) {
                    Ok(e) => exported.push(e),
                    Err(e) => {
                        va::vaDestroyContext(display, context);
                        va::vaDestroySurfaces(display, surfaces.as_mut_ptr(), POOL as i32);
                        va::vaDestroyConfig(display, config);
                        return Err(VideoError::new(e));
                    }
                }
            }
            log::info!(
                "post-processor output: {} x{POOL}",
                export::describe(&exported[0])
            );
            if exported[0].modifier != va::DRM_FORMAT_MOD_LINEAR {
                log::warn!(
                    "asked for linear output and got modifier {:#x}; the picture may be wrong",
                    exported[0].modifier
                );
            }

            Ok(Converter {
                display,
                config,
                context,
                surfaces,
                exported,
                free: Arc::new(Mutex::new((0..POOL).rev().collect())),
                size,
                device: ffi::av_buffer_ref(device),
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Convert one decoded picture into a surface from the pool.
    pub fn convert(&mut self, picture: &Picture) -> Result<Converted> {
        let index = self
            .free
            .lock()
            .ok()
            .and_then(|mut free| free.pop())
            .ok_or_else(|| {
                VideoError::new("every output surface is still with the compositor")
            })?;
        // Taken now, so an early return below gives it straight back.
        let lease = Lease {
            index,
            free: self.free.clone(),
        };
        let target = self.surfaces[index];
        let source = unsafe { (*picture.frame()).data[3] as usize as u32 };

        unsafe {
            // **The picture, not the surface it sits in.** A decoder's surface is padded out
            // to the codec's alignment — 1421x954 of Firestorm lives in 1424x960 — and a
            // pipeline with no regions means "the whole surface", padding included. That
            // padding is uninitialised NV12, which is green, and it was scaled into the
            // picture as a bar down the right-hand edge.
            let region = va::Rectangle {
                x: 0,
                y: 0,
                width: self.size.0 as u16,
                height: self.size.1 as u16,
            };
            let mut parameters = [0u8; va::PIPELINE_BUFFER_SIZE];
            parameters[0..4].copy_from_slice(&source.to_ne_bytes());
            let pointer = (&region as *const va::Rectangle as usize).to_ne_bytes();
            parameters[va::PIPELINE_SURFACE_REGION..va::PIPELINE_SURFACE_REGION + 8]
                .copy_from_slice(&pointer);
            parameters[va::PIPELINE_OUTPUT_REGION..va::PIPELINE_OUTPUT_REGION + 8]
                .copy_from_slice(&pointer);
            let mut buffer = 0u32;
            check(
                "could not describe the conversion",
                va::vaCreateBuffer(
                    self.display,
                    self.context,
                    va::PROC_PIPELINE_PARAMETER_BUFFER,
                    va::PIPELINE_BUFFER_SIZE as u32,
                    1,
                    parameters.as_mut_ptr().cast(),
                    &mut buffer,
                ),
            )?;
            let result = (|| {
                check(
                    "could not start a conversion",
                    va::vaBeginPicture(self.display, self.context, target),
                )?;
                check(
                    "the conversion refused its input",
                    va::vaRenderPicture(self.display, self.context, &mut buffer, 1),
                )?;
                check(
                    "the conversion did not finish",
                    va::vaEndPicture(self.display, self.context),
                )?;
                // Queued is not done. The compositor reads this surface as a dmabuf, outside
                // anything that would wait for the post-processor on its behalf.
                check(
                    "waiting for a conversion failed",
                    va::vaSyncSurface(self.display, target),
                )
            })();
            va::vaDestroyBuffer(self.display, buffer);
            result?;
        }

        let exported = &self.exported[index];
        Ok(Converted {
            width: self.size.0,
            height: self.size.1,
            fourcc: exported.fourcc,
            modifier: exported.modifier,
            planes: exported.planes.clone(),
            timestamp: picture.timestamp,
            _lease: lease,
        })
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe {
            self.exported.clear();
            va::vaDestroyContext(self.display, self.context);
            va::vaDestroySurfaces(self.display, self.surfaces.as_mut_ptr(), self.surfaces.len() as i32);
            va::vaDestroyConfig(self.display, self.config);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}
