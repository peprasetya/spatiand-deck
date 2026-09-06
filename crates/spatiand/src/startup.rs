//! What the panel says while the glasses are still dark.
//!
//! Starting a session takes about six seconds on the Deck, and for all of them both screens
//! are black. That reads as a machine that has hung, not one that is working — and the first
//! thing anyone does with a black screen is press something, which is the one input most
//! likely to make it worse.
//!
//! Nothing about the wait is avoidable. The measured breakdown, from a real session's log:
//!
//! | | |
//! |---|---|
//! | opening the GPU and building the renderer | ~1.0 s |
//! | reading the applications and decoding the environment | ~1.0 s |
//! | opening the headset, reading its brightness over the MCU | ~0.5 s |
//! | picking a connector and bringing up the sidecar | ~0.3 s |
//! | modeset and letting the DisplayPort link train | ~0.8 s |
//! | **asking the glasses for a stereo mode and waiting for it** | **~2.2 s** |
//! | building the scene framebuffer and the launcher's icons | ~0.5 s |
//!
//! The largest single item is the one nothing can hurry: the glasses are told to become a
//! side-by-side display and the wider mode appears on the connector when it appears.
//!
//! So the answer is to say what is happening rather than to make it quicker. The panel is the
//! only screen that can say it — the glasses are dark precisely because the stages that would
//! make them not dark have not finished — which is why this is a sidecar screen and not a
//! world one.
//!
//! ## Only the last three are shown
//!
//! The sidecar is itself one of the stages, so nothing can be drawn before it exists. The
//! stages before it are listed here anyway, because the progress needs a denominator and
//! "step 2 of 3" would understate a wait the wearer is timing with their patience.

/// One stage of bringing a session up, in the order they happen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Graphics,
    Environment,
    Headset,
    Sidecar,
    Link,
    Stereo,
    World,
}

/// Every stage, in order.
pub const STAGES: [Stage; 7] = [
    Stage::Graphics,
    Stage::Environment,
    Stage::Headset,
    Stage::Sidecar,
    Stage::Link,
    Stage::Stereo,
    Stage::World,
];

impl Stage {
    /// What to tell the wearer this stage is doing.
    ///
    /// Present tense and plain. "Initialising display subsystem" tells someone waiting nothing
    /// they can act on; "Asking the glasses for 3D" at least says which box is busy, and if it
    /// stops there the message is the diagnosis.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Graphics => "Starting the graphics",
            Stage::Environment => "Loading the room",
            Stage::Headset => "Finding the glasses",
            Stage::Sidecar => "Waking this panel",
            Stage::Link => "Bringing up the display",
            // Named for what it is waiting on, because this is the long one and the one that
            // fails. A wearer stuck here has glasses that answered over USB and will not
            // switch modes, which is a real fault with a real fix -- a different cable.
            Stage::Stereo => "Asking the glasses for 3D",
            Stage::World => "Building the world",
        }
    }

    /// How far through this is, counting from one.
    pub fn step(self) -> usize {
        STAGES.iter().position(|s| *s == self).unwrap_or(0) + 1
    }
}

/// The longest a label may be.
///
/// The panel is 800 px across in portrait and the text is set large enough to read at arm's
/// length on a screen resting beside you, so this is narrow. A label that wraps is not broken,
/// it just stops being a glance.
#[cfg_attr(not(test), allow(dead_code))]
pub const MAX_LABEL: usize = 26;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_label_fits_on_the_panel() {
        for stage in STAGES {
            assert!(
                stage.label().len() <= MAX_LABEL,
                "{:?} is {} characters: {:?}",
                stage,
                stage.label().len(),
                stage.label()
            );
        }
    }

    #[test]
    fn the_steps_run_from_one_to_the_end() {
        assert_eq!(Stage::Graphics.step(), 1);
        assert_eq!(Stage::World.step(), STAGES.len());
        // Strictly increasing, so a progress bar built from these never goes backwards.
        for pair in STAGES.windows(2) {
            assert!(pair[0].step() < pair[1].step());
        }
    }

    #[test]
    fn no_label_is_an_instruction() {
        // These appear while the wearer can do nothing but wait. A message that reads as a
        // prompt -- "connect your glasses" -- belongs in `crate::waiting`, which is shown when
        // there is something to act on. Confusing the two means telling someone to fix
        // something that is not broken.
        for stage in STAGES {
            let label = stage.label();
            assert!(
                !label.contains('?') && !label.ends_with('.'),
                "{label:?} reads like a prompt"
            );
        }
    }
}
