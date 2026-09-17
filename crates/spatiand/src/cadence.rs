//! How evenly the window in front is drawing, for the frame-rate line in the log.
//!
//! The compositor's own frame rate says nothing about a game's. A game that stutters while the
//! room around it is presented at a steady 60 fps looks perfectly healthy in "presented 120
//! frames" -- which is exactly what the log said while Stumble Guys stuttered every time a
//! thumb touched a pad. What the wearer sees is the gaps between the *game's* frames, so that
//! is what is counted here, next to the two things we send it that it might be reacting to:
//! pointer motion, and the cursor it asks us to show or hide in reply.

use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

#[derive(Debug, Default)]
pub struct Cadence {
    /// The surface being watched. A commit from any other resets the gap, so switching windows
    /// is not reported as the new one having stalled.
    watching: Option<WlSurface>,
    last: Option<Instant>,
    drawn: u32,
    longest: Duration,
    motions: u32,
    cursor_changes: u32,
    cursor_visible: Option<bool>,
}

impl Cadence {
    /// The window in front committed a frame.
    pub fn drew(&mut self, surface: &WlSurface, now: Instant) {
        if self.watching.as_ref() != Some(surface) {
            self.watching = Some(surface.clone());
            self.last = None;
        }
        if let Some(last) = self.last {
            self.longest = self.longest.max(now.saturating_duration_since(last));
        }
        self.last = Some(now);
        self.drawn += 1;
    }

    /// Pointer motion was delivered to a surface.
    pub fn pointed(&mut self) {
        self.motions += 1;
    }

    /// A client set its cursor. Only a change between shown and hidden is counted: a client
    /// re-sending the cursor it already has is not a change of mode.
    pub fn cursor(&mut self, visible: bool) {
        if self.cursor_visible.is_some_and(|was| was != visible) {
            self.cursor_changes += 1;
        }
        self.cursor_visible = Some(visible);
    }

    /// What happened since the last call, or `None` if the window in front drew nothing.
    pub fn take(&mut self) -> Option<String> {
        let drawn = std::mem::take(&mut self.drawn);
        let longest = std::mem::take(&mut self.longest);
        let motions = std::mem::take(&mut self.motions);
        let cursor_changes = std::mem::take(&mut self.cursor_changes);
        if drawn == 0 {
            return None;
        }
        let mut line = format!(
            "the window in front drew {drawn} (longest gap {} ms)",
            longest.as_millis()
        );
        if motions > 0 {
            line += &format!(", pointer motion {motions}");
        }
        if cursor_changes > 0 {
            line += &format!(", cursor shown/hidden {cursor_changes} times");
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_drawn_says_nothing() {
        let mut c = Cadence::default();
        c.pointed();
        assert_eq!(c.take(), None);
    }

    #[test]
    fn the_cursor_counts_changes_not_repeats() {
        let mut c = Cadence::default();
        for visible in [true, true, false, false, true] {
            c.cursor(visible);
        }
        assert_eq!(c.cursor_changes, 2);
    }
}
