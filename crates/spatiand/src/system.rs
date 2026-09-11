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
    crate::sidecar::Audio {
        outputs: parse_devices(&text, Direction::Output),
        inputs: parse_devices(&text, Direction::Input),
    }
}

/// Make a device the default, and move anything already playing over to it.
///
/// Setting the default alone would leave whatever is currently making sound still pointed at
/// the old device, so picking "Glasses" mid-video would do nothing audible until the next
/// thing started. `wpctl` has no "move existing streams" of its own, so the default is set and
/// the sound server's own rescan is relied on — which is what the desktop's own picker does.
pub fn set_default_device(id: u32) {
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
