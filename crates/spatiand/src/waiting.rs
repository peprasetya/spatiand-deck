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
pub const MAX_LINE: usize = 34;

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
             This is the cable, not a setting.\n\
             Reseat it, turn the plug over, or\n\
             use the cable the glasses came\n\
             with — many USB-C cables carry\n\
             data but no video.{exit_hint}"
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

    fn every_message() -> Vec<String> {
        let mut out = Vec::new();
        for hint in ["", "\n\nHold any button for 2s\nto end the session."] {
            out.push(message(Missing::Picture, true, hint));
            out.push(message(Missing::Headset, true, hint));
            out.push(message(Missing::Headset, false, hint));
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
    fn the_half_connected_case_does_not_tell_you_to_do_what_you_have_done() {
        // The specific way this message can go wrong. The glasses are plugged in; being told to
        // plug them in is what makes a wearer conclude the software cannot see them at all.
        let text = message(Missing::Picture, true, "");
        assert!(
            !text.contains("Plug in") && !text.contains("Connect XREAL"),
            "the half-connected message tells the wearer to connect glasses that are connected"
        );
        // And it has to name the thing that actually needs changing.
        assert!(text.contains("cable"), "no mention of the cable: {text}");
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
