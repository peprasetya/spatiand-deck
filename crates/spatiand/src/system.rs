//! What the machine is doing, for the sidecar.
//!
//! All of it comes out of `/proc` and `/sys`, which means it works on any Linux rather than
//! only on a Deck, and needs no daemon, no D-Bus and no permissions beyond reading files.
//!
//! The parsing is separated from the reading so it can be tested against real captured text.
//! That matters more than it looks: `/proc/stat` is a cumulative counter, so a busy percentage
//! is a *difference* between two readings, and an off-by-one in which fields count as idle
//! produces a number that is plausible, stable, and wrong.

use std::collections::VecDeque;

/// How many samples of history the graphs keep.
///
/// At one sample a second this is a minute and a half, which is long enough to see a shader
/// compile or a page load as a shape rather than a spike.
pub const HISTORY: usize = 90;

/// A named measurement with a short history.
#[derive(Debug, Clone)]
pub struct Series {
    pub label: &'static str,
    samples: VecDeque<f32>,
}

impl Series {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            samples: VecDeque::with_capacity(HISTORY),
        }
    }

    pub fn push(&mut self, value: f32) {
        if self.samples.len() == HISTORY {
            self.samples.pop_front();
        }
        self.samples.push_back(value.clamp(0.0, 1.0));
    }

    pub fn latest(&self) -> f32 {
        self.samples.back().copied().unwrap_or(0.0)
    }

    pub fn samples(&self) -> impl Iterator<Item = f32> + '_ {
        self.samples.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Kept beside `len` because clippy asks for the pair and a reader expects it, even
    /// though only `len` is called today.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Cumulative CPU jiffies, as `/proc/stat` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTotals {
    pub busy: u64,
    pub total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
///
/// Fields are user, nice, system, idle, iowait, irq, softirq, steal, guest, guest_nice. Idle
/// and iowait are the only ones that are *not* work — counting iowait as busy is the classic
/// mistake here and makes a machine waiting on disk look pegged.
pub fn parse_cpu(text: &str) -> Option<CpuTotals> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 5 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let idle = values[3] + values[4];
    Some(CpuTotals {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Fraction of memory in use, from `/proc/meminfo`.
///
/// `MemAvailable` rather than `MemFree`: free memory on Linux is nearly always small because
/// the kernel uses the rest for cache, and reporting it would show a healthy machine as
/// permanently full.
pub fn parse_memory(text: &str) -> Option<f32> {
    let field = |name: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    if total <= 0.0 {
        return None;
    }
    Some(((total - available) / total) as f32)
}

/// Two rates that belong together, in bytes a second, with a short history.
///
/// A pair rather than two [`Series`], because they share one graph and one scale: download and
/// upload drawn against different ceilings would make a trickle of one look as big as a flood
/// of the other. And bytes rather than a fraction, because a link or a disk has no natural
/// "100%" to divide by -- the graph scales to what the history has actually seen.
#[derive(Debug, Clone)]
pub struct Rates {
    pub label: &'static str,
    /// What each half is called in the reading, in the order they are pushed.
    pub names: [&'static str; 2],
    samples: VecDeque<[f32; 2]>,
}

/// The least a rate graph is scaled to, in bytes a second.
///
/// Without a floor an idle link scales its own background chatter to full height, and a panel
/// that shows a few hundred bytes of mDNS as a wall of bars says "busy" when nothing is.
pub const RATE_FLOOR: f32 = 256.0 * 1024.0;

impl Rates {
    pub fn new(label: &'static str, names: [&'static str; 2]) -> Self {
        Self {
            label,
            names,
            samples: VecDeque::with_capacity(HISTORY),
        }
    }

    pub fn push(&mut self, value: [f32; 2]) {
        if self.samples.len() == HISTORY {
            self.samples.pop_front();
        }
        let clean = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        self.samples.push_back([clean(value[0]), clean(value[1])]);
    }

    pub fn latest(&self) -> [f32; 2] {
        self.samples.back().copied().unwrap_or([0.0, 0.0])
    }

    pub fn samples(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        self.samples.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// The top of the graph: the most either half has reached in the history shown.
    pub fn scale(&self) -> f32 {
        self.samples
            .iter()
            .flat_map(|pair| pair.iter().copied())
            .fold(RATE_FLOOR, f32::max)
    }
}

/// A rate as a person reads it: `740 KB/s`, `12 MB/s`, `1.4 MB/s`.
///
/// Binary units, as every file manager and `iftop` show them. Two significant figures below
/// ten and none above, so the reading does not flicker in its last digit once a second.
pub fn format_rate(bytes_per_second: f32) -> String {
    let b = bytes_per_second.max(0.0);
    const K: f32 = 1024.0;
    let scaled = |v: f32, unit: &str| {
        if v < 9.95 {
            format!("{v:.1} {unit}")
        } else {
            format!("{v:.0} {unit}")
        }
    };
    if b < K {
        format!("{b:.0} B/s")
    } else if b < K * K {
        format!("{:.0} KB/s", b / K)
    } else if b < K * K * K {
        scaled(b / (K * K), "MB/s")
    } else {
        scaled(b / (K * K * K), "GB/s")
    }
}

/// Bytes received and sent, summed over the interfaces `counts` accepts, from `/proc/net/dev`.
///
/// Which interfaces count is the caller's business. Summing all of them double-counts: a
/// packet sent over Tailscale appears once on `tailscale0` and again, wrapped, on `wlan0`,
/// and loopback carries nothing that ever left the machine.
pub fn parse_net(text: &str, counts: impl Fn(&str) -> bool) -> Option<[u64; 2]> {
    let mut total = [0u64; 2];
    let mut any = false;
    // The first two lines are column headings. Each interface is `name: rx... tx...`, where
    // a large counter can run straight into the colon, so split there rather than on space.
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !counts(name) {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        // Receive bytes first, transmit bytes ninth: eight receive columns, then transmit.
        if fields.len() < 9 {
            continue;
        }
        total[0] += fields[0];
        total[1] += fields[8];
        any = true;
    }
    any.then_some(total)
}

/// Bytes read and written, summed over the disks `counts` accepts, from `/proc/diskstats`.
///
/// The kernel counts in 512-byte sectors here whatever the device's real sector size is.
/// Partitions have to be left out by the caller, or every byte is counted once for the disk
/// and again for the partition it landed on.
pub fn parse_disk(text: &str, counts: impl Fn(&str) -> bool) -> Option<[u64; 2]> {
    let mut total = [0u64; 2];
    let mut any = false;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // major minor name, reads merged sectors-read ms, writes merged sectors-written ...
        if fields.len() < 10 || !counts(fields[2]) {
            continue;
        }
        let (Ok(read), Ok(written)) = (fields[5].parse::<u64>(), fields[9].parse::<u64>()) else {
            continue;
        };
        total[0] += read * 512;
        total[1] += written * 512;
        any = true;
    }
    any.then_some(total)
}

/// Everything the sidecar shows, sampled on a timer.
pub struct Monitors {
    pub cpu: Series,
    pub gpu: Series,
    pub memory: Series,
    /// Received and sent, over the machine's real network hardware.
    pub network: Rates,
    /// Read and written, over its real disks.
    pub disk: Rates,
    previous_cpu: CpuTotals,
    previous_net: Option<[u64; 2]>,
    previous_disk: Option<[u64; 2]>,
    last_sample: std::time::Instant,
}

impl Default for Monitors {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitors {
    pub fn new() -> Self {
        Self {
            cpu: Series::new("CPU"),
            gpu: Series::new("GPU"),
            memory: Series::new("MEM"),
            network: Rates::new("NET", ["\u{2193}", "\u{2191}"]),
            disk: Rates::new("DISK", ["R", "W"]),
            previous_cpu: read_cpu().unwrap_or_default(),
            previous_net: read_net(),
            previous_disk: read_disk(),
            last_sample: std::time::Instant::now(),
        }
    }

    /// Take a reading if enough time has passed. Returns whether anything changed.
    pub fn tick(&mut self) -> bool {
        let elapsed = self.last_sample.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return false;
        }
        self.last_sample = std::time::Instant::now();

        // Per second of the interval actually measured, not per tick. The loop that calls this
        // can stall for a moment, and dividing a 1.4 s interval's bytes by one second would
        // show a spike that never happened.
        let seconds = elapsed.as_secs_f32();
        let rate = |now: Option<[u64; 2]>, before: &mut Option<[u64; 2]>| -> [f32; 2] {
            let rates = match (now, *before) {
                // Saturating, because a counter goes backwards when an interface is replaced
                // or a disk removed, and that is not a negative rate.
                (Some(n), Some(b)) => [0, 1].map(|i| n[i].saturating_sub(b[i]) as f32 / seconds),
                _ => [0.0, 0.0],
            };
            *before = now;
            rates
        };
        self.network.push(rate(read_net(), &mut self.previous_net));
        self.disk.push(rate(read_disk(), &mut self.previous_disk));

        if let Some(now) = read_cpu() {
            let busy = now.busy.saturating_sub(self.previous_cpu.busy) as f32;
            let total = now.total.saturating_sub(self.previous_cpu.total) as f32;
            // A zero interval happens if the clock jumps; reporting 0% is better than NaN,
            // which propagates into the graph and draws nothing at all.
            self.cpu.push(if total > 0.0 { busy / total } else { 0.0 });
            self.previous_cpu = now;
        }
        self.gpu.push(read_gpu_busy().unwrap_or(0.0));
        self.memory.push(read_memory().unwrap_or(0.0));
        true
    }
}

fn read_cpu() -> Option<CpuTotals> {
    parse_cpu(&std::fs::read_to_string("/proc/stat").ok()?)
}

fn read_memory() -> Option<f32> {
    parse_memory(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Only interfaces backed by hardware, which is what `device` under sysfs means. That leaves
/// out loopback, Tailscale, bridges and VPNs, all of which carry traffic that is also counted
/// on the real interface underneath.
fn read_net() -> Option<[u64; 2]> {
    parse_net(&std::fs::read_to_string("/proc/net/dev").ok()?, |name| {
        std::path::Path::new("/sys/class/net")
            .join(name)
            .join("device")
            .exists()
    })
}

/// Only whole physical disks: those under `/sys/block` with a device behind them. Partitions
/// are not listed there, and loop, zram and device-mapper nodes have no `device`.
fn read_disk() -> Option<[u64; 2]> {
    parse_disk(&std::fs::read_to_string("/proc/diskstats").ok()?, |name| {
        std::path::Path::new("/sys/block")
            .join(name)
            .join("device")
            .exists()
    })
}

/// GPU utilisation, if the driver publishes it. amdgpu does; many do not.
fn read_gpu_busy() -> Option<f32> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let path = entry.path().join("device/gpu_busy_percent");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(percent) = text.trim().parse::<f32>() {
                return Some((percent / 100.0).clamp(0.0, 1.0));
            }
        }
    }
    None
}

/// Screen brightness as a fraction, and the file to write to change it.
pub struct Backlight {
    pub path: std::path::PathBuf,
    pub max: u32,
}

impl Backlight {
    pub fn find() -> Option<Self> {
        let entries = std::fs::read_dir("/sys/class/backlight").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let max = std::fs::read_to_string(path.join("max_brightness"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            return Some(Self {
                path: path.join("brightness"),
                max,
            });
        }
        None
    }

    pub fn level(&self) -> Option<f32> {
        let value: u32 = std::fs::read_to_string(&self.path)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(value as f32 / self.max.max(1) as f32)
    }

    /// Writing here needs the file to be writable by the session, which it is on SteamOS via
    /// a udev rule. Failure is reported rather than swallowed so the caller can say so.
    pub fn set(&self, level: f32) -> std::io::Result<()> {
        let value = (level.clamp(0.0, 1.0) * self.max as f32).round() as u32;
        std::fs::write(&self.path, value.to_string())
    }
}

/// Output volume, via PipeWire's own tool.
///
/// Shelling out rather than speaking to PipeWire directly: the wire protocol is a large
/// dependency for one number, and `wpctl` is part of the same package as the daemon, so if
/// there is audio at all this exists.
pub fn volume() -> Option<f32> {
    let out = std::process::Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    parse_volume(&String::from_utf8_lossy(&out.stdout))
}

/// Set the output volume, as a fraction.
///
/// Capped at 1.0 rather than passing the slider's value straight through. `wpctl` will happily
/// go above unity, and a slider whose right-hand end is 150% has most of its travel in the
/// range where the Deck's speakers distort.
pub fn set_volume(level: f32) {
    let level = level.clamp(0.0, 1.0);
    let result = std::process::Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{level:.3}")])
        .status();
    match result {
        Ok(status) if !status.success() => log::warn!("wpctl set-volume failed: {status}"),
        Err(e) => log::warn!("could not run wpctl: {e}"),
        _ => {}
    }
}

/// Mute or unmute the output.
pub fn set_muted(muted: bool) {
    wpctl_mute(if muted { "1" } else { "0" });
}

/// Flip the output's mute, whichever way it is.
pub fn toggle_mute() {
    wpctl_mute("toggle");
}

fn wpctl_mute(how: &str) {
    let result = std::process::Command::new("wpctl")
        .args(["set-mute", "@DEFAULT_AUDIO_SINK@", how])
        .status();
    match result {
        Ok(status) if !status.success() => log::warn!("wpctl set-mute {how} failed: {status}"),
        Err(e) => log::warn!("could not run wpctl: {e}"),
        _ => {}
    }
}

/// `wpctl` prints `Volume: 0.45` or `Volume: 0.45 [MUTED]`.
pub fn parse_volume(text: &str) -> Option<f32> {
    let value = text.split_whitespace().nth(1)?;
    value.parse::<f32>().ok().map(|v| v.clamp(0.0, 1.5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_busy_excludes_idle_and_iowait() {
        // Counting iowait as work is the classic error and makes a machine waiting on disk
        // look pegged at 100%.
        let text = "cpu  100 20 30 1000 50 5 5 0 0 0\ncpu0 1 2 3 4 5\n";
        let t = parse_cpu(text).expect("parses");
        assert_eq!(t.total, 100 + 20 + 30 + 1000 + 50 + 5 + 5);
        assert_eq!(t.busy, t.total - 1050, "idle and iowait are not work");
    }

    #[test]
    fn a_truncated_cpu_line_is_rejected_rather_than_half_read() {
        assert!(parse_cpu("cpu  1 2\n").is_none());
        assert!(parse_cpu("something else\n").is_none());
    }

    #[test]
    fn memory_uses_available_not_free() {
        // Free memory on Linux is nearly always small because the kernel caches with the rest;
        // reporting it shows a healthy machine as permanently full.
        let text = "MemTotal:       15160360 kB\nMemFree:          200000 kB\n\
                    MemAvailable:   12625676 kB\n";
        let used = parse_memory(text).expect("parses");
        assert!((used - 0.167).abs() < 0.01, "got {used}");
    }

    #[test]
    fn memory_survives_a_missing_field() {
        assert!(parse_memory("MemTotal: 100 kB\n").is_none());
        assert!(parse_memory("").is_none());
    }

    #[test]
    fn volume_is_read_from_wpctls_output() {
        assert_eq!(parse_volume("Volume: 0.45\n"), Some(0.45));
        assert_eq!(parse_volume("Volume: 0.45 [MUTED]\n"), Some(0.45));
        assert_eq!(parse_volume("nonsense"), None);
    }

    /// `/proc/net/dev` as a Deck on Wi-Fi with Tailscale up prints it, including a receive
    /// counter large enough to run into its colon.
    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  901234     100    0    0    0     0          0         0   901234     100    0    0    0     0       0          0
 wlan0:12345678901 9000    0    0    0     0          0         0  5550000    4000    0    0    0     0       0          0
tailscale0: 3000000   2000    0    0    0     0          0         0  1000000    1000    0    0    0     0       0          0
";

    #[test]
    fn network_traffic_is_counted_once_on_the_real_interface() {
        let only_wlan = parse_net(NET_DEV, |name| name == "wlan0");
        assert_eq!(only_wlan, Some([12_345_678_901, 5_550_000]));
    }

    #[test]
    fn no_real_interface_is_no_reading_rather_than_zero() {
        assert_eq!(parse_net(NET_DEV, |_| false), None);
    }

    const DISKSTATS: &str = "\
 259       0 nvme0n1 5000 10 400000 900 3000 20 200000 800 0 1000 1700 0 0 0 0 0 0
 259       1 nvme0n1p1 100 0 8000 10 50 0 4000 5 0 20 15 0 0 0 0 0 0
   7       0 loop0 20 0 160 1 0 0 0 0 0 1 1 0 0 0 0 0 0
";

    #[test]
    fn a_disk_is_counted_in_bytes_and_its_partitions_are_not_counted_again() {
        let whole = parse_disk(DISKSTATS, |name| name == "nvme0n1");
        assert_eq!(whole, Some([400_000 * 512, 200_000 * 512]));
    }

    #[test]
    fn a_rate_reads_the_way_a_person_says_it() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(740.0 * 1024.0), "740 KB/s");
        assert_eq!(format_rate(1.44 * 1024.0 * 1024.0), "1.4 MB/s");
        assert_eq!(format_rate(12.3 * 1024.0 * 1024.0), "12 MB/s");
    }

    #[test]
    fn an_idle_link_is_not_drawn_as_a_busy_one() {
        let mut rates = Rates::new("NET", ["d", "u"]);
        rates.push([300.0, 40.0]);
        assert_eq!(rates.scale(), RATE_FLOOR, "background chatter must not fill the graph");
        rates.push([4.0e6, 1.0e5]);
        assert_eq!(rates.scale(), 4.0e6);
        rates.push([f32::NAN, -5.0]);
        assert_eq!(rates.latest(), [0.0, 0.0]);
    }

    #[test]
    fn a_series_keeps_a_bounded_history() {
        // Unbounded, this grows for the life of the session and the graph's horizontal scale
        // changes every second.
        let mut s = Series::new("x");
        for i in 0..(HISTORY * 3) {
            s.push(i as f32 / (HISTORY * 3) as f32);
        }
        assert_eq!(s.len(), HISTORY);
        assert!(
            s.latest() > 0.9,
            "the newest sample should be the last pushed"
        );
    }

    #[test]
    fn a_series_clamps_out_of_range_readings() {
        // A driver reporting 101% or a negative delta would otherwise draw outside the graph.
        let mut s = Series::new("x");
        s.push(4.0);
        assert_eq!(s.latest(), 1.0);
        s.push(-2.0);
        assert_eq!(s.latest(), 0.0);
    }

    #[test]
    fn an_empty_series_reads_as_zero_rather_than_panicking() {
        assert_eq!(Series::new("x").latest(), 0.0);
        assert!(Series::new("x").is_empty());
    }
}

// --- audio devices ---

/// Which end of the audio path a device sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Output,
    Input,
}

impl Direction {
    pub const ALL: [Direction; 2] = [Direction::Output, Direction::Input];

    pub fn label(self) -> &'static str {
        match self {
            Direction::Output => "Output",
            Direction::Input => "Input",
        }
    }

    /// What `wpctl status` calls this section.
    fn heading(self) -> &'static str {
        match self {
            Direction::Output => "Sinks:",
            Direction::Input => "Sources:",
        }
    }
}

/// Somewhere sound can come from or go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// PipeWire's node id, which is what `wpctl` takes. Not stable across a replug, which is
    /// why the list is re-read rather than cached.
    pub id: u32,
    /// Shortened for a panel read at arm's length. See [`friendly_name`].
    pub name: String,
    pub is_default: bool,
}

/// Everything currently available, in both directions.
///
/// Re-read rather than watched. A proper subscription would mean speaking the PipeWire wire
/// protocol, which is a large dependency for a list that changes when somebody plugs in a
/// headset — and the volume worker already reads the volume on a timer, so this costs one more
/// process on a tick that was happening anyway. Plugging in a USB or Bluetooth device shows up
/// on the next tick, which is what "updates when something is connected" needs to mean here.
///
/// Both directions from one `wpctl status`. It takes 30 ms on the Deck, and asking once per
/// direction was paying that twice for the same text.
pub fn audio_devices() -> crate::sidecar::Audio {
    let Ok(out) = std::process::Command::new("wpctl").arg("status").output() else {
        return crate::sidecar::Audio::default();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut inputs = parse_devices(&text, Direction::Input);
    // The default may be a node the list hides on purpose; see `default_loopback`. Asking
    // costs one more process every two seconds, and only while that is the case.
    if !inputs.iter().any(|device| device.is_default) {
        if let Some(hidden) = default_loopback(&text) {
            mark_default_behind(&mut inputs, hidden);
        }
    }
    crate::sidecar::Audio {
        outputs: parse_devices(&text, Direction::Output),
        inputs,
    }
}

/// What a sound server calls its loopback copy of a capture device.
///
/// See [`recordable`], which is where this earns its keep.
const LOOPBACK: &str = "alsa_loopback_device.";

/// The node that can actually be recorded from, which is not always the one chosen.
///
/// **On this machine the real capture nodes deliver nothing.** Both microphones appear twice:
/// once as the ALSA device, `alsa_input.usb-…Air…mono-fallback`, and once as a loopback copy
/// of it under `Filters:`. Recording from the ALSA node yields a header and not one frame of
/// sound, every time, from either microphone:
///
/// ```text
///        0 bytes  <- alsa_input.usb-Vendor_Air_…mono-fallback
///   563200 bytes  <- alsa_loopback_device.alsa_input.usb-Vendor_Air_…mono-fallback
///        0 bytes  <- alsa_input.pci-…HiFi__Internal_Mic__source
///   563200 bytes  <- alsa_loopback_device.alsa_input.pci-…HiFi__Internal_Mic__source
/// ```
///
/// Neither is muted and both sit in the same state; the copy is simply what this system
/// intends clients to use, which is why the sound server's own default pointed at one before
/// anybody touched it. The picker lists the ALSA nodes because those are what carries a
/// readable name — "Air Mono" rather than a bus path — so choosing "Glasses Microphone" used
/// to set the default to a device that cannot be recorded from, and every microphone on the
/// machine went quiet. That is what this is for.
///
/// It is self-limiting: on a machine with no such copy nothing matches and the chosen device
/// is used unchanged. A sink has no loopback twin either, so an output passes straight
/// through.
fn recordable(id: u32) -> u32 {
    let Some(name) = node_name(id) else { return id };
    let Ok(out) = std::process::Command::new("wpctl").arg("status").output() else {
        return id;
    };
    let twin = find_node(&String::from_utf8_lossy(&out.stdout), &format!("{LOOPBACK}{name}"));
    match twin {
        Some(twin) => {
            log::info!("audio: recording from {LOOPBACK}{name} rather than the device itself");
            twin
        }
        None => id,
    }
}

/// What the sound server calls a node, as opposed to what it shows a person.
fn node_name(id: u32) -> Option<String> {
    property(id, "node.name")
}

/// One of a node's properties, asked for by name.
fn property(id: u32, key: &str) -> Option<String> {
    let out = std::process::Command::new("wpctl")
        .args(["inspect", &id.to_string()])
        .output()
        .ok()?;
    property_in(&String::from_utf8_lossy(&out.stdout), key)
}

/// Pull `<key> = "…"` out of what `wpctl inspect` printed.
pub fn property_in(text: &str, key: &str) -> Option<String> {
    let wanted = format!("{key} = ");
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['*', ' ']);
        if let Some(value) = line.strip_prefix(&wanted) {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The default device, when it is one of the copies the list deliberately hides.
///
/// [`parse_devices`] skips the `Filters:` block because it is plumbing, and [`recordable`]
/// makes the default a node inside it. Both are right, and together they left the panel with
/// no device marked at all: the wearer picked a microphone, the dot appeared, the list was
/// read again two seconds later and nothing in it was the default any more. This is the other
/// half — it finds the hidden node so the device standing in front of it can be marked.
pub fn default_loopback(text: &str) -> Option<u32> {
    for line in text.lines() {
        let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']).trim();
        let Some(rest) = trimmed.strip_prefix('*') else {
            continue;
        };
        let Some((id, name)) = rest.trim_start().split_once('.') else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else { continue };
        if name.trim_start().starts_with(LOOPBACK) {
            return Some(id);
        }
    }
    None
}

/// Mark the device that stands in front of `hidden`, matched on the name they share.
///
/// A copy carries the same `node.description` as the device it copies — "Air Mono" for both —
/// which is what makes this a lookup rather than a guess.
fn mark_default_behind(devices: &mut [AudioDevice], hidden: u32) {
    let Some(description) = property(hidden, "node.description") else {
        return;
    };
    let name = friendly_name(&description);
    if let Some(device) = devices.iter_mut().find(|device| device.name == name) {
        device.is_default = true;
    }
}

/// The id of the node with exactly this name, from anywhere in `wpctl status`.
///
/// Anywhere, deliberately: the node wanted here is under `Filters:`, which [`parse_devices`]
/// goes out of its way to skip because it is plumbing. It is still plumbing. It is just
/// plumbing that has to be named when a device is chosen.
pub fn find_node(text: &str, name: &str) -> Option<u32> {
    for line in text.lines() {
        let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']).trim();
        let trimmed = trimmed.strip_prefix('*').map_or(trimmed, str::trim_start);
        let Some((id, rest)) = trimmed.split_once('.') else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else { continue };
        if rest.split('[').next().unwrap_or(rest).trim() == name {
            return Some(id);
        }
    }
    None
}

/// Make a device the default, and move anything already playing over to it.
///
/// Setting the default alone would leave whatever is currently making sound still pointed at
/// the old device, so picking "Glasses" mid-video would do nothing audible until the next
/// thing started. `wpctl` has no "move existing streams" of its own, so the default is set and
/// the sound server's own rescan is relied on — which is what the desktop's own picker does.
///
/// What is made default is not always what was picked; see [`recordable`].
pub fn set_default_device(id: u32) {
    let id = recordable(id);
    let result = std::process::Command::new("wpctl")
        .args(["set-default", &id.to_string()])
        .status();
    match result {
        Ok(status) if !status.success() => log::warn!("wpctl set-default {id} failed: {status}"),
        Err(e) => log::warn!("could not run wpctl: {e}"),
        _ => {}
    }
}

/// Pull one section out of `wpctl status`.
///
/// The output is a drawn tree meant for a person, so this is parsing a human-readable format,
/// which is worth being uneasy about. The alternatives are worse: `pw-dump` is JSON but needs
/// a JSON dependency and describes the whole graph rather than the two lists wanted here, and
/// the wire protocol is an enormous amount of surface for a picker. So it is parsed
/// defensively — anything that does not look like a numbered entry is skipped rather than
/// guessed at — and pinned by tests against real captured output.
///
/// A section ends at the next line whose tree characters step back out, which is how a device
/// list is told from the `Filters:` block that follows it. That block matters: on this machine
/// the *default source* is a loopback filter rather than anything under `Sources:`, and
/// treating filters as devices would offer the wearer a list of plumbing.
///
/// Our own per-window sinks are dropped for the same reason. Each window that makes a sound
/// has one — see [`crate::audio`] — and they are real sinks that a sound server will happily
/// list, so without this the output picker fills up with a row per window, all of them named
/// the same thing, none of them somewhere a person wants their sound to go. They are plumbing,
/// and this is where the plumbing is hidden.
pub fn parse_devices(text: &str, direction: Direction) -> Vec<AudioDevice> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']);
        if trimmed.starts_with(direction.heading()) {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        // Any other section heading ends this one.
        if trimmed.ends_with(':') && !trimmed.is_empty() {
            break;
        }
        let Some(device) = parse_device_line(trimmed) else {
            continue;
        };
        if is_our_own_plumbing(&device.name) {
            continue;
        }
        out.push(device);
    }
    out
}

/// Is this one of the sinks Spatiand made for itself?
///
/// Matched on the description a window's sink is given, which is the only thing `wpctl status`
/// prints. It is a fixed string set in one place — `spatiand_audio::server` — so this is a
/// comparison against a constant rather than a guess about names.
fn is_our_own_plumbing(name: &str) -> bool {
    name.trim() == spatiand_audio::server::SINK_DESCRIPTION
}

/// One `  *   81. Air Analog Stereo   [vol: 0.32]` line.
fn parse_device_line(text: &str) -> Option<AudioDevice> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // The asterisk marks the default, and it sits before the id rather than after it.
    let (is_default, rest) = match text.strip_prefix('*') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let (id, name) = rest.split_once('.')?;
    let id = id.trim().parse::<u32>().ok()?;
    // Everything from `[vol:` onward is a reading, not a name.
    let name = name.split('[').next().unwrap_or(name).trim();
    if name.is_empty() {
        return None;
    }
    Some(AudioDevice {
        id,
        name: friendly_name(name),
        is_default,
    })
}

/// Turn a driver's name for a device into one a person would use.
///
/// PipeWire reports what the kernel calls the hardware, which on this machine means
/// "ACP/ACP3X/ACP6x Audio Coprocessor Speaker" — accurate, and useless on a panel four inches
/// wide. The wearer asked for the glasses, the Deck and a headset to be recognisable, and
/// these are the substitutions that make them so.
///
/// Substring rules rather than a lookup by id, because ids are not stable across a replug.
/// Anything unmatched passes through with its whitespace tidied, so an unknown USB interface
/// still appears — under an ugly name, which is better than not appearing.
pub fn friendly_name(raw: &str) -> String {
    // Longest first: "Internal Microphone" has to be replaced before "Microphone" would be.
    const RULES: [(&str, &str); 8] = [
        ("ACP/ACP3X/ACP6x Audio Coprocessor", "Deck"),
        ("Internal Microphone", "Microphone"),
        ("Analog Stereo", ""),
        ("Digital Stereo", ""),
        // The glasses present as an ALSA card called "Air". Matched on the word so it cannot
        // eat the "air" inside another device's name.
        ("Air Mono", "Glasses Microphone"),
        ("Air", "Glasses"),
        ("Headphones", "Headphones"),
        ("Mono", ""),
    ];
    let mut name = raw.to_string();
    for (from, to) in RULES {
        if name.contains(from) {
            name = name.replace(from, to);
        }
    }
    // Collapse whatever runs of whitespace the substitutions left behind.
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        raw.trim().to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn the_microphone_that_can_be_recorded_from_is_found_behind_the_one_that_is_shown() {
        // The wearer picks "Glasses Microphone", which is node 50. Recording from 50 yields
        // nothing on this machine; 60, its loopback copy, is where the sound is. The `*` in
        // front of 60 is there to be tripped over -- it marks the default, and it sits before
        // the id.
        let twin = find_node(
            REAL,
            "alsa_loopback_device.alsa_input.usb-Vendor_Air_A00011_32_00-00.mono-fallback",
        );
        assert_eq!(twin, Some(60));
    }

    #[test]
    fn a_device_with_no_copy_of_itself_is_left_alone() {
        // Nothing matches, and the answer is that nothing matches -- not the first line that
        // happens to have a number in it. A machine that arranges its microphones normally
        // must come through this unchanged.
        assert_eq!(find_node(REAL, "alsa_loopback_device.alsa_input.nonesuch"), None);
        assert_eq!(find_node(REAL, "Air Mono"), Some(50));
    }

    #[test]
    fn the_hidden_default_is_found_so_the_panel_can_show_one() {
        // The wearer's complaint, as a test: no device in the list carries the `*`, because
        // the default is node 60 down in `Filters:`. Without finding it the panel shows a
        // dot that appears when a microphone is picked and vanishes on the next read.
        let sources = parse_devices(REAL, Direction::Input);
        assert!(
            !sources.iter().any(|device| device.is_default),
            "nothing in the list is marked, which is the whole problem"
        );
        assert_eq!(default_loopback(REAL), Some(60));
    }

    #[test]
    fn an_ordinary_default_is_not_mistaken_for_a_hidden_one() {
        // A machine that marks a real device needs no rescue, and an output default -- node
        // 81 in this capture -- must not be read as one either.
        assert!(parse_devices(REAL, Direction::Output).iter().any(|d| d.is_default));
        let plain = REAL.replace(
            "*   60. alsa_loopback_device",
            "    60. alsa_loopback_device",
        );
        assert_eq!(default_loopback(&plain), None);
    }

    #[test]
    fn a_nodes_real_name_is_read_from_what_inspect_prints() {
        const INSPECT: &str = r#"id 215, type PipeWire:Interface:Node
    alsa.card = "1"
  * media.class = "Audio/Source"
  * node.name = "alsa_input.usb-Vendor_Air_A00011_32_00-00.mono-fallback"
"#;
        assert_eq!(
            property_in(INSPECT, "node.name").as_deref(),
            Some("alsa_input.usb-Vendor_Air_A00011_32_00-00.mono-fallback")
        );
        assert_eq!(property_in("id 4, type PipeWire:Interface:Node\n", "node.name"), None);
    }

    /// Captured verbatim from the Deck with the glasses plugged in, because a parser for a
    /// human-readable format is only ever as good as the samples it was written against.
    const REAL: &str = r#"PipeWire 'pipewire-0' [1.6.4, deck@steamdeck, cookie:727404770]
 └─ Clients:
        32. WirePlumber                         [1.6.4, deck@steamdeck, pid:1392]
       106. wpctl                               [1.6.4, deck@steamdeck, pid:47032]

Audio
 ├─ Devices:
 │      69. Rembrandt Radeon High Definition Audio Controller [alsa]
 │      95. Air                                 [alsa]
 │      99. ACP/ACP3X/ACP6x Audio Coprocessor   [alsa]
 │  
 ├─ Sinks:
 │      62. ACP/ACP3X/ACP6x Audio Coprocessor Headphones [vol: 1.00]
 │      66. ACP/ACP3X/ACP6x Audio Coprocessor Speaker [vol: 0.24]
 │  *   81. Air Analog Stereo                   [vol: 0.32]
 │  
 ├─ Sources:
 │      50. Air Mono                            [vol: 1.00]
 │      72. ACP/ACP3X/ACP6x Audio Coprocessor Internal Microphone [vol: 0.78]
 │  
 ├─ Filters:
 │    - loopback-1392-18                                            
 │  *   60. alsa_loopback_device.alsa_input.usb-Vendor_Air_A00011_32_00-00.mono-fallback [Audio/Source]
 │  
 └─ Streams:

Video
 ├─ Devices:
 │  
 ├─ Sinks:
 │  
"#;

    #[test]
    fn finds_every_output_the_deck_actually_offers() {
        let sinks = parse_devices(REAL, Direction::Output);
        assert_eq!(sinks.len(), 3, "{sinks:#?}");
        assert_eq!(sinks.iter().map(|d| d.id).collect::<Vec<_>>(), [62, 66, 81]);
        assert_eq!(sinks[2].name, "Glasses");
        assert!(sinks[2].is_default, "the asterisk marks the one in use");
        assert!(!sinks[0].is_default && !sinks[1].is_default);
    }

    #[test]
    fn finds_every_input() {
        let sources = parse_devices(REAL, Direction::Input);
        assert_eq!(sources.len(), 2, "{sources:#?}");
        assert_eq!(sources[0].name, "Glasses Microphone");
        assert_eq!(sources[1].name, "Deck Microphone");
    }

    #[test]
    fn a_sections_devices_do_not_leak_into_the_next() {
        // The failure this guards: `Sources:` is followed by `Filters:`, and on this machine
        // the filter list contains an entry marked default. Running past the end of a section
        // would offer the wearer a loopback node called
        // "alsa_loopback_device.alsa_input.usb-Vendor_Air..." as though it were a microphone.
        for direction in Direction::ALL {
            for device in parse_devices(REAL, direction) {
                assert!(
                    !device.name.contains("loopback"),
                    "{direction:?} picked up a filter: {device:?}"
                );
            }
        }
    }

    #[test]
    fn the_deck_speakers_are_named_for_the_deck() {
        // The wearer's requirement, quite literally: the glasses, the Deck and a headset have
        // to be recognisable at a glance.
        let sinks = parse_devices(REAL, Direction::Output);
        let names: Vec<&str> = sinks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Deck Headphones", "Deck Speaker", "Glasses"]);
    }

    #[test]
    fn an_unknown_device_still_appears_under_its_own_name() {
        // A USB interface or a Bluetooth speaker nobody wrote a rule for must still be
        // offered. Hiding what we cannot prettify would make the picker silently incomplete,
        // which is the one thing a picker must never be.
        let text = " ├─ Sinks:\n │      90. Jabra Evolve2 65 \n │  \n ├─ Sources:\n";
        let sinks = parse_devices(text, Direction::Output);
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "Jabra Evolve2 65");
    }

    #[test]
    fn nothing_at_all_is_not_an_error() {
        // No sound server, or a machine mid-boot. An empty list means the panel shows no
        // devices; it must not mean a panic on a screen nobody can get back from.
        assert!(parse_devices("", Direction::Output).is_empty());
        assert!(parse_devices("Audio\n ├─ Sinks:\n │  \n", Direction::Output).is_empty());
    }

    #[test]
    fn a_name_that_is_only_noise_keeps_its_original() {
        // "Air Mono" maps to a real name, but a device called just "Mono" would be erased
        // entirely by the tidying rules. Better an odd name than a blank row.
        assert_eq!(friendly_name("Mono"), "Mono");
        assert_eq!(friendly_name("Analog Stereo"), "Analog Stereo");
    }

    #[test]
    fn tidying_does_not_leave_double_spaces_behind() {
        // Removing a word from the middle of a name leaves two spaces where one belongs, and
        // the gap is obvious in a proportional font.
        let name = friendly_name("Air Analog Stereo");
        assert!(!name.contains("  "), "{name:?}");
        assert_eq!(name, "Glasses");
    }

    #[test]
    fn a_windows_own_sink_is_not_offered_as_somewhere_to_send_sound() {
        // Every window that makes a sound has one of these. They are real sinks and a sound
        // server lists them, so without filtering the picker fills with a row per window, all
        // named the same, none of them anywhere a person wants their sound to go.
        let text = "\
Audio
 ├─ Sinks:
 │  *   92. Air Analog Stereo                   [vol: 0.63]
 │      96. ACP/ACP3X/ACP6x Audio Coprocessor Speaker [vol: 0.30]
 │     166. Spatiand window                     [vol: 1.00]
 │     201. Spatiand window                     [vol: 1.00]
 │  
 ├─ Sources:
";
        let devices = parse_devices(text, Direction::Output);
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("Spatiand window")),
            "our own plumbing was offered: {names:?}"
        );
        // And the real ones survive, which is the half that would be easy to break.
        assert_eq!(devices.len(), 2, "{names:?}");
        assert!(devices[0].is_default);
    }
}
