//! Persisting the axis map.
//!
//! Stored at `$XDG_CONFIG_HOME/spatiand/axes.toml` (falling back to `~/.config`), which on
//! SteamOS lives on `/home` and therefore survives OS updates — the calibration is a
//! per-user, per-device measurement and should not have to be redone because Valve shipped a
//! new image.
//!
//! A stored map from an older convention is **discarded rather than migrated**. Version 1
//! folded signs into a left-handed frame, and silently reinterpreting it would produce a
//! mirrored map, whose symptoms look like filter drift rather than like a bad file.

use std::path::PathBuf;

use crate::axis::AxisMap;

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spatiand")
}

pub fn axes_path() -> PathBuf {
    config_dir().join("axes.toml")
}

/// Load the stored map, or `None` if there is none, it is unreadable, or it predates the
/// current convention.
pub fn load_axes() -> Option<AxisMap> {
    let path = axes_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<AxisMap>(&text) {
        Ok(map) if map.is_usable() => {
            log::info!("loaded axis map from {}: {}", path.display(), map.summary());
            Some(map)
        }
        Ok(map) => {
            log::warn!(
                "discarding {} — stored map is from an older convention (v{}) or malformed",
                path.display(),
                map.version
            );
            None
        }
        Err(e) => {
            log::warn!("could not parse {}: {e}", path.display());
            None
        }
    }
}

pub fn save_axes(map: &AxisMap) -> std::io::Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = axes_path();
    // Keep the previous map alongside the new one. Both things that write here -- finishing a
    // calibration and the HUD's cycle control -- are one button press, and the cycle control
    // in particular is easy to hit while simply reading the menu. Losing a map that took a
    // measurement to get right, with no way back, is not a reasonable outcome of that.
    if path.exists() {
        let _ = std::fs::copy(&path, dir.join("axes.previous.toml"));
    }
    let text = toml::to_string_pretty(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let map = AxisMap {
            version: crate::axis::CURRENT_VERSION,
            yaw_axis: 0,
            yaw_sign: -1.0,
            pitch_axis: 2,
            pitch_sign: 1.0,
            roll_axis: 1,
            roll_sign: -1.0,
        };
        let text = toml::to_string_pretty(&map).unwrap();
        let back: AxisMap = toml::from_str(&text).unwrap();
        assert_eq!(map, back);
    }

    #[test]
    fn saving_keeps_the_previous_map() {
        // The HUD's cycle control is one press away at all times, and a map that took a
        // measurement to establish should survive being stepped past by accident.
        let dir = std::env::temp_dir().join(format!("spatiand-axes-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let first = AxisMap::XREAL_AIR;
        let second = AxisMap::IDENTITY;
        save_axes(&first).expect("first save");
        save_axes(&second).expect("second save");
        let previous = std::fs::read_to_string(config_dir().join("axes.previous.toml"))
            .expect("previous should have been kept");
        let recovered: AxisMap = toml::from_str(&previous).expect("parses");
        assert_eq!(recovered, first);
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn a_v1_file_is_not_accepted() {
        let text = "version = 1\nyaw_axis = 2\nyaw_sign = 1.0\npitch_axis = 1\n\
                    pitch_sign = 1.0\nroll_axis = 0\nroll_sign = -1.0\n";
        let map: AxisMap = toml::from_str(text).unwrap();
        assert!(
            !map.is_usable(),
            "v1 stored a left-handed convention and must be re-measured, not reinterpreted"
        );
    }
}
