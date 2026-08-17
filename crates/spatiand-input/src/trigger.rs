//! An analogue trigger read as a button.
//!
//! The controller reports both: a digital bit that closes at the very bottom of the travel,
//! and the analogue pull it took to get there. Using only the bit means every click needs the
//! trigger bottomed out against its stop, which is a firm squeeze for something that is meant
//! to be the mouse button you use all day. Using the analogue value with a single threshold is
//! worse — a finger resting near it chatters the button on and off several times a second.
//!
//! So: two thresholds. It takes a deliberate pull to press, and letting go most of the way to
//! release, with a band in between where nothing changes. The digital bit is honoured as well,
//! since a fully-pulled trigger is a press by any reading, and it is the one part of this whose
//! meaning does not depend on a threshold we chose.

/// Pull past this and the trigger is down.
const PRESS: f32 = 0.55;
/// Release back past this and it is up again. The gap between the two is the anti-chatter band.
const RELEASE: f32 = 0.30;

/// One trigger's press state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Trigger {
    down: bool,
}

impl Trigger {
    /// Feed this frame's pull (0..1) and the digital full-pull bit. Returns whether it is down.
    ///
    /// Must be called every frame, including frames where the answer is obviously no: the
    /// hysteresis is state, and a call skipped is a release that never happened.
    pub fn update(&mut self, pull: f32, bottomed: bool) -> bool {
        self.down = if self.down {
            bottomed || pull > RELEASE
        } else {
            bottomed || pull >= PRESS
        };
        self.down
    }

    pub fn is_down(&self) -> bool {
        self.down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deliberate_pull_presses_and_letting_go_releases() {
        let mut t = Trigger::default();
        assert!(!t.update(0.0, false));
        assert!(!t.update(0.4, false), "half a pull is not a click");
        assert!(t.update(0.7, false));
        assert!(t.update(0.0, false) == false);
    }

    #[test]
    fn a_finger_resting_on_the_threshold_does_not_chatter() {
        // The failure this prevents: a trigger held right at the edge sending dozens of clicks
        // a second into whatever is under the cursor.
        let mut t = Trigger::default();
        t.update(0.6, false);
        for pull in [0.54, 0.56, 0.53, 0.57, 0.52] {
            assert!(t.is_down() == t.update(pull, false), "chattered at {pull}");
            assert!(t.is_down(), "released inside the band at {pull}");
        }
        assert!(!t.update(0.2, false), "past the release threshold it lets go");
    }

    #[test]
    fn the_digital_bit_presses_on_its_own() {
        // Belt and braces: if the analogue field is ever wrong or dead, a bottomed-out trigger
        // still clicks.
        let mut t = Trigger::default();
        assert!(t.update(0.0, true));
        assert!(!t.update(0.0, false));
    }
}
