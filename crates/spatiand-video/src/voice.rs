//! The wearer's voice, made small enough to send.
//!
//! Mono at 48 kHz is 768 kbit/s of samples, which is most of a megabit spent on one person
//! talking — on the uplink, which is the direction with the least to spare. Opus carries the
//! same voice in about twenty-four, and it is the codec every real-time system reaches for
//! because it is the one unambiguously good answer for speech at low delay.
//!
//! **Nothing is installed for it.** The ffmpeg this already links carries Opus through
//! `libopus`, on both machines and at both versions — the Deck's `libavcodec` links
//! `libopus.so.0` and the host's the same. The two ends never had to agree on a library, only
//! on a codec, and this is the second thing they agree on after HEVC.
//!
//! **Twenty milliseconds a frame, voice mode.** The delay a codec adds is one frame, and
//! twenty milliseconds of it is nothing beside the hundreds a queue adds when a link cannot
//! keep up with raw samples. `application=voip` tells Opus that what it is carrying is a
//! person rather than music, which is where its bit rate comes from.
//!
//! Raw ffi rather than a wrapper, as everywhere else here: the two ends build against
//! different ffmpeg versions, and this is the part of the API that has not changed in a
//! decade.

use std::ffi::{CStr, CString};

use rsmpeg::ffi;

/// What a voice costs. Generous for speech; Opus spends less when there is less to say.
const BITS_PER_SECOND: i64 = 24_000;

/// Milliseconds of sound in one packet. The delay this codec adds, and no more.
const FRAME_MS: &str = "20";

pub struct Encoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    packet: *mut ffi::AVPacket,
    /// Samples that have arrived but do not yet make a whole frame.
    pending: Vec<i16>,
    /// How many samples the encoder insists on at a time.
    per_frame: usize,
    /// Sample count so far, which is what Opus wants for a timestamp.
    pts: i64,
}

// The pointers are owned by this and touched from nowhere else; the encoder itself is used
// from the one thread that reads the microphone.
unsafe impl Send for Encoder {}

impl Encoder {
    /// An encoder for `channels` channels at `rate`, or why there is none.
    pub fn new(rate: u32, channels: u16) -> Result<Encoder, String> {
        unsafe {
            let name = CString::new("libopus").expect("a literal");
            let mut codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                // Some builds carry ffmpeg's own Opus encoder instead of the library's.
                codec = ffi::avcodec_find_encoder(ffi::AV_CODEC_ID_OPUS);
            }
            if codec.is_null() {
                return Err("this ffmpeg has no Opus encoder".into());
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err("no room for an encoder".into());
            }
            let mut encoder = Encoder {
                ctx,
                frame: std::ptr::null_mut(),
                packet: std::ptr::null_mut(),
                pending: Vec::new(),
                per_frame: 0,
                pts: 0,
            };
            (*ctx).sample_rate = rate as i32;
            (*ctx).sample_fmt = ffi::AV_SAMPLE_FMT_S16;
            (*ctx).bit_rate = BITS_PER_SECOND;
            ffi::av_channel_layout_default(&raw mut (*ctx).ch_layout, i32::from(channels));

            let mut options: *mut ffi::AVDictionary = std::ptr::null_mut();
            let set = |options: &mut *mut ffi::AVDictionary, key: &str, value: &str| {
                let key = CString::new(key).expect("a literal");
                let value = CString::new(value).expect("a literal");
                ffi::av_dict_set(options, key.as_ptr(), value.as_ptr(), 0);
            };
            set(&mut options, "application", "voip");
            set(&mut options, "frame_duration", FRAME_MS);
            // Tell Opus to expect a little loss, so it protects itself rather than relying on
            // a retransmission that would arrive after the moment for it had passed.
            set(&mut options, "packet_loss", "5");
            let opened = ffi::avcodec_open2(ctx, codec, &raw mut options);
            ffi::av_dict_free(&raw mut options);
            if opened < 0 {
                return Err(format!("the Opus encoder would not open: {}", reason(opened)));
            }

            encoder.per_frame = match (*ctx).frame_size {
                // An encoder that takes any size still has to be given one.
                0 => (rate as usize / 1000) * 20,
                size => size as usize,
            };
            encoder.frame = ffi::av_frame_alloc();
            encoder.packet = ffi::av_packet_alloc();
            if encoder.frame.is_null() || encoder.packet.is_null() {
                return Err("no room for a frame".into());
            }
            (*encoder.frame).format = ffi::AV_SAMPLE_FMT_S16;
            (*encoder.frame).nb_samples = encoder.per_frame as i32;
            ffi::av_channel_layout_copy(
                &raw mut (*encoder.frame).ch_layout,
                &raw const (*ctx).ch_layout,
            );
            let got = ffi::av_frame_get_buffer(encoder.frame, 0);
            if got < 0 {
                return Err(format!("no room for samples: {}", reason(got)));
            }
            Ok(encoder)
        }
    }

    /// Samples in, whole packets out. What does not fill a frame waits for the next call.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, String> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        let channels = unsafe { (*self.ctx).ch_layout.nb_channels as usize };
        let per_call = self.per_frame * channels;
        while self.pending.len() >= per_call {
            unsafe {
                if ffi::av_frame_make_writable(self.frame) < 0 {
                    return Err("the frame could not be written to".into());
                }
                let into = (*self.frame).data[0].cast::<i16>();
                std::ptr::copy_nonoverlapping(self.pending.as_ptr(), into, per_call);
                (*self.frame).pts = self.pts;
                self.pts += self.per_frame as i64;
                let sent = ffi::avcodec_send_frame(self.ctx, self.frame);
                if sent < 0 {
                    return Err(format!("the encoder refused a frame: {}", reason(sent)));
                }
                loop {
                    let got = ffi::avcodec_receive_packet(self.ctx, self.packet);
                    if got == -(ffi::EAGAIN as i32) || got == ffi::AVERROR_EOF {
                        break;
                    }
                    if got < 0 {
                        return Err(format!("the encoder gave nothing back: {}", reason(got)));
                    }
                    let data = (*self.packet).data;
                    let size = (*self.packet).size as usize;
                    out.push(std::slice::from_raw_parts(data, size).to_vec());
                    ffi::av_packet_unref(self.packet);
                }
            }
            self.pending.drain(..per_call);
        }
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.packet.is_null() {
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

/// What ffmpeg calls a numbered failure.
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
    fn this_ffmpeg_can_encode_opus() {
        // The question this module exists to answer, asked on the machine that has to do it.
        // The Deck's libavcodec links libopus; if that ever stops being true, this says so
        // here rather than as silence in a viewer.
        assert!(Encoder::new(48_000, 1).is_ok());
    }

    #[test]
    fn a_frames_worth_of_speech_comes_out_smaller_than_it_went_in() {
        let mut encoder = Encoder::new(48_000, 1).expect("an encoder");
        // A second of a tone, which is more structure than speech and so harder to compress.
        let samples: Vec<i16> = (0..48_000)
            .map(|i| ((f64::from(i) * 0.05).sin() * 8000.0) as i16)
            .collect();
        let packets = encoder.encode(&samples).expect("it encodes");
        assert!(!packets.is_empty(), "a whole second must make packets");
        let bytes: usize = packets.iter().map(Vec::len).sum();
        assert!(
            bytes * 8 < 96_000,
            "a second came to {} bits, which is no better than the samples were",
            bytes * 8
        );
    }

    #[test]
    fn a_part_of_a_frame_waits_for_the_rest_of_it() {
        let mut encoder = Encoder::new(48_000, 1).expect("an encoder");
        // Five milliseconds, well short of the twenty a packet holds.
        assert_eq!(encoder.encode(&vec![0i16; 240]).expect("it encodes").len(), 0);
    }
}
