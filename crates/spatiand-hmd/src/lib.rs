//! Headset abstraction — the seam that keeps Spatiand from being an XREAL Air program.
//!
//! Everything above this crate sees only [`Hmd`] and the types below. Nothing here mentions
//! a specific product except inside a driver module and `devices.toml`.
//!
//! Two shapes of headset have to fit behind one trait, and they differ in an important way:
//!
//! * **Raw-IMU devices** (XREAL Air and every other USB-HID pair of glasses) hand over
//!   gyro/accel/mag and nothing else. Turning that into an orientation is `spatiand-track`'s
//!   job, and it is genuinely hard — see [`HmdInfo::provides_fused_pose`].
//! * **Runtime-backed devices** (OpenXR/Monado, a vendor SDK) hand over an already-fused
//!   pose and will never emit an [`ImuSample`] at all.
//!
//! So [`HmdEvent`] carries both, and consumers branch on `provides_fused_pose` once at
//! startup rather than guessing per event.

use std::time::Duration;

pub mod device;
pub mod hid;
pub mod null;
pub mod xreal;

pub use device::DeviceSpec;
pub use null::NullHmd;
pub use xreal::XrealGlasses;

use glam::{DQuat, DVec3};

/// Angular rate in deg/s, acceleration in g, magnetic field in gauss.
///
/// Those units are the ones the hardware reports and the ones the sanity checks are written
/// against: with the headset at rest `accel.length()` must read ≈ 1.0 and `mag.length()`
/// ≈ 0.3. If either is wrong the field offsets are wrong, and every downstream symptom will
/// look like a filter bug instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuSample {
    /// Device-supplied timestamp, nanoseconds. Monotonic; the epoch is arbitrary.
    pub timestamp_ns: u64,
    pub gyro: DVec3,
    pub accel: DVec3,
    pub mag: DVec3,
    /// Degrees Celsius, if the device reports it.
    pub temperature_c: Option<f32>,
}

impl ImuSample {
    /// Cheap plausibility check. The first packets after starting a stream arrive with every
    /// sensor field zeroed and must be discarded rather than fed to a filter.
    pub fn is_plausible(&self) -> bool {
        self.accel != DVec3::ZERO && (0.5..2.0).contains(&self.accel.length())
    }
}

/// An orientation from a headset that does its own fusion. Position is present only for
/// 6DoF devices; 3DoF glasses leave it `None` and the shell uses a neck model instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub orientation: DQuat,
    pub position: Option<DVec3>,
    pub timestamp_ns: u64,
}

/// What the headset should do with the pixels it is sent.
///
/// `Stereo` means the device accepts a double-width signal and gives each eye half of it.
/// Which concrete mode that maps to — and at what refresh — is a per-device detail in
/// `devices.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    Mono,
    Stereo,
}

/// Buttons on the headset itself, not on any controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmdButton {
    BrightnessUp,
    BrightnessDown,
    /// Reported but not identified as any of the above.
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HmdEvent {
    Imu(ImuSample),
    Pose(Pose),
    Button { button: HmdButton, pressed: bool },
    /// The wearer changed the mode with a physical button, or the device changed it itself.
    DisplayModeChanged(DisplayMode),
    Disconnected,
}

/// Static description of the connected headset. Read once at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct HmdInfo {
    pub name: String,
    /// Resolution delivered to **one** eye in stereo mode.
    pub per_eye: (u32, u32),
    /// Horizontal field of view of one eye, degrees. Note this is *horizontal*: vendor
    /// marketing quotes a diagonal figure, which is a different and larger number.
    pub h_fov_deg: f64,
    /// Interpupillary distance to start from, millimetres. A per-user calibration should
    /// override this — it materially affects whether the world feels solid.
    pub default_ipd_mm: f64,
    /// Whether the device can present a stereo signal at all.
    pub supports_stereo: bool,
    /// `true` for runtime-backed devices that emit [`HmdEvent::Pose`]; `false` for raw-IMU
    /// devices, whose samples must be run through `spatiand-track`.
    pub provides_fused_pose: bool,
    /// Where this model's IMU axes point in the head frame, if it has been measured.
    ///
    /// `Some` means the sensor convention is known hardware fact and the tracker should use
    /// it in preference to anything stored on disk. `None` means it has to be measured from
    /// the wearer's own movements. See [`device::Mounting`].
    pub sensor_axes: Option<device::Mounting>,
}

#[derive(Debug, thiserror::Error)]
pub enum HmdError {
    #[error("no supported headset found")]
    NotFound,
    #[error("{device} is missing HID interface {interface}")]
    InterfaceMissing { device: String, interface: u8 },
    #[error("i/o on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("device did not acknowledge {what} (msgid {msgid:#06x})")]
    NoAck { what: &'static str, msgid: u16 },
    #[error("device reported a malformed packet: {0}")]
    Protocol(String),
    #[error("{0} is not supported by this device")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, HmdError>;

/// A connected headset.
///
/// Implementations are expected to be cheap to poll and safe to move to a dedicated thread;
/// they are not required to be `Sync`, since exactly one owner should be talking to the
/// hardware.
pub trait Hmd: Send {
    fn info(&self) -> &HmdInfo;

    /// Switch the panel between mono and stereo. Returns the mode actually reached, which
    /// may differ from the request if the device fell back (e.g. a lower refresh rate).
    fn set_display_mode(&mut self, mode: DisplayMode) -> Result<DisplayMode>;

    fn display_mode(&self) -> DisplayMode;

    /// Wait up to `timeout` for the next event. `Ok(None)` means the timeout expired with
    /// nothing to report, which is not an error.
    fn poll(&mut self, timeout: Duration) -> Result<Option<HmdEvent>>;

    /// A file descriptor that becomes readable when [`Hmd::poll`] has work, for integration
    /// into an external event loop. `None` if the backend cannot offer one.
    fn event_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// Best effort: leave the device in a state a normal desktop can use. Called on shutdown
    /// and, importantly, on panic paths — glasses left in stereo mode show every desktop
    /// squashed into half the screen, which looks like a broken machine.
    fn shutdown(&mut self) {
        let _ = self.set_display_mode(DisplayMode::Mono);
    }
}

/// Is a supported headset plugged in?
///
/// Deliberately cheap and side-effect free: it walks the hidraw nodes and matches against the
/// device table without opening anything. The hotplug poll calls this on a timer, and opening
/// the device to find out would send MCU traffic to hardware that may be mid-enumeration.
pub fn is_present() -> bool {
    hid::enumerate()
        .iter()
        .any(|n| device::lookup(n.vid, n.pid).is_some())
}

/// Probe for any supported headset.
///
/// Ordered deliberately: real hardware first, then the null device only if explicitly asked
/// for via `SPATIAND_HMD=null`, so plugging glasses in never silently loses to a stub.
pub fn open_any() -> Result<Box<dyn Hmd>> {
    if std::env::var("SPATIAND_HMD").as_deref() == Ok("null") {
        log::info!("SPATIAND_HMD=null — using the stub headset");
        return Ok(Box::new(NullHmd::new()));
    }
    match XrealGlasses::open_any() {
        Ok(g) => Ok(Box::new(g)),
        Err(HmdError::NotFound) => Err(HmdError::NotFound),
        Err(e) => Err(e),
    }
}
