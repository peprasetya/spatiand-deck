//! What the panel says when there is no world to show.
//!
//! A pair of glasses is two devices sharing one cable — a USB device carrying the IMU and the
//! control channel, and a DisplayPort sink carrying the picture — and they fail independently.
//! So "can the wearer see anything" has more than one negative answer, and the answers need
//! different words: one is fixed by plugging the glasses in, and the other is fixed by a
//! *different cable* while the glasses look, by every sign the wearer can check, already
//! plugged in.
//!
//! The messages live here rather than in the backend that shows them because the snapshot
//! backend renders them too, and a screen nobody can look at before shipping is how a message
//! comes to be three lines too tall for the panel it appears on.

/// What is stopping the world from being shown, when something is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Missing {
    /// Nothing is plugged in.
    Headset,
    /// The headset answered over USB, but no display came with it.
    Picture,
}

/// The longest a line may be.
///
/// The panel wraps on width, so nothing here *breaks* if a line runs long — it just wraps
/// somewhere the writing did not intend, and a sentence that folds mid-phrase reads as a bug in
/// the layout. Held to a width that fits the Deck's own panel, which is the screen these are
/// actually read on: the glasses, by definition, are not showing anything when they appear.
#[cfg_attr(not(test), allow(dead_code))]
pub const MAX_LINE: usize = 34;

/// The offer of a way out, appended to whatever the screen is already saying.
///
/// Always a hold, never a press. Leaving used to be a single button when nothing was open yet,
/// on the reasoning that there was nothing to lose — but the cost of leaving is not what makes
/// an accidental exit feel like a fault. Not having meant it is, and that is the same whether
/// or not any windows were open. Spatiand is also *started* with a button, so a press is the
/// one input guaranteed to be arriving at the moment this screen appears.
///
/// Empty when there is no controller to hold: an instruction naming hardware that is not
/// attached is worse than saying nothing.
pub fn exit_hint(has_controller: bool, had_headset: bool) -> &'static str {
    match (has_controller, had_headset) {
        // The wording differs because the stakes do. With windows open this ends a session and
        // takes them with it; with nothing open it is simply a way back out.
        (true, true) => "\n\nHold any button for 2s\nto end the session.",
        (true, false) => "\n\nHold any button for 2s\nto return to the desktop.",
        (false, _) => "",
    }
}

/// What to say, given which half is missing and whether there is anything open to lose.
pub fn message(missing: Missing, had_headset: bool, exit_hint: &str) -> String {
    match missing {
        // The half-connected case, and the one that needs the most explaining: by every sign
        // the wearer can check, the glasses *are* plugged in. The head tracking even works.
        // Saying "connect your glasses" here would be telling them to do what they have done.
        Missing::Picture => format!(
            "Glasses connected, but dark\n\n\
             Head tracking works, so the USB\n\
             side is fine — but no display\n\
             came with it, and there is\n\
             nowhere to put the world.\n\n\
             Reseat the cable, or turn the\n\
             plug over. A cable that carries\n\
             only data will do this too.\n\n\
             Still dark? Shut the Deck fully\n\
             down — not restart. The USB-C\n\
             port can stop offering video\n\
             until it has been powered off.{exit_hint}"
        ),
        Missing::Headset if had_headset => format!(
            "Glasses disconnected\n\n\
             Your windows are still open.\n\n\
             Spatiand is waiting for the\n\
             glasses to come back — plug\n\
             them in again and this screen\n\
             will hand over to them.{exit_hint}"
        ),
        Missing::Headset => format!(
            "Plug in your XR glasses\n\n\
             Spatiand is waiting.\n\n\
             Connect XREAL Air glasses\n\
             over USB-C and this screen\n\
             will hand over to them.{exit_hint}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every screen the wearer can actually be shown, hints included. The hints are part of
    /// this because they are text on the same panel, and a line that overruns is a line that
    /// overruns whichever function wrote it.
    fn every_message() -> Vec<String> {
        let mut out = Vec::new();
        for controller in [true, false] {
            for (missing, had) in [
                (Missing::Picture, true),
                (Missing::Headset, true),
                (Missing::Headset, false),
            ] {
                out.push(message(missing, had, exit_hint(controller, had)));
            }
        }
        out
    }

    #[test]
    fn no_line_is_wider_than_the_panel_can_set_it() {
        for text in every_message() {
            for line in text.lines() {
                assert!(
                    line.chars().count() <= MAX_LINE,
                    "{} chars: {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn leaving_is_never_offered_as_a_single_press() {
        // The regression this guards. A press is the one input certain to be arriving as this
        // screen appears, because a button is what starts Spatiand -- so offering one here is
        // offering to quit before the wearer has looked up, and it was reported as a crash.
        for had in [true, false] {
            let hint = exit_hint(true, had);
            assert!(
                hint.contains("Hold"),
                "{hint:?} offers something other than a hold"
            );
            assert!(!hint.contains("Press any"), "{hint:?} still offers a press");
        }
    }

    #[test]
    fn nothing_is_offered_when_there_is_no_controller_to_offer_it_on() {
        for had in [true, false] {
            assert_eq!(exit_hint(false, had), "");
        }
    }

    #[test]
    fn the_half_connected_case_does_not_tell_you_to_do_what_you_have_done() {
        // The specific way this message can go wrong. The glasses are plugged in; being told to
        // plug them in is what makes a wearer conclude the software cannot see them at all.
        let text = message(Missing::Picture, true, "");
        assert!(
            !text.contains("Plug in") && !text.contains("Connect XREAL"),
            "the half-connected message tells the wearer to connect glasses that are connected"
        );
        // And it has to name both things that actually fix it. The second one is here
        // because it was *observed*, not guessed: a session with the glasses answering over
        // USB and `card0-DP-1` reading disconnected, unchanged by reseating, by turning the
        // plug over, or by walking the glasses through every display mode they have — and
        // then cured by a full power-off. The port had stopped offering video and stayed that
        // way across five days of uptime. Nobody reaches for "shut it all the way down" on
        // their own, and a screen that only mentions the cable sends them hunting for a
        // second cable they do not need.
        for expected in ["cable", "powered off"] {
            assert!(
                text.contains(expected),
                "no mention of {expected:?}: {text}"
            );
        }
        // Specifically not a restart, which leaves the port controller powered and wedged.
        assert!(
            text.contains("not restart"),
            "does not rule out a plain restart: {text}"
        );
    }

    #[test]
    fn every_message_says_which_half_is_missing_in_its_first_line() {
        // The heading is the only part read from across a room, or by someone lifting the
        // glasses off to look at the panel.
        for (missing, had, want) in [
            (Missing::Picture, true, "dark"),
            (Missing::Headset, true, "disconnected"),
            (Missing::Headset, false, "Plug in"),
        ] {
            let text = message(missing, had, "");
            let heading = text.lines().next().unwrap();
            assert!(heading.contains(want), "{heading:?} does not say {want:?}");
        }
    }

    #[test]
    fn an_exit_offer_is_appended_rather_than_replacing_anything() {
        let hint = "\n\nHold any button.";
        for (missing, had) in [(Missing::Picture, true), (Missing::Headset, true)] {
            let plain = message(missing, had, "");
            let with = message(missing, had, hint);
            assert_eq!(with, format!("{plain}{hint}"));
        }
    }
}
