//! Everything that differs between one Linux system and the next.
//!
//! The rule the project holds itself to is that `grep -ri 'steamos\|steamdeck\|gamescope'`
//! may only hit the three HAL crates and the docs. This is one of the three: session
//! registration, launching applications, and finding out what is installed.
//!
//! Most of it turns out to be portable anyway. Desktop entries are a freedesktop standard, so
//! [`desktop_entry`] is the same code on SteamOS, Fedora and Debian — which is the point. Only
//! the pieces that genuinely are distribution-shaped sit behind a backend.

pub mod desktop_entry;
pub mod icons;
pub mod launch;
pub mod settings;

pub use desktop_entry::{icon_for_app, scan, DesktopEntry};
pub use icons::resolve as resolve_icon;
pub use launch::{launch, publish_session_environment};
pub use settings::{has_desktop_settings, panel_available, settings_command};
