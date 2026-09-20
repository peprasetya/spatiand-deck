//! One window's decoder: encoded frames in, pictures on the GPU out.
//!
//! Frames go in whole — see [`spatiand_stream::video`] for why a frame with a hole in it is
//! thrown away rather than decoded — and what comes out is a VA-API surface lent out as a
//! dmabuf, which is what a Wayland surface takes. Nothing crosses the CPU.

use std::ffi::{CStr, CString};
use std::ptr;

use rsmpeg::ffi;
use spatiand_stream::Codec;

/// libavutil's description of a dmabuf. Declared here for the same reason as in the host's
/// encoder: `hwcontext_drm.h` is a public header that the bindings do not carry.
pub(crate) mod drm {
    pub const MAX_PLANES: usize = 4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Object {
        pub fd: std::os::raw::c_int,
        pub size: usize,
        pub format_modifier: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Plane {
        pub object_index: std::os::raw::c_int,
        pub offset: isize,
        pub pitch: isize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Layer {
        pub format: u32,
        pub nb_planes: std::os::raw::c_int,
        pub planes: [Plane; MAX_PLANES],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct FrameDescriptor {
        pub nb_objects: std::os::raw::c_int,
        pub objects: [Object; MAX_PLANES],
        pub nb_layers: std::os::raw::c_int,
        pub layers: [Layer; MAX_PLANES],
    }
}

#[derive(Debug)]
pub struct VideoError(String);

impl VideoError {
    pub(crate) fn new(what: impl Into<String>) -> VideoError {
        VideoError(what.into())
    }
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for VideoError {}

type Result<T> = std::result::Result<T, VideoError>;

pub(crate) fn fail(what: &str, code: i32) -> VideoError {
    let mut buf = [0i8; 256];
    let text = unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr().cast(), buf.len());
        CStr::from_ptr(buf.as_ptr().cast())
            .to_string_lossy()
            .into_owned()
    };
    VideoError(format!("{what}: {text} ({code})"))
}

/// One decoded picture, as descriptors a compositor can import.
///
/// The frame owns the surface and the descriptors point into it, so both are kept here: a
/// picture is valid for exactly as long as it is held.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// A `DRM_FORMAT_*` fourcc — `NV12` for everything here.
    pub fourcc: u32,
    pub modifier: u64,
    /// One per plane: the descriptor it lives in, its offset and its stride.
    pub planes: Vec<(std::os::fd::RawFd, u32, u32)>,
    /// What the host called this frame, straight back out.
    pub timestamp: i64,
    drm_frame: *mut ffi::AVFrame,
    frame: *mut ffi::AVFrame,
}

impl Picture {
    /// The decoded surface, for whatever comes next — the conversion, usually.
    pub(crate) fn frame(&self) -> *mut ffi::AVFrame {
        self.frame
    }

    /// Copy the picture into memory, as NV12.
    ///
    /// Slow and deliberately so: this exists for tests and for writing a file to look at. The
    /// real path never calls it, because the whole point is that these pixels stay on the GPU.
    pub fn to_nv12(&self) -> Result<Vec<u8>> {
        unsafe {
            let software = ffi::av_frame_alloc();
            (*software).format = ffi::AV_PIX_FMT_NV12;
            let r = ffi::av_hwframe_transfer_data(software, self.frame, 0);
            if r < 0 {
                ffi::av_frame_free(&mut { software });
                return Err(fail("could not read a picture back", r));
            }
            let mut out =
                Vec::with_capacity((self.width * self.height + self.width * self.height / 2) as usize);
            for (plane, rows) in [(0usize, self.height), (1, self.height / 2)] {
                let stride = (*software).linesize[plane] as usize;
                let data = (*software).data[plane];
                for row in 0..rows as usize {
                    let start = data.add(row * stride);
                    out.extend_from_slice(std::slice::from_raw_parts(
                        start,
                        self.width as usize,
                    ));
                }
            }
            ffi::av_frame_free(&mut { software });
            Ok(out)
        }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        unsafe {
            ffi::av_frame_free(&mut self.drm_frame);
            ffi::av_frame_free(&mut self.frame);
        }
    }
}

/// Decodes one window's stream.
pub struct Decoder {
    ctx: *mut ffi::AVCodecContext,
    device: *mut ffi::AVBufferRef,
    packet: *mut ffi::AVPacket,
    size: Option<(u32, u32)>,
}

// SAFETY: everything here is owned by this struct and freed in `Drop`, and a decoder belongs to
// exactly one thread — one per window, which is the design.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Open a hardware decoder for `codec` on `node`.
    pub fn new(node: &str, codec: Codec) -> Result<Decoder> {
        unsafe {
            let mut device = ptr::null_mut();
            let path = CString::new(node).unwrap();
            let r = ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AV_HWDEVICE_TYPE_VAAPI,
                path.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                return Err(fail(&format!("could not open {node} for VA-API"), r));
            }

            let id = match codec {
                Codec::H264 => ffi::AV_CODEC_ID_H264,
                Codec::H265 => ffi::AV_CODEC_ID_HEVC,
                Codec::Av1 => ffi::AV_CODEC_ID_AV1,
            };
            let decoder = ffi::avcodec_find_decoder(id);
            if decoder.is_null() {
                return Err(VideoError(format!(
                    "this libavcodec cannot decode {}",
                    codec.label()
                )));
            }
            let ctx = ffi::avcodec_alloc_context3(decoder);
            if ctx.is_null() {
                return Err(VideoError("out of memory for a decoder".into()));
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device);
            // Pictures stay on the GPU. Left to itself libavcodec picks a software format
            // whenever one is offered, and everything still *works* — which is the trap: the
            // picture is correct and is being copied out of the GPU and back in every frame.
            (*ctx).get_format = Some(pick_vaapi);
            // Room for pictures held outside the decoder: the newest one waiting for the
            // compositor, and the one being converted. Without it a held picture is a surface
            // the decoder needs for its references, and it stalls or overwrites.
            (*ctx).extra_hw_frames = 4;
            // One window's stream is decoded in order; threads here would buy nothing and cost
            // a frame or two of buffering.
            (*ctx).thread_count = 1;

            let r = ffi::avcodec_open2(ctx, decoder, ptr::null_mut());
            if r < 0 {
                return Err(fail("could not open the decoder", r));
            }
            log::info!("{} decoder ready on {node}", codec.label());
            Ok(Decoder {
                ctx,
                device,
                packet: ffi::av_packet_alloc(),
                size: None,
            })
        }
    }

    /// Feed one whole frame and take whatever pictures come out.
    ///
    /// Usually one. A caller more than a frame behind should show the last and drop the rest:
    /// an old picture is worth less than the wait to show it.
    pub fn decode(&mut self, timestamp: i64, frame: &[u8]) -> Result<Vec<Picture>> {
        unsafe {
            (*self.packet).data = frame.as_ptr() as *mut u8;
            (*self.packet).size = frame.len() as i32;
            (*self.packet).pts = timestamp;
            let r = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if r < 0 {
                return Err(fail("the decoder would not take a frame", r));
            }

            let mut out = Vec::new();
            loop {
                let frame = ffi::av_frame_alloc();
                let r = ffi::avcodec_receive_frame(self.ctx, frame);
                if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                    ffi::av_frame_free(&mut { frame });
                    break;
                }
                if r < 0 {
                    ffi::av_frame_free(&mut { frame });
                    return Err(fail("the decoder produced an error instead of a picture", r));
                }
                let width = (*frame).width as u32;
                let height = (*frame).height as u32;
                if self.size != Some((width, height)) {
                    log::info!("stream is {width}x{height}");
                    self.size = Some((width, height));
                }
                match self.lend(frame, width, height) {
                    Ok(picture) => out.push(picture),
                    Err(e) => {
                        ffi::av_frame_free(&mut { frame });
                        return Err(e);
                    }
                }
            }
            Ok(out)
        }
    }

    /// Map a decoded surface out as a dmabuf, without copying it.
    unsafe fn lend(
        &mut self,
        frame: *mut ffi::AVFrame,
        width: u32,
        height: u32,
    ) -> Result<Picture> {
        let drm_frame = ffi::av_frame_alloc();
        (*drm_frame).format = ffi::AV_PIX_FMT_DRM_PRIME;
        let r = ffi::av_hwframe_map(
            drm_frame,
            frame,
            (ffi::AV_HWFRAME_MAP_DIRECT | ffi::AV_HWFRAME_MAP_READ) as i32,
        );
        if r < 0 {
            ffi::av_frame_free(&mut { drm_frame });
            return Err(fail("could not lend a decoded picture to the compositor", r));
        }
        let descriptor = &*((*drm_frame).data[0] as *const drm::FrameDescriptor);
        if descriptor.nb_layers < 1 {
            ffi::av_frame_free(&mut { drm_frame });
            return Err(VideoError("a decoded picture has no layers".into()));
        }
        let layer = &descriptor.layers[0];
        let mut planes = Vec::new();
        for i in 0..layer.nb_planes as usize {
            let plane = &layer.planes[i];
            let object = &descriptor.objects[plane.object_index as usize];
            planes.push((object.fd, plane.offset as u32, plane.pitch as u32));
        }
        Ok(Picture {
            width,
            height,
            fourcc: layer.format,
            modifier: descriptor.objects[0].format_modifier,
            planes,
            timestamp: (*frame).pts,
            drm_frame,
            frame,
        })
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// The device and frame pool the decoder is using, for a conversion that has to share them.
    pub fn device(&self) -> (*mut ffi::AVBufferRef, *mut ffi::AVBufferRef) {
        unsafe { (self.device, (*self.ctx).hw_frames_ctx) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// Insist on hardware surfaces.
unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let mut format = formats;
    while *format != ffi::AV_PIX_FMT_NONE {
        if *format == ffi::AV_PIX_FMT_VAAPI {
            return ffi::AV_PIX_FMT_VAAPI;
        }
        format = format.add(1);
    }
    log::error!("this stream cannot be decoded on the GPU here");
    ffi::AV_PIX_FMT_NONE
}
