//! XREAL Air family driver.
//!
//! The protocol is documented in full in `docs/xreal-air.md`; this is that document in code.
//! Three details cost real time to establish and are easy to reintroduce as bugs:
//!
//! 1. **Both length fields count themselves.** The MCU body length for a one-byte payload is
//!    `18`, not `14`; the IMU's is `4`, not `3`. A wrong length is not tolerated — the device
//!    answers with one constant rejection packet that looks exactly like being ignored.
//! 2. **The MCU interleaves async pushes with command acks.** After writing a command you
//!    must keep reading until the msgid you sent is echoed at offset 15, discarding `0x6Cxx`
//!    events on the way. Reading exactly one report gets an unrelated event and looks like
//!    failure. (Observed in the wild: `0x6C18`, which is not in the documented event list.)
//! 3. **The magnetometer is offset binary with big-endian scalers**, alone among the three
//!    sensor groups. XOR `0x8000`, then reinterpret those bits as signed.
//!
//! The MCU is spoken to from a thread of its own, which is the only thing that reads it. See
//! [`mcu`] for why.

mod mcu;

use std::time::Duration;

use glam::DVec3;

use crate::device::{self, DeviceSpec};
use crate::hid::{self, HidDevice, HidNode};
use crate::{DisplayMode, Hmd, HmdButton, HmdError, HmdEvent, HmdInfo, ImuSample, Result};

// --- MCU (interface 4) ---
const MCU_HEAD: u8 = 0xFD;
const MSG_W_DISP_MODE: u16 = 0x0008;
const MSG_R_BRIGHTNESS: u16 = 0x0003;
const MSG_W_BRIGHTNESS: u16 = 0x0004;

/// How many brightness steps the panel has, counting from zero.
///
/// Eight is what XREAL's own SDK reports (`GetBrightnessLevelNumber`, `docs/xreal-air.md`).
/// The messages themselves have been seen on the wire on an Air: a read answered step 5, a
/// write of step 2 read back as 2, and 5 again after writing it back. Whether every one of the
/// eight is distinct on the panel has not been watched step by step, which is why this is not
/// in `devices.toml` beside the display modes yet. Everything here treats a refusal as
/// ordinary: a headset that does not answer simply reports no brightness, and the sidecar
/// leaves the row out rather than drawing a control that does nothing.
const BRIGHTNESS_LEVELS: u8 = 8;
/// The dimmest step [`XrealGlasses::request_brightness`] will select.
///
/// Not zero, and that is the entire reason this exists. Step 0 turns the panel off, and the
/// control you would reach for to turn it back on is drawn *inside the glasses* — so the one
/// setting there is no way back from is the one at the bottom of the slider.
///
/// The shell has its own floor and it does not protect this. That one is a fraction —
/// `MINIMUM_BRIGHTNESS`, five percent — chosen for the Deck's own backlight, which is a
/// continuous sysfs value where five percent is genuinely five percent. Here there are eight
/// steps, so `round(0.05 * 7)` is 0 and the floor lands precisely on the thing it was written
/// to prevent. A floor expressed in fractions cannot know how coarse the device beneath it is,
/// which is why this one lives next to the number that makes it coarse.
const DIMMEST_STEP: u8 = 1;
/// Bytes the MCU length field counts before the payload: itself (2) + timestamp (8)
/// + msgid (2) + reserved (5).
const MCU_LEN_OVERHEAD: u16 = 17;
/// Offset of the msgid within an MCU packet: head (1) + crc (4) + len (2) + timestamp (8).
const MCU_MSGID_OFFSET: usize = 15;
const MCU_DATA_OFFSET: usize = 22;

// Async events the glasses push unprompted.
const EVT_DISPLAY_TOGGLED: u16 = 0x6C04;
const EVT_BUTTON_PRESSED: u16 = 0x6C05;

// --- IMU (interface 3) ---
const IMU_HEAD: u8 = 0xAA;
const MSG_START_IMU: u8 = 0x19;
/// Signature of an IMU *data* packet, as opposed to an ack or a rejection.
const IMU_DATA_SIG: [u8; 2] = [0x01, 0x02];

pub struct XrealGlasses {
    spec: &'static DeviceSpec,
    info: HmdInfo,
    imu: HidDevice,
    mcu: mcu::Mcu,
    mode: DisplayMode,
    /// Scratch buffer sized from the device spec, reused every poll so the ~1 kHz sample path
    /// does no allocation.
    buf: Vec<u8>,
    streaming: bool,
}

impl XrealGlasses {
    /// Find and open the first supported pair of glasses.
    /// Open the first supported pair of glasses.
    ///
    /// **Only ever hold one handle at a time.** [`Drop`] stops the IMU stream, so a second
    /// handle opened while the first is still alive will be silenced the moment the first goes
    /// away — and in Rust an assignment evaluates the new value before dropping the old, which
    /// makes `hmd = open_any()` exactly that pattern. The symptom is a device that looks
    /// perfectly healthy and never sends a sample.
    pub fn open_any() -> Result<Self> {
        let nodes = hid::enumerate();
        let spec = nodes
            .iter()
            .filter_map(|n| device::lookup(n.vid, n.pid))
            .find(|s| s.driver == "xreal")
            .ok_or(HmdError::NotFound)?;
        Self::open(spec, &nodes)
    }

    fn open(spec: &'static DeviceSpec, nodes: &[HidNode]) -> Result<Self> {
        if !spec.verified {
            log::warn!(
                "{} has not been tested on real hardware; its devices.toml row is a best guess",
                spec.name
            );
        }
        let pick = |iface: u8| -> Result<&HidNode> {
            nodes
                .iter()
                .find(|n| n.vid == spec.vid && n.pid == spec.pid && n.interface == iface)
                .ok_or_else(|| HmdError::InterfaceMissing {
                    device: spec.name.clone(),
                    interface: iface,
                })
        };
        let imu = HidDevice::open(pick(spec.imu_interface)?)?;
        let mcu = HidDevice::open(pick(spec.mcu_interface)?)?;
        log::info!(
            "{}: IMU {} MCU {}",
            spec.name,
            imu.path().display(),
            mcu.path().display()
        );
        let mcu = mcu::Mcu::start(mcu, mcu::TIMING)?;

        let info = HmdInfo {
            name: spec.name.clone(),
            per_eye: spec.per_eye,
            h_fov_deg: spec.h_fov_deg,
            default_ipd_mm: spec.default_ipd_mm,
            supports_stereo: true,
            // Raw gyro/accel/mag only. `spatiand-track` turns it into an orientation.
            provides_fused_pose: false,
            sensor_axes: spec.sensor_axes,
        };

        let mut this = Self {
            spec,
            info,
            imu,
            mcu,
            // We cannot read the mode back reliably at startup, so assume the desktop-safe
            // one; the first set_display_mode call makes it true either way.
            mode: DisplayMode::Mono,
            buf: vec![0u8; spec.imu_report_len.max(64)],
            streaming: false,
        };
        this.start_imu_stream()?;
        Ok(this)
    }

    // --- MCU ---

    fn mcu_packet(msgid: u16, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(MCU_LEN_OVERHEAD as usize + data.len());
        body.extend_from_slice(&(MCU_LEN_OVERHEAD + data.len() as u16).to_le_bytes());
        // The device does not validate this, but it does echo it, which makes matching a
        // reply to its command possible if we ever pipeline more than one.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        body.extend_from_slice(&now_ms.to_le_bytes());
        body.extend_from_slice(&msgid.to_le_bytes());
        body.extend_from_slice(&[0u8; 5]);
        body.extend_from_slice(data);

        let mut packet = Vec::with_capacity(5 + body.len());
        packet.push(MCU_HEAD);
        packet.extend_from_slice(&hid::crc32(&body).to_le_bytes());
        packet.extend_from_slice(&body);
        packet
    }

    /// A raw brightness step as 0..1.
    ///
    /// Steps run `0..BRIGHTNESS_LEVELS - 1` inclusive, so the divisor is one less than the
    /// count — otherwise the top step reports as 7/8 and the slider can never reach its end.
    fn brightness_to_unit(step: u8) -> f32 {
        let top = (BRIGHTNESS_LEVELS - 1).max(1) as f32;
        (step.min(BRIGHTNESS_LEVELS - 1) as f32 / top).clamp(0.0, 1.0)
    }

    /// The step in the payload of a reply to `R_BRIGHTNESS`.
    ///
    /// It comes after a status byte, as in every MCU reply -- `docs/xreal-air.md`, "Reply
    /// layout is not command layout". Reading the first byte as the value, which is what the
    /// command layout suggests, read the status instead: `0x00` for success, so step 0, the
    /// dimmest, for every read. That is why the sidecar's slider sat at the bottom whatever
    /// the glasses were showing.
    fn brightness_in(payload: &[u8]) -> Result<u8> {
        match payload {
            [0, step, ..] => Ok(*step),
            [status, ..] if *status != 0 => Err(HmdError::Protocol(format!(
                "brightness read refused with status {status:#04x}"
            ))),
            _ => Err(HmdError::Protocol("brightness reply had no value".into())),
        }
    }

    /// The nearest raw step to a 0..1 request. May be 0; see [`DIMMEST_STEP`] for why
    /// [`XrealGlasses::request_brightness`] will not send that.
    fn unit_to_brightness(level: f32) -> u8 {
        let top = (BRIGHTNESS_LEVELS - 1).max(1) as f32;
        (level.clamp(0.0, 1.0) * top).round() as u8
    }

    /// Send an MCU command and wait for the device to echo its msgid back, returning the
    /// reply's payload.
    ///
    /// Waits for as long as the device takes, up to `mcu::TIMING.ack`, so nothing on the
    /// render thread should call it. Events that arrive meanwhile are kept for [`Hmd::poll`].
    fn mcu_command(&mut self, msgid: u16, data: &[u8], what: &'static str) -> Result<Vec<u8>> {
        self.mcu.exchange(msgid, data, what)
    }

    /// The next event from the MCU thread, noting a mode change on the way past.
    fn mcu_event(&mut self) -> Option<HmdEvent> {
        let event = self.mcu.next_event()?;
        if let HmdEvent::DisplayModeChanged(mode) = event {
            self.mode = mode;
        }
        Some(event)
    }

    fn decode_async(msgid: u16, packet: &[u8]) -> Option<HmdEvent> {
        let data = packet.get(MCU_DATA_OFFSET..)?;
        match msgid {
            EVT_DISPLAY_TOGGLED => {
                // The wearer long-pressed brightness-up. The payload is the new raw mode;
                // anything we know to be a stereo mode counts as stereo.
                let raw = *data.first()?;
                Some(HmdEvent::DisplayModeChanged(
                    if raw == 0x03 || raw == 0x04 || raw == 0x09 {
                        DisplayMode::Stereo
                    } else {
                        DisplayMode::Mono
                    },
                ))
            }
            EVT_BUTTON_PRESSED => {
                let code = *data.first()?;
                let button = match code {
                    0x01 => HmdButton::BrightnessUp,
                    0x02 => HmdButton::BrightnessDown,
                    other => HmdButton::Unknown(other),
                };
                Some(HmdEvent::Button {
                    button,
                    pressed: true,
                })
            }
            _ => None,
        }
    }

    // --- IMU ---

    fn imu_control(&mut self, start: bool) -> Result<()> {
        // Length 4 counts itself (2) + msgid (1) + data (1). Three is rejected.
        let body = [0x04u8, 0x00, MSG_START_IMU, u8::from(start)];
        let mut packet = Vec::with_capacity(9);
        packet.push(IMU_HEAD);
        packet.extend_from_slice(&hid::crc32(&body).to_le_bytes());
        packet.extend_from_slice(&body);
        self.imu.write_report(&packet)
    }

    fn start_imu_stream(&mut self) -> Result<()> {
        self.imu_control(true)?;
        self.streaming = true;
        Ok(())
    }

    /// Decode one 64-byte IMU data packet. Offsets are from `docs/xreal-air.md` §5 and were
    /// re-confirmed against this hardware.
    fn parse_imu(packet: &[u8]) -> Option<ImuSample> {
        if packet.len() < 54 || packet[0..2] != IMU_DATA_SIG {
            return None;
        }
        let i16le = |o: usize| i16::from_le_bytes([packet[o], packet[o + 1]]) as f64;
        let i32le = |o: usize| {
            i32::from_le_bytes([packet[o], packet[o + 1], packet[o + 2], packet[o + 3]]) as f64
        };
        let i16be = |o: usize| i16::from_be_bytes([packet[o], packet[o + 1]]) as f64;
        let i32be = |o: usize| {
            i32::from_be_bytes([packet[o], packet[o + 1], packet[o + 2], packet[o + 3]]) as f64
        };
        // 24-bit little-endian, sign-extended.
        let i24le = |o: usize| {
            let v = packet[o] as i32 | (packet[o + 1] as i32) << 8 | (packet[o + 2] as i32) << 16;
            (if v & 0x0080_0000 != 0 {
                v - (1 << 24)
            } else {
                v
            }) as f64
        };

        let timestamp_ns = u64::from_le_bytes(packet[4..12].try_into().ok()?);
        let temperature_c = Some(i16le(2) as f32);

        let (gyro_mul, gyro_div) = (i16le(12), i32le(14));
        let (acc_mul, acc_div) = (i16le(27), i32le(29));
        // Alone among the three groups, the magnetometer's scalers are big-endian.
        let (mag_mul, mag_div) = (i16be(42), i32be(44));
        if gyro_div == 0.0 || acc_div == 0.0 || mag_div == 0.0 {
            // The first packets after starting the stream arrive entirely zeroed.
            return None;
        }

        let gyro = DVec3::new(i24le(18), i24le(21), i24le(24)) * gyro_mul / gyro_div;
        let accel = DVec3::new(i24le(33), i24le(36), i24le(39)) * acc_mul / acc_div;
        // Offset binary: XOR the sign bit, then reinterpret as two's complement.
        let mag_axis = |o: usize| {
            let raw = u16::from_le_bytes([packet[o], packet[o + 1]]) ^ 0x8000;
            raw as i16 as f64
        };
        let mag = DVec3::new(mag_axis(48), mag_axis(50), mag_axis(52)) * mag_mul / mag_div;

        Some(ImuSample {
            timestamp_ns,
            gyro,
            accel,
            mag,
            temperature_c,
        })
    }
}

impl Hmd for XrealGlasses {
    fn info(&self) -> &HmdInfo {
        &self.info
    }

    fn set_display_mode(&mut self, mode: DisplayMode) -> Result<DisplayMode> {
        let (primary, fallback) = match mode {
            DisplayMode::Mono => (self.spec.mode_mono, None),
            DisplayMode::Stereo => (self.spec.mode_stereo, Some(self.spec.mode_stereo_fallback)),
        };

        let mut err = match self.mcu_command(MSG_W_DISP_MODE, &[primary], "display mode") {
            Ok(_) => {
                self.mode = mode;
                return Ok(mode);
            }
            Err(e) => e,
        };
        // The preferred stereo mode is the higher refresh rate. If the link cannot carry it,
        // a lower one is much better than staying flat, so try it before giving up.
        if let Some(alt) = fallback {
            log::warn!("display mode {primary:#04x} not acknowledged ({err}); trying {alt:#04x}");
            match self.mcu_command(MSG_W_DISP_MODE, &[alt], "display mode (fallback)") {
                Ok(_) => {
                    self.mode = mode;
                    return Ok(mode);
                }
                Err(e) => err = e,
            }
        }
        Err(err)
    }

    fn display_mode(&self) -> DisplayMode {
        self.mode
    }

    fn brightness(&mut self) -> Result<f32> {
        let reply = self.mcu_command(MSG_R_BRIGHTNESS, &[], "read brightness")?;
        Ok(Self::brightness_to_unit(Self::brightness_in(&reply)?))
    }

    fn request_brightness(&mut self, level: f32) -> Result<f32> {
        // Clamped here rather than in `unit_to_brightness`, which is the honest inverse of
        // `brightness_to_unit` and has to stay able to express step 0 — the glasses report it,
        // and reading it back as something else would be a lie about the hardware's state.
        // Refusing to *select* it is a different thing from pretending it cannot happen.
        let step = Self::unit_to_brightness(level).max(DIMMEST_STEP);
        self.mcu.brightness(step)?;
        // The step asked for, not one read back: that would be a second round trip, and the
        // wearer is dragging. A refusal arrives later, through `poll`.
        Ok(Self::brightness_to_unit(step))
    }

    fn poll(&mut self, timeout: Duration) -> Result<Option<HmdEvent>> {
        // Whatever the MCU thread has already passed over comes first. It is rare, and
        // otherwise a stream of IMU samples could keep a button press waiting.
        if let Some(event) = self.mcu_event() {
            return Ok(Some(event));
        }
        // Wait on both at once: the IMU streams at ~1 kHz and the MCU speaks only
        // occasionally, so waiting on them separately would either add latency to samples or
        // leave an event sitting until the next one.
        let mut fds = [
            libc::pollfd {
                fd: self.imu.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.mcu.ready_fd().unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if rc < 0 {
            let source = std::io::Error::last_os_error();
            if source.kind() == std::io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(HmdError::Io {
                path: "xreal poll".into(),
                source,
            });
        }
        if rc == 0 {
            return Ok(None);
        }

        // A hangup on the IMU means the glasses were unplugged. One on the MCU arrives from
        // its thread as an event.
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(Some(HmdEvent::Disconnected));
        }

        if fds[1].revents != 0 {
            self.mcu.clear_ready();
            if let Some(event) = self.mcu_event() {
                return Ok(Some(event));
            }
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let n = self
                .imu
                .read_report(&mut self.buf, Duration::ZERO)?
                .unwrap_or(0);
            if let Some(sample) = Self::parse_imu(&self.buf[..n]) {
                if sample.is_plausible() {
                    return Ok(Some(HmdEvent::Imu(sample)));
                }
            }
            // Acks, rejections and the zeroed warm-up packets all land here. Not an error;
            // the caller polls again.
        }
        Ok(None)
    }

    fn event_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.imu.as_raw_fd())
    }
}

impl Drop for XrealGlasses {
    fn drop(&mut self) {
        // Leaving the glasses in stereo makes every ordinary desktop look broken — squashed
        // into half the screen — and the cause is not remotely obvious to whoever plugs them
        // in next. Restoring is best effort but worth attempting on every exit path.
        if self.mode == DisplayMode::Stereo {
            let mono = self.spec.mode_mono;
            let _ = self.mcu_command(MSG_W_DISP_MODE, &[mono], "restore mono");
        }
        if self.streaming {
            let _ = self.imu_control(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_reaches_both_ends_of_the_slider() {
        // The off-by-one that makes a slider feel broken: dividing by the level *count* rather
        // than the top level leaves the brightest step reading as 7/8, so dragging to the far
        // right never fills the bar and the wearer keeps pushing at a control already at its
        // limit.
        assert_eq!(XrealGlasses::brightness_to_unit(0), 0.0);
        assert_eq!(XrealGlasses::brightness_to_unit(BRIGHTNESS_LEVELS - 1), 1.0);
        assert_eq!(XrealGlasses::unit_to_brightness(0.0), 0);
        assert_eq!(XrealGlasses::unit_to_brightness(1.0), BRIGHTNESS_LEVELS - 1);
    }

    #[test]
    fn the_bottom_of_the_slider_is_dim_rather_than_off() {
        // The failure this prevents, in full: the wearer drags the glasses' brightness to the
        // bottom, the panel goes off, and the slider that would bring it back is drawn inside
        // the glasses. Nothing on the Deck says what happened -- Spatiand reads the brightness
        // back as 0% and carries on -- so it presents as the glasses having died.
        //
        // The shell's own floor does not catch it. That one is five percent, and five percent
        // of seven steps rounds to zero, so the floor selects exactly the step it exists to
        // avoid. Every way the wearer can reach the bottom is checked here, because the number
        // that broke it looked like a floor and was one, for a different device.
        for level in [0.0, 0.01, 0.05, 0.07, -1.0, f32::MIN] {
            let step = XrealGlasses::unit_to_brightness(level).max(DIMMEST_STEP);
            assert!(
                step >= 1,
                "{level} selects step {step}, which is the panel off"
            );
        }
        // And it is a floor, not a rescaling: everything above it is untouched.
        for step in 1..BRIGHTNESS_LEVELS {
            let level = XrealGlasses::brightness_to_unit(step);
            assert_eq!(
                XrealGlasses::unit_to_brightness(level).max(DIMMEST_STEP),
                step,
                "the floor moved step {step}"
            );
        }
    }

    #[test]
    fn the_brightness_is_the_byte_after_the_status() {
        // The payload of a real reply from an Air showing step 5, from offset 22 on.
        assert_eq!(XrealGlasses::brightness_in(&[0x00, 0x05, 0, 0, 0, 0]).unwrap(), 5);
        // Read as the value, the status byte made every panel look as dim as it goes.
        assert_ne!(XrealGlasses::brightness_in(&[0x00, 0x05]).unwrap(), 0);
        assert!(XrealGlasses::brightness_in(&[0x01, 0x05]).is_err(), "a refusal is not a step");
        assert!(XrealGlasses::brightness_in(&[0x00]).is_err());
        assert!(XrealGlasses::brightness_in(&[]).is_err());
    }

    #[test]
    fn every_step_survives_the_round_trip() {
        // The sidecar shows the value it got back, so a step that does not map cleanly both
        // ways would make the handle jump away from the finger that just placed it.
        for step in 0..BRIGHTNESS_LEVELS {
            let back = XrealGlasses::unit_to_brightness(XrealGlasses::brightness_to_unit(step));
            assert_eq!(back, step, "step {step} came back as {back}");
        }
    }

    #[test]
    fn a_request_past_either_end_is_clamped_rather_than_wrapped() {
        // A drag off the end of the bar produces these, and a u8 that wraps would turn "as dim
        // as possible" into "as bright as possible" on a display strapped to someone's face.
        assert_eq!(XrealGlasses::unit_to_brightness(-5.0), 0);
        assert_eq!(XrealGlasses::unit_to_brightness(9.0), BRIGHTNESS_LEVELS - 1);
        assert_eq!(XrealGlasses::brightness_to_unit(200), 1.0);
    }

    #[test]
    fn mcu_packet_matches_the_documented_capture() {
        // docs/xreal-air.md §6 shows a real "into 3D" packet. We cannot reproduce its
        // timestamp, but every other field — and critically the length — must match.
        let pkt = XrealGlasses::mcu_packet(MSG_W_DISP_MODE, &[0x03]);
        assert_eq!(pkt.len(), 23, "one-byte payload must give a 23-byte packet");
        assert_eq!(pkt[0], 0xFD);
        // Length field counts itself: 2 + 8 + 2 + 5 + 1 = 18.
        assert_eq!(u16::from_le_bytes([pkt[5], pkt[6]]), 18);
        assert_eq!(
            u16::from_le_bytes([pkt[MCU_MSGID_OFFSET], pkt[MCU_MSGID_OFFSET + 1]]),
            MSG_W_DISP_MODE
        );
        assert_eq!(pkt[MCU_DATA_OFFSET], 0x03);
        // The CRC covers everything from the length field onward.
        assert_eq!(
            u32::from_le_bytes([pkt[1], pkt[2], pkt[3], pkt[4]]),
            hid::crc32(&pkt[5..])
        );
    }

    #[test]
    fn imu_packet_is_the_documented_nine_bytes() {
        let body = [0x04u8, 0x00, MSG_START_IMU, 0x01];
        let mut pkt = vec![IMU_HEAD];
        pkt.extend_from_slice(&hid::crc32(&body).to_le_bytes());
        pkt.extend_from_slice(&body);
        assert_eq!(
            pkt,
            vec![0xaa, 0xc5, 0xd1, 0x21, 0x42, 0x04, 0x00, 0x19, 0x01]
        );
    }

    // Scalers as reported by this hardware, confirmed by the spike and by docs §5.
    const GYRO_MUL: i16 = 4000;
    const ACC_MUL: i16 = 32;
    const MAG_MUL: i16 = 128;
    const SENSOR_DIV: i32 = 16_777_216;
    const MAG_DIV: i32 = 262_144;

    fn put_i24(p: &mut [u8], off: usize, v: i32) {
        p[off] = (v & 0xFF) as u8;
        p[off + 1] = ((v >> 8) & 0xFF) as u8;
        p[off + 2] = ((v >> 16) & 0xFF) as u8;
    }

    /// A complete resting packet.
    ///
    /// The capture printed in the docs stops at offset 28, so it cannot be used verbatim —
    /// its zero divisors are (correctly) refused by the parser. Its real prefix is kept
    /// byte-for-byte and the remainder is filled with the scalers this hardware reports,
    /// giving a packet that is faithful where the capture exists and documented elsewhere.
    fn resting_capture() -> Vec<u8> {
        let mut p = vec![0u8; 64];
        // --- verbatim from docs §5, offsets 0..29 ---
        p[0..29].copy_from_slice(&[
            0x01, 0x02, 0x94, 0x04, 0x28, 0x0a, 0x4d, 0xee, 0x3a, 0x02, 0x00, 0x00, 0xa0, 0x0f,
            0x00, 0x00, 0x00, 0x01, 0xe0, 0x07, 0x00, 0x80, 0xf2, 0xff, 0xe0, 0xf4, 0xff, 0x20,
            0x00,
        ]);
        // --- reconstructed remainder ---
        p[29..33].copy_from_slice(&SENSOR_DIV.to_le_bytes());
        // One g straight up, so |accel| reads 1.0 exactly like the real thing at rest.
        let one_g = SENSOR_DIV / ACC_MUL as i32;
        put_i24(&mut p, 33, 0);
        put_i24(&mut p, 36, 0);
        put_i24(&mut p, 39, one_g);
        // Magnetometer scalers alone are big-endian.
        p[42..44].copy_from_slice(&MAG_MUL.to_be_bytes());
        p[44..48].copy_from_slice(&MAG_DIV.to_be_bytes());
        for (i, axis) in [0.21f64, 0.0, -0.21].iter().enumerate() {
            let raw = (axis * MAG_DIV as f64 / MAG_MUL as f64).round() as i16;
            // Stored as offset binary, which is what the XOR in the parser undoes.
            let stored = (raw as u16) ^ 0x8000;
            p[48 + i * 2..50 + i * 2].copy_from_slice(&stored.to_le_bytes());
        }
        p
    }

    #[test]
    fn parses_the_documented_scalers() {
        let sample = XrealGlasses::parse_imu(&resting_capture()).expect("should parse");
        // Raw gyro X from the capture is 0x0007e0; the multiplier/divisor are what make the
        // result deg/s rather than an arbitrary count.
        let expected_x = 0x0007e0 as f64 * GYRO_MUL as f64 / SENSOR_DIV as f64;
        assert!((sample.gyro.x - expected_x).abs() < 1e-9);
        // Y and Z in the capture are negative — a sign-extension slip in the int24 reader
        // would make them large positives.
        assert!(sample.gyro.y < 0.0 && sample.gyro.z < 0.0);
        // Little-endian u64 over bytes 4..12 of the capture.
        assert_eq!(sample.timestamp_ns, 0x0000_023a_ee4d_0a28);
    }

    #[test]
    fn resting_packet_passes_the_physical_sanity_checks() {
        // The three checks from docs §12. Getting any offset wrong destroys all of them at
        // once, which is exactly why they are worth asserting together.
        let sample = XrealGlasses::parse_imu(&resting_capture()).expect("should parse");
        assert!(
            (sample.accel.length() - 1.0).abs() < 0.01,
            "|accel| must read ~1 g, got {}",
            sample.accel.length()
        );
        assert!(
            (sample.mag.length() - 0.297).abs() < 0.01,
            "|mag| must read ~0.3 G, got {}",
            sample.mag.length()
        );
        // Not zero: a resting headset still shows its gyro *bias*, which on these glasses is
        // ~1.2 deg/s and is precisely what `spatiand-track` exists to estimate away. The
        // documented capture decodes to (+0.48, -0.82, -0.68), which sits right on the
        // (+0.55, -0.80, -0.72) HoloFrame measured and the (+0.64, -0.86, -0.73) this
        // hardware gave the spike — a third independent confirmation of the field offsets.
        assert!(
            (0.5..2.0).contains(&sample.gyro.length()),
            "expected the known resting bias, got {}",
            sample.gyro.length()
        );
        assert!(
            sample.gyro.x > 0.0 && sample.gyro.y < 0.0 && sample.gyro.z < 0.0,
            "bias signs should match the measured (+,-,-) pattern: {:?}",
            sample.gyro
        );
        assert!(sample.is_plausible());
    }

    #[test]
    fn rejects_a_zeroed_warmup_packet() {
        let mut p = vec![0u8; 64];
        p[0..2].copy_from_slice(&IMU_DATA_SIG);
        assert!(
            XrealGlasses::parse_imu(&p).is_none(),
            "an all-zero packet has zero divisors and must be discarded, not divided by"
        );
    }

    #[test]
    fn rejects_a_non_data_signature() {
        let mut p = resting_capture();
        p[1] = 0x53; // the init-packet signature AA 53
        assert!(XrealGlasses::parse_imu(&p).is_none());
    }

    #[test]
    fn magnetometer_offset_binary_round_trips() {
        // 0x8000 in offset binary is zero; without the XOR it reads as -32768 and the field
        // magnitude becomes nonsense. This is the exact bug the spike hit.
        let mut p = resting_capture();
        p[42..44].copy_from_slice(&128i16.to_be_bytes());
        p[44..48].copy_from_slice(&262_144i32.to_be_bytes());
        p[48..50].copy_from_slice(&0x8000u16.to_le_bytes());
        p[50..52].copy_from_slice(&0x8000u16.to_le_bytes());
        p[52..54].copy_from_slice(&0x8000u16.to_le_bytes());
        let sample = XrealGlasses::parse_imu(&p).expect("should parse");
        assert_eq!(sample.mag, DVec3::ZERO);
    }
}
