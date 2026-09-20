//! Cutting a stream of bytes into the frames a decoder expects.
//!
//! The link never needs this: a frame arrives as a frame, because the host sends what its
//! encoder produced and [`spatiand_stream::video`] puts the pieces back together. It is here
//! for the other case — a file full of frames with nothing to say where each begins, which is
//! what a capture written for inspection is.
//!
//! Handing a decoder the whole file instead produces one picture and a complaint about "two
//! slices reporting being the first in the same frame", which is worth recognising: it means
//! the boundaries are missing, not that the stream is broken.

use std::ptr;

use rsmpeg::ffi;
use spatiand_stream::Codec;

/// Splits a byte stream into access units, using libavcodec's own parser.
pub struct Units {
    parser: *mut ffi::AVCodecParserContext,
    ctx: *mut ffi::AVCodecContext,
}

impl Units {
    pub fn new(codec: Codec) -> Result<Units, String> {
        unsafe {
            let id = match codec {
                Codec::H264 => ffi::AV_CODEC_ID_H264,
                Codec::H265 => ffi::AV_CODEC_ID_HEVC,
                Codec::Av1 => ffi::AV_CODEC_ID_AV1,
            };
            let parser = ffi::av_parser_init(id as i32);
            if parser.is_null() {
                return Err(format!("no parser for {}", codec.label()));
            }
            // The parser needs a context to keep its own state in; it is never opened, and
            // nothing is decoded through it.
            let ctx = ffi::avcodec_alloc_context3(ffi::avcodec_find_decoder(id));
            Ok(Units { parser, ctx })
        }
    }

    /// Every whole frame in `bytes`, in order.
    pub fn frames<'a>(&mut self, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut rest = bytes;
        unsafe {
            while !rest.is_empty() {
                let mut data: *mut u8 = ptr::null_mut();
                let mut size: i32 = 0;
                let used = ffi::av_parser_parse2(
                    self.parser,
                    self.ctx,
                    &mut data,
                    &mut size,
                    rest.as_ptr(),
                    rest.len() as i32,
                    ffi::AV_NOPTS_VALUE,
                    ffi::AV_NOPTS_VALUE,
                    0,
                );
                if used <= 0 {
                    break;
                }
                if size > 0 {
                    // The parser hands back a pointer into what it was given, so the slice
                    // borrows the caller's bytes rather than copying them.
                    let offset = data as usize - bytes.as_ptr() as usize;
                    out.push(&bytes[offset..offset + size as usize]);
                }
                rest = &rest[used as usize..];
            }
        }
        out
    }
}

impl Drop for Units {
    fn drop(&mut self) {
        unsafe {
            ffi::av_parser_close(self.parser);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
