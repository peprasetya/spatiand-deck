//! Turning a window's buffer into encoded pictures, without ever copying it to the CPU.
//!
//! The path, and why it has these steps and no others:
//!
//! ```text
//!   client's surfaces            (whatever the application drew, plus its subsurfaces)
//!        │  GL, on the host's own renderer
//!        ▼
//!   one BGRA buffer              (a GBM buffer object we allocated, so we know its format)
//!        │  exported as dmabuf, imported by VA-API — no copy, no CPU
//!        ▼
//!   a VA-API surface
//!        │  the GPU's video postprocessor: BGRA → NV12
//!        ▼
//!   NV12                         (what every video encoder actually takes)
//!        │  VA-API encode
//!        ▼
//!   HEVC / H.264 / AV1
//! ```
//!
//! **The buffer belongs to the encoder, and the compositor draws into it.** The obvious
//! direction — allocate a buffer, composite, hand it to the encoder — does not work: libavutil
//! will not create a pool of dmabuf-backed frames at all (`av_hwframe_ctx_init` on a DRM device
//! answers "function not implemented"), because a dmabuf is something it expects to *receive*.
//! Turned around, everything is supported: VA-API allocates the surface, exports it as a dmabuf,
//! and GL draws into that. One allocation, no import per frame, and nothing crosses the CPU
//! either way.
//!
//! **Why composite first rather than encode the client's own buffer.** A window is not one
//! buffer. It is a surface with subsurfaces — a video with its controls over it, a browser with
//! its own compositing — and what the wearer should see is all of them. Drawing them into a
//! buffer of our choosing also settles the format and modifier question: a client may hand over
//! anything its driver likes, and the encoder wants one specific thing.
//!
//! **Why the postprocessor rather than a shader.** Converting to NV12 in GL means writing the
//! two planes of a planar format from a renderer that thinks in RGBA. The GPU has a fixed
//! function block that does exactly this conversion, it costs a fraction of a millisecond, and
//! it is already in the path.
//!
//! Everything here is libavcodec's VA-API encoder. The decode half, on the headset, is a
//! different library for a reason given in `docs/remote.md`.

use std::ffi::{CStr, CString};
use std::os::fd::FromRawFd;
use std::ptr;

use rsmpeg::ffi;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use spatiand_stream::Codec;

/// What went wrong, in terms that name the step rather than the return code.
#[derive(Debug)]
pub struct EncodeError(String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EncodeError {}

type Result<T> = std::result::Result<T, EncodeError>;

fn fail(what: &str, code: i32) -> EncodeError {
    let mut buf = [0i8; 256];
    let text = unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr().cast(), buf.len());
        CStr::from_ptr(buf.as_ptr().cast()).to_string_lossy().into_owned()
    };
    EncodeError(format!("{what}: {text} ({code})"))
}

/// libavutil's description of a dmabuf, declared here because the bindings do not carry it.
///
/// `hwcontext_drm.h` is a public libavutil header, but `rusty_ffmpeg` does not generate
/// bindings for it, and it is the one structure this whole file exists to fill in. The layout
/// is that header's, and it has not changed since the API appeared; `AV_DRM_MAX_PLANES` is 4.
///
/// If a future ffmpeg ever changes it, the symptom would be a mapping that fails or a picture
/// made of garbage, so it is pinned by the version we build and ship against.
mod drm {
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

/// One encoded picture.
///
/// `keyframe` and `pts` are what the packet header carries; until the network half exists,
/// the only consumer is a file.
#[allow(dead_code)]
pub struct Coded {
    pub bytes: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// A VA-API encoder for one window, at one size.
///
/// A window that is resized gets a new one: an encoder's picture size is fixed when it is
/// opened, and pretending otherwise is how a stream ends up stretched.
pub struct Encoder {
    codec_ctx: *mut ffi::AVCodecContext,
    /// The VA-API device everything shares.
    device: *mut ffi::AVBufferRef,
    /// The pool of NV12 surfaces the encoder draws its input from.
    frames: *mut ffi::AVBufferRef,
    /// The surface the compositor draws into, and which the conversion reads.
    source_frames: *mut ffi::AVBufferRef,
    canvas: *mut ffi::AVFrame,
    /// The same memory, as something GL can bind.
    canvas_dmabuf: Dmabuf,
    /// Converts whatever the client's buffer is into the encoder's NV12.
    filter: Filter,
    packet: *mut ffi::AVPacket,
    pub width: u32,
    pub height: u32,
    next_pts: i64,
}

// SAFETY: every pointer here is owned by this struct and freed in `Drop`; libavcodec's contexts
// are not shared between threads. One encoder lives on one thread, which is the design.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Open an encoder on `node`, for pictures of exactly this size.
    pub fn new(
        node: &str,
        codec: Codec,
        width: u32,
        height: u32,
        bitrate_kbit: u32,
        fps: u32,
    ) -> Result<Encoder> {
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

            // What the encoder reads: NV12, its own pool.
            let frames = match hw_frames_of(device, ffi::AV_PIX_FMT_NV12, width, height, 8) {
                Ok(frames) => frames,
                Err(e) => {
                    ffi::av_buffer_unref(&mut device);
                    return Err(e);
                }
            };

            // What the compositor draws into: one BGRA surface, allocated here and exported as
            // a dmabuf. Two of them would let a frame be drawn while the last is encoding; one
            // is enough while the encode costs a millisecond and a half.
            let source_frames = hw_frames_of(device, ffi::AV_PIX_FMT_BGRA, width, height, 2)?;
            let canvas = ffi::av_frame_alloc();
            let r = ffi::av_hwframe_get_buffer(source_frames, canvas, 0);
            if r < 0 {
                return Err(fail("could not allocate a surface to draw into", r));
            }
            let canvas_dmabuf = export_dmabuf(canvas, width, height)?;

            let name = CString::new(encoder_name(codec)).unwrap();
            let encoder = ffi::avcodec_find_encoder_by_name(name.as_ptr());
            if encoder.is_null() {
                return Err(EncodeError(format!(
                    "this libavcodec has no {} encoder",
                    encoder_name(codec)
                )));
            }
            let codec_ctx = ffi::avcodec_alloc_context3(encoder);
            if codec_ctx.is_null() {
                return Err(EncodeError("out of memory for an encoder".into()));
            }
            (*codec_ctx).width = width as i32;
            (*codec_ctx).height = height as i32;
            (*codec_ctx).pix_fmt = ffi::AV_PIX_FMT_VAAPI;
            // Milliseconds. The wire carries capture times of its own, so this is only what
            // the bitstream needs to be well formed.
            (*codec_ctx).time_base = ffi::AVRational { num: 1, den: 1000 };
            // **The rate controller divides the bitrate by this.** Left unset, it falls back to
            // the time base — a thousand frames a second — and every frame is given a
            // thousandth of the ceiling. The symptom is not an error: it is a stream that sits
            // at a twentieth of the bitrate it was told to use and looks like a bad video call,
            // which is exactly what the first run over the network produced.
            //
            // It is a budgeting figure, not a promise: frames are sent when a window draws
            // them, which is usually fewer than this.
            (*codec_ctx).framerate = ffi::AVRational {
                num: fps.max(1) as i32,
                den: 1,
            };
            (*codec_ctx).bit_rate = bitrate_kbit as i64 * 1000;
            (*codec_ctx).rc_max_rate = bitrate_kbit as i64 * 1000;
            // No B-frames, ever. A B-frame refers forwards, so it cannot be sent until the
            // frame after it exists — a whole frame of latency bought for a few percent of
            // bitrate, which is the wrong trade for something a head is attached to.
            (*codec_ctx).max_b_frames = 0;
            // Nothing after the picture it belongs to.
            (*codec_ctx).delay = 0;
            (*codec_ctx).gop_size = i32::MAX;
            (*codec_ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*codec_ctx).hw_frames_ctx = ffi::av_buffer_ref(frames);

            let mut options: *mut ffi::AVDictionary = ptr::null_mut();
            // Long keyframe interval on purpose: a keyframe is many times the size of an
            // ordinary one, and this stream gets them when something asks — a reconnection, a
            // loss, a window coming into view — rather than on a timer nobody set.
            set(&mut options, "idr_interval", "0");
            // **One picture in, one picture out.** VA-API's encoders keep two frames in flight
            // by default and hand a frame's packet out only when the next one is submitted.
            // Invisible on anything moving; on a terminal it was every keystroke showing one
            // keystroke late — "l" appeared when "s" was typed — because nothing else changed
            // to push it out. Unknown to FFmpeg before 5.0, where it is ignored.
            set(&mut options, "async_depth", "1");
            let r = ffi::avcodec_open2(codec_ctx, encoder, &mut options);
            ffi::av_dict_free(&mut options);
            if r < 0 {
                return Err(fail("could not open the VA-API encoder", r));
            }

            let filter = Filter::new(device, source_frames, width, height)?;
            let packet = ffi::av_packet_alloc();

            log::info!(
                "{} encoder ready at {width}x{height}, ceiling {bitrate_kbit} kbit/s at {fps} fps",
                codec.label()
            );
            Ok(Encoder {
                codec_ctx,
                device,
                frames,
                source_frames,
                canvas,
                canvas_dmabuf,
                filter,
                packet,
                width,
                height,
                next_pts: 0,
            })
        }
    }

    /// Where to draw the next picture. Bind it, draw, then call [`Encoder::encode`].
    pub fn canvas(&mut self) -> &mut Dmabuf {
        &mut self.canvas_dmabuf
    }

    /// Encode whatever is currently in the canvas.
    ///
    /// `pts_ms` is the host's own clock. `force_key` is how a keyframe is asked for: there is
    /// no periodic one.
    pub fn encode(&mut self, pts_ms: i64, force_key: bool) -> Result<Vec<Coded>> {
        unsafe {
            let converted = self.filter.run(self.canvas)?;
            (*converted).pts = pts_ms.max(self.next_pts);
            self.next_pts = (*converted).pts + 1;
            if force_key {
                (*converted).pict_type = ffi::AV_PICTURE_TYPE_I;
            } else {
                (*converted).pict_type = ffi::AV_PICTURE_TYPE_NONE;
            }
            let r = ffi::avcodec_send_frame(self.codec_ctx, converted);
            ffi::av_frame_free(&mut { converted });
            if r < 0 {
                return Err(fail("the encoder would not take a frame", r));
            }
            let mut out = Vec::new();
            loop {
                let r = ffi::avcodec_receive_packet(self.codec_ctx, self.packet);
                if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    return Err(fail("the encoder produced an error instead of a packet", r));
                }
                let packet = &*self.packet;
                let bytes =
                    std::slice::from_raw_parts(packet.data, packet.size as usize).to_vec();
                out.push(Coded {
                    keyframe: packet.flags & ffi::AV_PKT_FLAG_KEY as i32 != 0,
                    pts: packet.pts,
                    bytes,
                });
                ffi::av_packet_unref(self.packet);
            }
            Ok(out)
        }
    }

}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::avcodec_free_context(&mut self.codec_ctx);
            ffi::av_frame_free(&mut self.canvas);
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.source_frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// A pool of VA-API surfaces holding `sw_format` pictures, `pool` of them.
unsafe fn hw_frames_of(
    device: *mut ffi::AVBufferRef,
    sw_format: ffi::AVPixelFormat,
    width: u32,
    height: u32,
    pool: i32,
) -> Result<*mut ffi::AVBufferRef> {
    let frames = ffi::av_hwframe_ctx_alloc(device);
    if frames.is_null() {
        return Err(EncodeError("out of memory for a frame pool".into()));
    }
    let ctx = (*frames).data as *mut ffi::AVHWFramesContext;
    (*ctx).format = ffi::AV_PIX_FMT_VAAPI;
    (*ctx).sw_format = sw_format;
    (*ctx).width = width as i32;
    (*ctx).height = height as i32;
    (*ctx).initial_pool_size = pool;
    let r = ffi::av_hwframe_ctx_init(frames);
    if r < 0 {
        return Err(fail("could not describe a pool of surfaces", r));
    }
    Ok(frames)
}

/// The GPU's postprocessor, as a one-step filter graph: whatever came in, NV12 out.
struct Filter {
    graph: *mut ffi::AVFilterGraph,
    source: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
}

impl Filter {
    unsafe fn new(
        device: *mut ffi::AVBufferRef,
        source_frames: *mut ffi::AVBufferRef,
        width: u32,
        height: u32,
    ) -> Result<Filter> {
        let graph = ffi::avfilter_graph_alloc();
        if graph.is_null() {
            return Err(EncodeError("out of memory for a filter graph".into()));
        }

        // The input is hardware frames, and a source filter given only text arguments cannot
        // describe those: what a VA-API frame *is* lives in its frame context. So the filter is
        // allocated, told its shape through the parameters structure — which is the only way to
        // hand over a frames context — and only then initialised.
        let name = CString::new("in").unwrap();
        let buffer = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        let source = ffi::avfilter_graph_alloc_filter(graph, buffer, name.as_ptr());
        if source.is_null() {
            return Err(EncodeError("could not create the filter graph's input".into()));
        }
        let params = ffi::av_buffersrc_parameters_alloc();
        (*params).format = ffi::AV_PIX_FMT_VAAPI;
        (*params).width = width as i32;
        (*params).height = height as i32;
        (*params).time_base = ffi::AVRational { num: 1, den: 1000 };
        (*params).hw_frames_ctx = source_frames;
        let r = ffi::av_buffersrc_parameters_set(source, params);
        ffi::av_free(params.cast());
        if r < 0 {
            return Err(fail("the filter graph would not take its input's shape", r));
        }
        let r = ffi::avfilter_init_str(source, ptr::null());
        if r < 0 {
            return Err(fail("could not start the filter graph's input", r));
        }

        let sink_name = CString::new("out").unwrap();
        let buffersink = ffi::avfilter_get_by_name(c"buffersink".as_ptr());
        let sink = ffi::avfilter_graph_alloc_filter(graph, buffersink, sink_name.as_ptr());
        if sink.is_null() {
            return Err(EncodeError("could not create the filter graph's output".into()));
        }
        let r = ffi::avfilter_init_str(sink, ptr::null());
        if r < 0 {
            return Err(fail("could not start the filter graph's output", r));
        }

        let scale_name = CString::new("nv12").unwrap();
        let scale_args = CString::new("format=nv12").unwrap();
        let scale_filter = ffi::avfilter_get_by_name(c"scale_vaapi".as_ptr());
        if scale_filter.is_null() {
            return Err(EncodeError(
                "this libavcodec has no scale_vaapi filter, so nothing can convert to NV12"
                    .into(),
            ));
        }
        let scale = ffi::avfilter_graph_alloc_filter(graph, scale_filter, scale_name.as_ptr());
        if scale.is_null() {
            return Err(EncodeError("could not create the colour conversion".into()));
        }
        // The conversion runs on the GPU, so it needs the device too. Set before init, because
        // that is when the filter decides what it can do.
        (*scale).hw_device_ctx = ffi::av_buffer_ref(device);
        let r = ffi::avfilter_init_str(scale, scale_args.as_ptr());
        if r < 0 {
            return Err(fail("could not start the colour conversion", r));
        }

        let r = ffi::avfilter_link(source, 0, scale, 0);
        if r < 0 {
            return Err(fail("could not link the input to the conversion", r));
        }
        let r = ffi::avfilter_link(scale, 0, sink, 0);
        if r < 0 {
            return Err(fail("could not link the conversion to the output", r));
        }
        let r = ffi::avfilter_graph_config(graph, ptr::null_mut());
        if r < 0 {
            return Err(fail("the filter graph would not configure", r));
        }
        Ok(Filter { graph, source, sink })
    }

    /// Push one frame through, and take ownership of what comes out.
    unsafe fn run(&mut self, frame: *mut ffi::AVFrame) -> Result<*mut ffi::AVFrame> {
        // `KEEP_REF` because the canvas belongs to the encoder and is drawn into again next
        // frame; the graph takes a reference rather than the frame itself.
        let r = ffi::av_buffersrc_add_frame_flags(
            self.source,
            frame,
            ffi::AV_BUFFERSRC_FLAG_KEEP_REF as i32,
        );
        if r < 0 {
            return Err(fail("the conversion would not take a frame", r));
        }
        let out = ffi::av_frame_alloc();
        let r = ffi::av_buffersink_get_frame(self.sink, out);
        if r < 0 {
            ffi::av_frame_free(&mut { out });
            return Err(fail("the conversion produced nothing", r));
        }
        Ok(out)
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        unsafe { ffi::avfilter_graph_free(&mut self.graph) }
    }
}

/// Lend a VA-API surface to GL, as a dmabuf.
///
/// The descriptors are duplicated because both sides outlive each other's opinions: libavutil
/// frees its mapping when the frame goes, and smithay's `Dmabuf` closes what it was given.
unsafe fn export_dmabuf(frame: *mut ffi::AVFrame, width: u32, height: u32) -> Result<Dmabuf> {
    let drm_frame = ffi::av_frame_alloc();
    (*drm_frame).format = ffi::AV_PIX_FMT_DRM_PRIME;
    let r = ffi::av_hwframe_map(
        drm_frame,
        frame,
        (ffi::AV_HWFRAME_MAP_DIRECT | ffi::AV_HWFRAME_MAP_WRITE | ffi::AV_HWFRAME_MAP_READ) as i32,
    );
    if r < 0 {
        ffi::av_frame_free(&mut { drm_frame });
        return Err(fail("could not lend the encoder's surface to GL", r));
    }
    let descriptor = &*((*drm_frame).data[0] as *const drm::FrameDescriptor);
    if descriptor.nb_layers < 1 {
        ffi::av_frame_free(&mut { drm_frame });
        return Err(EncodeError("the encoder's surface has no layers".into()));
    }
    let layer = &descriptor.layers[0];
    let format = Fourcc::try_from(layer.format)
        .map_err(|_| EncodeError(format!("unknown buffer format {:#x}", layer.format)))?;
    let mut builder = Dmabuf::builder(
        (width as i32, height as i32),
        format,
        Modifier::from(descriptor.objects[0].format_modifier),
        DmabufFlags::empty(),
    );
    for i in 0..layer.nb_planes as usize {
        let plane = &layer.planes[i];
        let object = &descriptor.objects[plane.object_index as usize];
        let fd = libc::dup(object.fd);
        if fd < 0 {
            ffi::av_frame_free(&mut { drm_frame });
            return Err(EncodeError("could not duplicate a buffer descriptor".into()));
        }
        builder.add_plane(
            std::os::fd::OwnedFd::from_raw_fd(fd),
            i as u32,
            plane.offset as u32,
            plane.pitch as u32,
        );
    }
    let dmabuf = builder
        .build()
        .ok_or_else(|| EncodeError("the encoder's surface has no planes".into()))?;
    ffi::av_frame_free(&mut { drm_frame });
    Ok(dmabuf)
}

fn encoder_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264_vaapi",
        Codec::H265 => "hevc_vaapi",
        Codec::Av1 => "av1_vaapi",
    }
}

unsafe fn set(options: *mut *mut ffi::AVDictionary, key: &str, value: &str) {
    let key = CString::new(key).unwrap();
    let value = CString::new(value).unwrap();
    ffi::av_dict_set(options, key.as_ptr(), value.as_ptr(), 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_codec_names_its_vaapi_encoder() {
        assert_eq!(encoder_name(Codec::H265), "hevc_vaapi");
        assert_eq!(encoder_name(Codec::H264), "h264_vaapi");
        assert_eq!(encoder_name(Codec::Av1), "av1_vaapi");
    }
}
