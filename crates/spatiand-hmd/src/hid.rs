//! Minimal hidraw access — enumeration by sysfs, blocking reads with a timeout.
//!
//! Deliberately not using libudev or hidapi. Everything needed is three lines of
//! `/sys/class/hidraw/*/device/uevent`, and on the Steam Deck the glasses' nodes already
//! carry an ACL granting the desktop user read/write, so there is no udev rule to install
//! and no root to acquire. Fewer C dependencies also means the crate cross-builds anywhere.
//!
//! The one non-obvious part is the report-id byte. Linux `write(2)` on a hidraw node treats
//! the first byte as the report number; devices that do not use numbered reports — these
//! glasses included — need a leading `0x00` which the kernel strips before it hits the wire.
//! Omitting it silently sends a packet shifted by one byte.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{HmdError, Result};

/// One hidraw node belonging to a USB HID device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidNode {
    pub path: PathBuf,
    pub vid: u16,
    pub pid: u16,
    /// USB interface number, recovered from the `HID_PHYS` suffix (`…/inputN`).
    ///
    /// This is the only field that discriminates the glasses' interfaces: IMU, MCU and the
    /// unidentified third interface all report the same HID usage page (`0x0041`), so usage
    /// cannot be used to tell them apart.
    pub interface: u8,
}

/// Enumerate every hidraw node on the system.
pub fn enumerate() -> Vec<HidNode> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    for entry in entries.flatten() {
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = std::fs::read_to_string(&uevent) else {
            continue;
        };
        if let Some(node) = parse_uevent(&text, entry.file_name().to_string_lossy().as_ref()) {
            out.push(node);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Split out from [`enumerate`] so the parsing can be tested without sysfs.
fn parse_uevent(text: &str, node_name: &str) -> Option<HidNode> {
    let mut vid_pid = None;
    let mut interface = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("HID_ID=") {
            // "bus:VVVVVVVV:PPPPPPPP", all hex, vendor and product zero-padded to 8.
            let mut parts = v.split(':');
            let _bus = parts.next()?;
            let vid = u32::from_str_radix(parts.next()?.trim(), 16).ok()? as u16;
            let pid = u32::from_str_radix(parts.next()?.trim(), 16).ok()? as u16;
            vid_pid = Some((vid, pid));
        } else if let Some(v) = line.strip_prefix("HID_PHYS=") {
            // e.g. "usb-0000:04:00.3-1/input3"
            if let Some((_, n)) = v.rsplit_once("/input") {
                interface = n.trim().parse::<u8>().ok();
            }
        }
    }
    let (vid, pid) = vid_pid?;
    Some(HidNode {
        path: Path::new("/dev").join(node_name),
        vid,
        pid,
        // A device with a single interface may omit the suffix; treat that as interface 0.
        interface: interface.unwrap_or(0),
    })
}

/// Find the node for a specific device and interface.
pub fn find(vid: u16, pid: u16, interface: u8) -> Option<HidNode> {
    enumerate()
        .into_iter()
        .find(|n| n.vid == vid && n.pid == pid && n.interface == interface)
}

/// An open hidraw node.
pub struct HidDevice {
    file: File,
    path: PathBuf,
}

impl HidDevice {
    pub fn open(node: &HidNode) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node.path)
            .map_err(|source| HmdError::Io {
                path: node.path.display().to_string(),
                source,
            })?;
        Ok(Self {
            file,
            path: node.path.clone(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Write one output report, prefixing the report-id byte these devices require.
    pub fn write_report(&mut self, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(payload.len() + 1);
        buf.push(0x00);
        buf.extend_from_slice(payload);
        self.file.write_all(&buf).map_err(|source| HmdError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }

    /// Read one input report, waiting at most `timeout`.
    ///
    /// `Ok(None)` means the timeout expired — a normal, non-error outcome that callers use
    /// to stay responsive to shutdown.
    pub fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        if !self.wait_readable(timeout)? {
            return Ok(None);
        }
        match self.file.read(buf) {
            Ok(n) => Ok(Some(n)),
            // A device unplugged mid-read surfaces here; the caller turns it into
            // HmdEvent::Disconnected rather than a hard failure.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(source) => Err(HmdError::Io {
                path: self.path.display().to_string(),
                source,
            }),
        }
    }

    fn wait_readable(&self, timeout: Duration) -> Result<bool> {
        let mut fds = [libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // poll(2) takes milliseconds; clamp so a very long Duration cannot overflow into a
        // negative (which would mean "block forever" and hang shutdown).
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
        if rc < 0 {
            let source = std::io::Error::last_os_error();
            // A signal during poll is not a failure; report "nothing ready" and let the
            // caller come round again.
            if source.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(HmdError::Io {
                path: self.path.display().to_string(),
                source,
            });
        }
        Ok(rc > 0 && fds[0].revents & libc::POLLIN != 0)
    }
}

/// CRC-32/ISO-HDLC — the ordinary zlib/PNG CRC32.
///
/// Both glasses protocols check every packet with this. Getting it wrong yields one constant
/// 12-byte rejection reply, which is easy to mistake for "the device ignored me".
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_documented_capture() {
        // docs/xreal-air.md §5: `aa c5 d1 21 42 04 00 19 01` is a real capture the device
        // acted on. If this fails, nothing else in the protocol will work.
        let body = [0x04u8, 0x00, 0x19, 0x01];
        assert_eq!(crc32(&body).to_le_bytes(), [0xc5, 0xd1, 0x21, 0x42]);
    }

    #[test]
    fn parses_a_real_deck_uevent() {
        // Captured verbatim from the Steam Deck with the glasses attached.
        let text = "DRIVER=hid-generic\n\
                    HID_ID=0003:00003318:00000424\n\
                    HID_NAME=Vendor Air\n\
                    HID_PHYS=usb-0000:04:00.3-1/input3\n\
                    HID_UNIQ=A00011:32:00\n\
                    MODALIAS=hid:b0003g0001v00003318p00000424\n";
        let node = parse_uevent(text, "hidraw4").expect("should parse");
        assert_eq!(node.vid, 0x3318);
        assert_eq!(node.pid, 0x0424);
        assert_eq!(node.interface, 3);
        assert_eq!(node.path, Path::new("/dev/hidraw4"));
    }

    #[test]
    fn parses_the_steam_controller_uevent() {
        let text = "DRIVER=hid-steam\n\
                    HID_ID=0003:000028DE:00001205\n\
                    HID_NAME=Valve Software Steam Controller\n\
                    HID_PHYS=usb-0000:04:00.4-3/input2\n";
        let node = parse_uevent(text, "hidraw3").expect("should parse");
        assert_eq!((node.vid, node.pid, node.interface), (0x28DE, 0x1205, 2));
    }

    #[test]
    fn missing_phys_defaults_to_interface_zero() {
        let text = "HID_ID=0003:00001234:00005678\nHID_PHYS=\n";
        let node = parse_uevent(text, "hidraw9").expect("should parse");
        assert_eq!(node.interface, 0);
    }
}
