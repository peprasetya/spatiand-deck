//! Raw IMU samples to a file, for replaying head tracking offline.
//!
//! Yaw drift cannot be diagnosed from inside the glasses: the wearer sees the world slide and
//! nothing says whether the gyro offset, the magnetometer or the wearer's own slow turn did it.
//! A recording of what the sensor actually said can be run through the tracker again, as often
//! as needed, with different tuning — which is how HoloFrame found its drift.
//!
//! On when `SPATIAND_IMU_RECORD` is set (in `~/.config/spatiand/session.env`): to a path, or to
//! `1` for `~/.local/share/spatiand/imu-<unix time>.bin`. The format is HoloFrame's
//! `imu-record`, so its tools read it too: ten little-endian f64 per sample — timestamp (ns),
//! gyro xyz (deg/s), accel xyz (g), mag xyz (gauss) — raw, before any axis mapping.
//!
//! Stops by itself after an hour, about 290 MB.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const MAX_SAMPLES: u64 = 3_600_000;

pub struct Recorder {
    out: BufWriter<File>,
    path: PathBuf,
    samples: u64,
}

impl Recorder {
    /// A recorder if the environment asks for one.
    pub fn from_env() -> Option<Recorder> {
        let asked = std::env::var("SPATIAND_IMU_RECORD").ok()?;
        let path = match asked.as_str() {
            "" | "0" => return None,
            "1" => {
                let home = std::env::var_os("HOME")?;
                let dir = PathBuf::from(home).join(".local/share/spatiand");
                let _ = std::fs::create_dir_all(&dir);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                dir.join(format!("imu-{now}.bin"))
            }
            other => PathBuf::from(other),
        };
        match File::create(&path) {
            Ok(file) => {
                log::info!("recording the IMU to {}", path.display());
                Some(Recorder {
                    out: BufWriter::with_capacity(1 << 16, file),
                    path,
                    samples: 0,
                })
            }
            Err(e) => {
                log::warn!("could not record the IMU to {}: {e}", path.display());
                None
            }
        }
    }

    /// Append one sample. `false` once the recording is full and should be dropped.
    pub fn write(&mut self, s: &spatiand_hmd::ImuSample) -> bool {
        let values = [
            s.timestamp_ns as f64,
            s.gyro.x,
            s.gyro.y,
            s.gyro.z,
            s.accel.x,
            s.accel.y,
            s.accel.z,
            s.mag.x,
            s.mag.y,
            s.mag.z,
        ];
        for v in values {
            if self.out.write_all(&v.to_le_bytes()).is_err() {
                return false;
            }
        }
        self.samples += 1;
        if self.samples >= MAX_SAMPLES {
            let _ = self.out.flush();
            log::info!("IMU recording full: {}", self.path.display());
            return false;
        }
        true
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.out.flush();
    }
}
