//! When each window is allowed to draw its next frame.
//!
//! A Wayland client asks "tell me when to draw again" and waits. Nothing draws a second frame
//! until a compositor answers, which is why a host that never answers sees one picture from an
//! animating application and then silence — the application is not stuck, it is being polite.
//!
//! So the callback is the throttle, and it is the most useful one this design has:
//!
//! * **In phase with the wearer's display.** The session says what its refresh is, and an
//!   application is woken just in time for a frame that will actually be shown. A host that
//!   paces to its own clock produces frames a few milliseconds too early or too late for
//!   every single one, for ever.
//! * **Cheap when nobody is looking.** A window behind the wearer is woken slowly, and one
//!   nobody is attached to at all can be woken slower still. The application keeps running —
//!   a world with other people in it, a download, a game between rounds — it simply stops
//!   spending a GPU on pictures that will never be sent anywhere.
//!
//! This is the only place that decides how often an application may draw, which is why the
//! bandwidth ceiling and the visibility reports both end up here.

use std::collections::HashMap;
use std::time::Duration;

use spatiand_stream::WindowId;

/// Never faster than this, whatever anybody asks for.
const MAX_FPS: u32 = 240;
/// A window is always woken this often, even when it is out of sight and nobody is watching.
///
/// Not zero, and the reason is worth stating: an application that is never woken can be an
/// application that never finishes starting, never notices it has been resized and never
/// repaints when the wearer turns to look at it. One frame every two seconds costs nothing
/// and keeps everything alive.
const FLOOR_FPS: f32 = 0.5;

/// What a window is currently worth.
///
/// The middle two arrive from the session's visibility reports, which is the half not yet
/// written; `--run` only ever says focused or detached.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// The one being used. As fast as the display can show it.
    Focused,
    /// In view, but not the one being used.
    Visible,
    /// Behind the wearer, or too small to read.
    Away,
    /// Nobody is connected at all.
    Detached,
}

#[derive(Debug, Clone, Copy)]
pub struct Rates {
    /// The session's own display, in millihertz — 72000 for the glasses.
    pub refresh_mhz: u32,
    /// The ceiling the wearer set.
    pub max_fps: u8,
    /// What a window out of sight gets.
    pub away_fps: u8,
    /// What a window gets when nobody is attached.
    pub detached_fps: u8,
}

impl Default for Rates {
    fn default() -> Self {
        Rates {
            refresh_mhz: 60_000,
            max_fps: 72,
            away_fps: 1,
            detached_fps: 1,
        }
    }
}

impl Rates {
    /// How often a window in this state may draw.
    pub fn fps(&self, attention: Attention) -> f32 {
        let display = (self.refresh_mhz as f32 / 1000.0).clamp(1.0, MAX_FPS as f32);
        let ceiling = (self.max_fps as f32).clamp(1.0, MAX_FPS as f32);
        let fps = match attention {
            Attention::Focused => display.min(ceiling),
            // Half the display's rate: still fluid to look at out of the corner of an eye, at
            // half the pictures and half the bits.
            Attention::Visible => (display / 2.0).min(ceiling),
            Attention::Away => self.away_fps as f32,
            Attention::Detached => self.detached_fps as f32,
        };
        fps.max(FLOOR_FPS)
    }

    pub fn interval(&self, attention: Attention) -> Duration {
        Duration::from_secs_f32(1.0 / self.fps(attention))
    }
}

/// Which windows are due to be woken.
#[derive(Debug, Default)]
pub struct Pacer {
    rates: Rates,
    attention: HashMap<WindowId, Attention>,
    last: HashMap<WindowId, Duration>,
}

impl Pacer {
    pub fn new(rates: Rates) -> Pacer {
        Pacer {
            rates,
            ..Default::default()
        }
    }

    /// Moved when the session says what its display does, or the wearer moves the ceiling.
    #[allow(dead_code)]
    pub fn set_rates(&mut self, rates: Rates) {
        self.rates = rates;
    }

    #[allow(dead_code)]
    pub fn rates(&self) -> Rates {
        self.rates
    }

    /// What the session says about a window.
    pub fn set_attention(&mut self, window: WindowId, attention: Attention) {
        self.attention.insert(window, attention);
    }

    pub fn attention(&self, window: WindowId) -> Attention {
        self.attention
            .get(&window)
            .copied()
            .unwrap_or(Attention::Detached)
    }

    pub fn forget(&mut self, window: WindowId) {
        self.attention.remove(&window);
        self.last.remove(&window);
    }

    /// Whether this window should be woken now, and remember that it was.
    ///
    /// `now` is the session's clock as the host knows it; a window that has never been woken
    /// is always due, so a newly mapped one draws immediately rather than waiting out an
    /// interval.
    ///
    /// The next wake is counted from **when it was due**, not from when this loop got round to
    /// it, so the rate stays the rate and stays in phase with the display. Counting from "now"
    /// adds the loop's own lateness to every interval, and a first attempt at this — allowing
    /// a wake three quarters of an interval early so a coarse loop would not skip frames —
    /// produced 84 wakes a second from a 72 Hz setting. Both of those are tested below.
    ///
    /// A window that has fallen more than one interval behind — the loop was blocked, or its
    /// rate has just been raised — starts again from now instead of trying to catch up on
    /// frames nobody will ever see.
    pub fn due(&mut self, window: WindowId, now: Duration) -> bool {
        let interval = self.rates.interval(self.attention(window));
        let Some(&last) = self.last.get(&window) else {
            self.last.insert(window, now);
            return true;
        };
        let next = last + interval;
        if now < next {
            return false;
        }
        self.last
            .insert(window, if now > next + interval { now } else { next });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glasses() -> Rates {
        Rates {
            refresh_mhz: 72_000,
            ..Default::default()
        }
    }

    #[test]
    fn the_window_in_front_gets_the_displays_own_rate() {
        assert_eq!(glasses().fps(Attention::Focused), 72.0);
    }

    #[test]
    fn a_window_behind_the_wearer_is_woken_rarely() {
        let rates = glasses();
        assert!(rates.fps(Attention::Away) <= 1.0);
        assert!(
            rates.fps(Attention::Away) > 0.0,
            "a window that is never woken never repaints when it comes back into view"
        );
    }

    #[test]
    fn the_ceiling_is_a_ceiling() {
        let rates = Rates {
            refresh_mhz: 144_000,
            max_fps: 30,
            ..Default::default()
        };
        assert_eq!(rates.fps(Attention::Focused), 30.0);
    }

    #[test]
    fn a_new_window_draws_at_once() {
        let mut pacer = Pacer::new(glasses());
        assert!(pacer.due(WindowId(1), Duration::from_secs(0)));
    }

    #[test]
    fn a_focused_window_is_woken_once_per_display_frame() {
        let mut pacer = Pacer::new(glasses());
        pacer.set_attention(WindowId(1), Attention::Focused);
        let mut woken = 0;
        // One second of a loop running at 250 Hz, as the real one does between network events.
        for tick in 0..250 {
            if pacer.due(WindowId(1), Duration::from_millis(tick * 4)) {
                woken += 1;
            }
        }
        assert!(
            (71..=73).contains(&woken),
            "woken {woken} times in a second at 72 Hz"
        );
    }

    #[test]
    fn an_unattended_window_is_woken_about_once_a_second() {
        let mut pacer = Pacer::new(glasses());
        pacer.set_attention(WindowId(1), Attention::Detached);
        let mut woken = 0;
        for tick in 0..2500 {
            if pacer.due(WindowId(1), Duration::from_millis(tick * 4)) {
                woken += 1;
            }
        }
        assert!((9..=11).contains(&woken), "woken {woken} times in ten seconds");
    }

    #[test]
    fn a_forgotten_window_starts_over() {
        let mut pacer = Pacer::new(glasses());
        pacer.set_attention(WindowId(1), Attention::Focused);
        assert!(pacer.due(WindowId(1), Duration::from_secs(0)));
        assert!(!pacer.due(WindowId(1), Duration::from_millis(1)));
        pacer.forget(WindowId(1));
        assert!(pacer.due(WindowId(1), Duration::from_millis(1)));
    }
}
