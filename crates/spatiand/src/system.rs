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

/// Everything the sidecar shows, sampled on a timer.
pub struct Monitors {
    pub cpu: Series,
    pub gpu: Series,
    pub memory: Series,
    previous_cpu: CpuTotals,
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
            previous_cpu: read_cpu().unwrap_or_default(),
            last_sample: std::time::Instant::now(),
        }
    }

    /// Take a reading if enough time has passed. Returns whether anything changed.
    pub fn tick(&mut self) -> bool {
        if self.last_sample.elapsed() < std::time::Duration::from_secs(1) {
            return false;
        }
        self.last_sample = std::time::Instant::now();

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
        let value: u32 = std::fs::read_to_string(&self.path).ok()?.trim().parse().ok()?;
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

    #[test]
    fn a_series_keeps_a_bounded_history() {
        // Unbounded, this grows for the life of the session and the graph's horizontal scale
        // changes every second.
        let mut s = Series::new("x");
        for i in 0..(HISTORY * 3) {
            s.push(i as f32 / (HISTORY * 3) as f32);
        }
        assert_eq!(s.len(), HISTORY);
        assert!(s.latest() > 0.9, "the newest sample should be the last pushed");
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
