//! The gamepad an application here is played with.
//!
//! The wearer's controller is on the other side of a network, and nothing on this machine can
//! see it. So the host creates a gamepad of its own — a uinput device with Steam's virtual
//! gamepad identity, the same one the session creates for local games — and writes into it
//! whatever the session says the pad is doing. An application enumerating devices finds an
//! ordinary controller; nothing about it says the thumbs are elsewhere.
//!
//! **It is created when the host starts, not when the first report arrives.** A program reads
//! the list of joysticks once, at startup, and a device that appears afterwards is invisible
//! to anything that does not watch for hotplug — Firestorm's joystick library among them. So
//! the pad exists before any application does.
//!
//! **Rumble has to be collected whether or not anybody uses it.** A game uploading a
//! force-feedback effect is blocked by the kernel until the request is answered, so
//! [`Pads::rumble`] is polled every time round the host's loop. What it returns goes back to
//! the session, which owns the only motors in this arrangement.
//!
//! Without `/dev/uinput` — a container, a machine whose `uinput` module is not loaded, a user
//! without permission — this says so once and everything else carries on. The applications
//! run; they simply see no controller.

use spatiand_pad::{Report, VirtualPad};
use spatiand_stream::Pad;

pub struct Pads {
    pad: Option<VirtualPad>,
    /// The last thing written, so an unchanged report costs nothing.
    last: Option<Report>,
    /// Said once, not once per report.
    complained: bool,
    /// Whether anything has ever been held down. Said once; see [`Pads::apply`].
    played: bool,
}

impl Pads {
    /// Create the pad, or say why there is none.
    pub fn start() -> Pads {
        let pad = match VirtualPad::create() {
            Ok(pad) => {
                log::info!("gamepad: applications here will see one controller");
                Some(pad)
            }
            Err(e) => {
                log::warn!(
                    "no gamepad ({e}); applications here will see no controller. \
                     This needs /dev/uinput and permission to write to it."
                );
                None
            }
        };
        Pads {
            pad,
            last: None,
            complained: false,
            played: false,
        }
    }

    pub fn exists(&self) -> bool {
        self.pad.is_some()
    }

    /// What the session says the wearer is holding.
    pub fn apply(&mut self, state: &Pad) {
        let report = report_of(state);
        if self.last == Some(report) {
            return;
        }
        self.last = Some(report);
        // The first time the wearer actually does something with it, say so. This is the line
        // that settles "the application does not see my controller": if it is here and the
        // application does nothing, everything up to the device worked and the question is
        // what the application does with a joystick it has been given — for Firestorm, that
        // it is switched on in its own preferences. If it is missing, nothing ever arrived.
        if !self.played && !state.at_rest() {
            self.played = true;
            log::info!(
                "gamepad: the session is playing here — buttons {:#06x}, sticks \
                 ({:.2},{:.2}) ({:.2},{:.2})",
                state.buttons,
                state.left.0,
                state.left.1,
                state.right.0,
                state.right.1
            );
        }
        let Some(pad) = self.pad.as_mut() else { return };
        match pad.send(&report) {
            Ok(()) => self.complained = false,
            Err(e) if !self.complained => {
                log::warn!("gamepad: the device stopped taking reports: {e}");
                self.complained = true;
            }
            Err(_) => {}
        }
    }

    /// Let go of everything. The session sends this when the wearer stops playing, and it is
    /// also what a lost link means: a stick left pushed over would keep walking.
    pub fn rest(&mut self) {
        self.apply(&Pad::default());
    }

    /// What a game has asked the motors to do, when it has asked for something new.
    ///
    /// Must be called every time round the loop even when nothing is listening; see the
    /// module notes.
    pub fn rumble(&mut self) -> Option<(u16, u16)> {
        self.pad.as_mut()?.poll_rumble()
    }
}

fn report_of(state: &Pad) -> Report {
    Report {
        buttons: state.buttons,
        dpad_up: state.dpad & Pad::UP != 0,
        dpad_down: state.dpad & Pad::DOWN != 0,
        dpad_left: state.dpad & Pad::LEFT != 0,
        dpad_right: state.dpad & Pad::RIGHT != 0,
        left: state.left,
        right: state.right,
        left_trigger: state.triggers.0,
        right_trigger: state.triggers.1,
        extra: state.extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_and_the_device_agree() {
        let state = Pad {
            buttons: 0b101,
            dpad: Pad::UP | Pad::RIGHT,
            left: (-0.5, 0.25),
            right: (0.75, -1.0),
            triggers: (0.1, 0.9),
            extra: [1.0, -1.0, 0.0, 0.5],
        };
        let report = report_of(&state);
        assert_eq!(report.buttons, 0b101);
        assert!(report.dpad_up && report.dpad_right);
        assert!(!report.dpad_down && !report.dpad_left);
        assert_eq!(report.left, (-0.5, 0.25));
        assert_eq!(report.right, (0.75, -1.0));
        assert_eq!((report.left_trigger, report.right_trigger), (0.1, 0.9));
        assert_eq!(report.extra, [1.0, -1.0, 0.0, 0.5]);
    }

    #[test]
    fn resting_is_everything_let_go() {
        assert_eq!(report_of(&Pad::default()), Report::default());
    }

    #[test]
    fn a_host_without_uinput_still_takes_reports() {
        // The failure mode that matters: no device, and nothing that touches it panics or
        // complains more than once.
        let mut pads = Pads {
            pad: None,
            last: None,
            complained: false,
            played: false,
        };
        pads.apply(&Pad {
            buttons: 1,
            ..Pad::default()
        });
        pads.rest();
        assert_eq!(pads.rumble(), None);
        assert!(!pads.exists());
    }
}
