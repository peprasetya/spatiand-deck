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

use std::time::Duration;

use glam::DVec3;

use crate::device::{self, DeviceSpec};
use crate::hid::{self, HidDevice, HidNode};
use crate::{
    DisplayMode, Hmd, HmdButton, HmdError, HmdEvent, HmdInfo, ImuSample, Result,
};

// --- MCU (interface 4) ---
const MCU_HEAD: u8 = 0xFD;
const MSG_W_DISP_MODE: u16 = 0x0008;
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
    mcu: HidDevice,
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

    /// Send an MCU command and wait for the device to echo its msgid back.
    ///
    /// Async events arriving in the meantime are handled rather than dropped, so a button
    /// press that happens to coincide with a mode change is not lost.
    fn mcu_command(
        &mut self,
        msgid: u16,
        data: &[u8],
        what: &'static str,
        pending: &mut Vec<HmdEvent>,
    ) -> Result<()> {
        self.mcu.write_report(&Self::mcu_packet(msgid, data))?;

        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        let mut buf = [0u8; 64];
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Some(n) = self.mcu.read_report(&mut buf, remaining)? else {
                break;
            };
            if n <= MCU_MSGID_OFFSET + 1 {
                continue;
            }
            let echoed = u16::from_le_bytes([buf[MCU_MSGID_OFFSET], buf[MCU_MSGID_OFFSET + 1]]);
            if echoed == msgid {
                return Ok(());
            }
            if let Some(evt) = Self::decode_async(echoed, &buf[..n]) {
                pending.push(evt);
            } else {
                log::trace!("MCU async push {echoed:#06x} while awaiting {what}");
            }
        }
        Err(HmdError::NoAck { what, msgid })
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
            (if v & 0x0080_0000 != 0 { v - (1 << 24) } else { v }) as f64
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
        let mut pending = Vec::new();
        let (primary, fallback) = match mode {
            DisplayMode::Mono => (self.spec.mode_mono, None),
            DisplayMode::Stereo => (self.spec.mode_stereo, Some(self.spec.mode_stereo_fallback)),
        };

        let mut err = match self.mcu_command(MSG_W_DISP_MODE, &[primary], "display mode", &mut pending) {
            Ok(()) => {
                self.mode = mode;
                return Ok(mode);
            }
            Err(e) => e,
        };
        // The preferred stereo mode is the higher refresh rate. If the link cannot carry it,
        // a lower one is much better than staying flat, so try it before giving up.
        if let Some(alt) = fallback {
            log::warn!("display mode {primary:#04x} not acknowledged ({err}); trying {alt:#04x}");
            match self.mcu_command(MSG_W_DISP_MODE, &[alt], "display mode (fallback)", &mut pending) {
                Ok(()) => {
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

    fn poll(&mut self, timeout: Duration) -> Result<Option<HmdEvent>> {
        // Watch both interfaces at once: the IMU streams at ~1 kHz and the MCU speaks only
        // occasionally, so polling them separately would either add latency to samples or
        // spin on the MCU.
        let mut fds = [
            libc::pollfd {
                fd: self.imu.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.mcu.as_raw_fd(),
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

        // A hangup on either interface means the glasses were unplugged.
        if fds.iter().any(|f| f.revents & (libc::POLLHUP | libc::POLLERR) != 0) {
            return Ok(Some(HmdEvent::Disconnected));
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
            return Ok(None);
        }

        if fds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 64];
            if let Some(n) = self.mcu.read_report(&mut buf, Duration::ZERO)? {
                if n > MCU_MSGID_OFFSET + 1 {
                    let msgid =
                        u16::from_le_bytes([buf[MCU_MSGID_OFFSET], buf[MCU_MSGID_OFFSET + 1]]);
                    if let Some(evt) = Self::decode_async(msgid, &buf[..n]) {
                        if let HmdEvent::DisplayModeChanged(m) = evt {
                            self.mode = m;
                        }
                        return Ok(Some(evt));
                    }
                    log::trace!("MCU async push {msgid:#06x}");
                }
            }
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
            let mut pending = Vec::new();
            let mono = self.spec.mode_mono;
            let _ = self.mcu_command(MSG_W_DISP_MODE, &[mono], "restore mono", &mut pending);
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
        assert_eq!(pkt, vec![0xaa, 0xc5, 0xd1, 0x21, 0x42, 0x04, 0x00, 0x19, 0x01]);
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
