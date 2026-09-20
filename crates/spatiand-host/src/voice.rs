//! The wearer's voice, turned back into samples.
//!
//! The other half of `spatiand_video::voice`, which is on the Deck and does the encoding. It
//! is written again here rather than shared because the two ends build against different
//! ffmpeg versions — the Deck has 7.1's libraries, this machine has 8's — and that was always
//! allowed: they agree on a codec, not on a library. The shape is small enough that saying it
//! twice is cheaper than making one crate build two ways.
//!
//! ffmpeg's own Opus decoder is built in everywhere and needs no library at all, so this asks
//! for `libopus` only if the built-in one is missing, which is the opposite way round from the
//! encoder.

use std::ffi::{CStr, CString};

use rsmpeg::ffi;

pub struct Decoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    packet: *mut ffi::AVPacket,
}

// Owned here and touched from nowhere else.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(rate: u32, channels: u16) -> Result<Decoder, String> {
        unsafe {
            let mut codec = ffi::avcodec_find_decoder(ffi::AV_CODEC_ID_OPUS);
            if codec.is_null() {
                let name = CString::new("libopus").expect("a literal");
                codec = ffi::avcodec_find_decoder_by_name(name.as_ptr());
            }
            if codec.is_null() {
                return Err("this ffmpeg has no Opus decoder".into());
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err("no room for a decoder".into());
            }
            let mut decoder = Decoder {
                ctx,
                frame: std::ptr::null_mut(),
                packet: std::ptr::null_mut(),
            };
            (*ctx).sample_rate = rate as i32;
            (*ctx).request_sample_fmt = ffi::AV_SAMPLE_FMT_S16;
            ffi::av_channel_layout_default(&raw mut (*ctx).ch_layout, i32::from(channels));
            let opened = ffi::avcodec_open2(ctx, codec, std::ptr::null_mut());
            if opened < 0 {
                return Err(format!("the Opus decoder would not open: {}", reason(opened)));
            }
            decoder.frame = ffi::av_frame_alloc();
            decoder.packet = ffi::av_packet_alloc();
            if decoder.frame.is_null() || decoder.packet.is_null() {
                return Err("no room for a frame".into());
            }
            Ok(decoder)
        }
    }

    /// One packet in, the samples it held out, signed 16-bit and interleaved.
    ///
    /// A packet that will not decode is a click, not a reason to stop: it is reported and the
    /// next one is tried.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            (*self.packet).data = packet.as_ptr().cast_mut();
            (*self.packet).size = packet.len() as i32;
            let sent = ffi::avcodec_send_packet(self.ctx, self.packet);
            if sent < 0 {
                return Err(format!("the decoder refused a packet: {}", reason(sent)));
            }
            let mut out = Vec::new();
            loop {
                let got = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if got == -(ffi::EAGAIN as i32) || got == ffi::AVERROR_EOF {
                    break;
                }
                if got < 0 {
                    return Err(format!("the decoder gave nothing back: {}", reason(got)));
                }
                out.extend_from_slice(&self.samples());
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// What the frame holds, as bytes a sound server takes.
    ///
    /// Opus decodes to planar floats unless the build says otherwise, and asking for signed
    /// 16-bit is a request rather than a promise, so both are handled here instead of assuming
    /// the one that happened to come out on this machine.
    unsafe fn samples(&self) -> Vec<u8> {
        let frame = self.frame;
        let count = (*frame).nb_samples as usize;
        let channels = (*frame).ch_layout.nb_channels as usize;
        let mut out = Vec::with_capacity(count * channels * 2);
        match (*frame).format {
            f if f == ffi::AV_SAMPLE_FMT_S16 => {
                let from = (*frame).data[0].cast::<i16>();
                for i in 0..count * channels {
                    out.extend_from_slice(&(*from.add(i)).to_le_bytes());
                }
            }
            f if f == ffi::AV_SAMPLE_FMT_FLTP => {
                for i in 0..count {
                    for c in 0..channels {
                        let plane = (*frame).data[c].cast::<f32>();
                        out.extend_from_slice(&clip(*plane.add(i)).to_le_bytes());
                    }
                }
            }
            f if f == ffi::AV_SAMPLE_FMT_FLT => {
                let from = (*frame).data[0].cast::<f32>();
                for i in 0..count * channels {
                    out.extend_from_slice(&clip(*from.add(i)).to_le_bytes());
                }
            }
            _ => {}
        }
        out
    }
}

/// A float sample as a whole one, without wrapping a loud noise round into a louder one.
fn clip(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.packet.is_null() {
                // The data belongs to the caller; make sure nothing tries to free it.
                (*self.packet).data = std::ptr::null_mut();
                (*self.packet).size = 0;
                ffi::av_packet_free(&raw mut self.packet);
            }
            if !self.frame.is_null() {
                ffi::av_frame_free(&raw mut self.frame);
            }
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&raw mut self.ctx);
            }
        }
    }
}

fn reason(code: i32) -> String {
    let mut buffer = [0i8; 256];
    unsafe {
        if ffi::av_strerror(code, buffer.as_mut_ptr().cast(), buffer.len()) < 0 {
            return format!("error {code}");
        }
        CStr::from_ptr(buffer.as_ptr().cast())
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loud_sample_is_clipped_rather_than_wrapped() {
        // The bug this guards is the one that turns a shout into a burst of noise: a float
        // above one, multiplied and cast, comes out as a large negative number.
        assert_eq!(clip(2.0), i16::MAX);
        assert_eq!(clip(-2.0), -i16::MAX);
        assert_eq!(clip(0.0), 0);
    }

    #[test]
    fn this_ffmpeg_can_decode_opus() {
        // Cheap, and it settles the question this module exists to answer on whatever machine
        // the host is actually running on.
        assert!(Decoder::new(48_000, 1).is_ok());
    }
}
