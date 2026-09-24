//! What is on the clipboard, and the rules both ends follow about it.
//!
//! **The session holds the clipboard for everything it can see.** Copy in an application on a
//! host and it can be pasted on the headset; copy on the headset and it can be pasted on any
//! host; copy on one host and it can be pasted on another, because the session sits between
//! them. Nobody has to think about which machine a window belongs to, which is the whole point
//! of the exercise.
//!
//! What travels is deliberately not the data. A copy announces **what forms it is available
//! in** and how big it is, and the bytes are fetched only when somebody actually pastes. A
//! copied screenshot would otherwise cost as much as a second of video, every time, for a
//! paste that usually never comes. The one exception is small text, which is carried with the
//! announcement so that the overwhelmingly common paste is instant.
//!
//! None of this touches a compositor, which is why it is all tested here rather than on a
//! headset: the two ends differ in where a selection comes from, not in what is decided about
//! it.

use std::io::Read;
use std::os::fd::OwnedFd;

/// The most any single form of a clipboard is allowed to be, in bytes.
///
/// A clipboard is a convenience, not a file transfer. Something the size of a video is either
/// a mistake or an application publishing its entire document as a paste format, and either
/// way sending it across a link that also carries the pictures is worse than not.
pub const LARGEST: usize = 32 << 20;

/// The forms of plain text, best first.
///
/// `text/plain;charset=utf-8` is what Wayland applications agree on. `UTF8_STRING` is X11's
/// name for the same thing and arrives through Xwayland. `text/plain` alone is ambiguous
/// about encoding, so it is the last resort rather than the first.
pub const TEXT_FORMS: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];

/// The best form of plain text on offer, if any.
pub fn best_text(mime_types: &[String]) -> Option<String> {
    TEXT_FORMS
        .iter()
        .find_map(|form| {
            mime_types
                .iter()
                .find(|m| m.eq_ignore_ascii_case(form))
                .cloned()
        })
        // A form this end has never heard of is still text if it says so. HTML, CSV and
        // `text/uri-list` all paste as text into something that asks for text.
        .or_else(|| mime_types.iter().find(|m| is_text(m)).cloned())
}

/// Whether a form is text, and so worth carrying with the announcement when it is small.
pub fn is_text(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "utf8_string" | "string" | "text" | "compound_text"
        )
}

/// The names X11 applications ask text by, which Wayland applications never offer.
///
/// This is not pedantry. An X11 client asks for `STRING`, `TEXT` or `COMPOUND_TEXT` — often
/// blindly, before looking at what is on offer — and Xwayland refuses anything the selection
/// did not name, with `Mime type requested by X client not offered`. So a perfectly good piece
/// of text copied in a Wayland application could not be pasted into Firestorm or a terminal at
/// all, while the same text copied in Chrome could, because Chrome happens to offer the X11
/// names as well.
///
/// `COMPOUND_TEXT` is among them, and that is a judgement rather than an oversight. It is an
/// ISO 2022 encoding, not UTF-8, so answering it with UTF-8 is wrong for text outside ASCII.
/// It was left out for exactly that reason, on the assumption that a client refused it would
/// ask for something else — and the logs say otherwise: Firestorm and QTerminal ask for
/// `COMPOUND_TEXT`, are refused, and **give up**, so the paste produced nothing at all. A
/// plain-ASCII paste is identical either way, and a wrong accent is better than no paste.
pub const X11_TEXT_NAMES: [&str; 4] = ["UTF8_STRING", "STRING", "TEXT", "COMPOUND_TEXT"];

/// What to offer this compositor's own clients, given what the selection really has.
///
/// Text gets every name it is known by, so that whichever one an application asks for is one it
/// was offered. Everything else is passed through untouched: there is no second name for a PNG.
pub fn with_text_aliases(mime_types: &[String]) -> Vec<String> {
    let mut out = mime_types.to_vec();
    if !mime_types.iter().any(|m| is_text(m)) {
        return out;
    }
    for alias in X11_TEXT_NAMES
        .iter()
        .copied()
        .chain(["text/plain;charset=utf-8", "text/plain"])
    {
        if !out.iter().any(|m| m.eq_ignore_ascii_case(alias)) {
            out.push(alias.to_string());
        }
    }
    out
}

/// Which form to actually fetch, when an application asks for `requested`.
///
/// An exact match if there is one. Otherwise, if what is wanted is text and something on offer
/// is text, that: the names differ but the bytes do not, and the alternative is a paste that
/// silently produces nothing.
pub fn resolve(requested: &str, available: &[String]) -> Option<String> {
    if let Some(exact) = available.iter().find(|m| m.eq_ignore_ascii_case(requested)) {
        return Some(exact.clone());
    }
    is_text(requested).then(|| best_text(available)).flatten()
}

/// Forms no other machine can do anything with, which are left out of what is announced.
///
/// X11 answers `TARGETS` with a handful of names that describe the selection rather than its
/// contents. Announcing them would offer a paste that hands back the word "TIMESTAMP".
///
/// Chrome adds two of its own to every copy: a token naming the frame that copied, which
/// means something only inside the browser process that issued it, and the page's address,
/// which it uses to decide how a paste may be used. Carried to another machine, the Chrome
/// there asked for both on every paste, each a round trip to the machine that copied, and
/// pasted nothing until they came back -- and on its own machine the frame token names
/// nothing at all. Chrome pastes plain text and HTML across machines without either.
pub fn is_private(mime: &str) -> bool {
    let lower = mime.to_ascii_lowercase();
    lower.starts_with("chromium/x-internal-")
        || lower == "chromium/x-source-url"
        || matches!(
            mime.to_ascii_uppercase().as_str(),
            "TARGETS" | "TIMESTAMP" | "MULTIPLE" | "SAVE_TARGETS" | "DELETE" | "INSERT_SELECTION"
        )
}

/// What is announced, from everything a selection claims to offer.
pub fn offered(mime_types: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for mime in mime_types {
        if is_private(&mime) || seen.iter().any(|m| m.eq_ignore_ascii_case(&mime)) {
            continue;
        }
        seen.push(mime);
    }
    seen
}

/// Everything a source writes into a pipe, up to `cap`.
///
/// Reading is the caller's thread's business and never the compositor's: the far end of this
/// pipe is an application, which may be slow, wedged, or waiting for something of its own. A
/// compositor that blocks here stops drawing.
///
/// Over the cap the transfer is abandoned rather than truncated. Half a PNG is not a smaller
/// PNG, and pasting one is worse than pasting nothing.
pub fn drain(fd: OwnedFd, cap: usize) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 << 10];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if out.len() + n > cap {
                    return Err(format!("more than {cap} bytes"));
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// A selection this end is holding on behalf of the other one.
///
/// The text is whatever came with the announcement; everything else has to be asked for. Kept
/// as one struct because the three fields are only ever right together: a set of forms from
/// one copy and text from the one before it would paste the previous clipboard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    pub mime_types: Vec<String>,
    /// The text that travelled with the announcement, if it was small enough.
    pub text: Option<String>,
    /// How big the largest form is, so a paste can say what it is waiting for.
    pub bytes: u32,
}

impl Held {
    /// Whether a paste of this form can be answered without asking the other end.
    pub fn answers(&self, mime_type: &str) -> Option<Vec<u8>> {
        let text = self.text.as_ref()?;
        // Any name for plain text, because that is what the carried text is. Not `text/html`
        // or `text/csv`, which are text but not *this* text, and not `image/png`, which must
        // never be answered with a caption.
        let plain = TEXT_FORMS
            .iter()
            .chain(X11_TEXT_NAMES.iter())
            .any(|f| f.eq_ignore_ascii_case(mime_type));
        plain.then(|| text.clone().into_bytes())
    }

    /// Whether this is the same clipboard as `other`, so an announcement can be dropped rather
    /// than sent round again.
    ///
    /// Announcements travel in both directions and a session can hold two hosts at once, so
    /// without this a single copy would circulate: A tells the session, the session tells B,
    /// B's compositor announces it, B tells the session, and so on for as long as anyone is
    /// watching.
    pub fn same_as(&self, other: &Held) -> bool {
        self == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_best_text_form_is_the_one_that_says_its_encoding() {
        let offered = strings(&["text/plain", "TARGETS", "text/plain;charset=utf-8"]);
        assert_eq!(best_text(&offered).as_deref(), Some("text/plain;charset=utf-8"));
    }

    #[test]
    fn chromes_own_bookkeeping_stays_on_its_own_machine() {
        let chrome = strings(&[
            "text/plain;charset=utf-8",
            "chromium/x-internal-source-rfh-token",
            "chromium/x-source-url",
            "chromium/x-web-custom-data",
            "text/html",
        ]);
        assert_eq!(
            offered(chrome),
            strings(&["text/plain;charset=utf-8", "chromium/x-web-custom-data", "text/html"]),
            "the frame token and source address are Chrome's own; its custom data is content"
        );
    }

    #[test]
    fn x11s_name_for_text_counts_as_text() {
        assert_eq!(
            best_text(&strings(&["TIMESTAMP", "UTF8_STRING"])).as_deref(),
            Some("UTF8_STRING")
        );
    }

    #[test]
    fn a_picture_offers_no_text_to_carry() {
        assert_eq!(best_text(&strings(&["image/png", "image/bmp"])), None);
    }

    #[test]
    fn html_and_a_list_of_files_are_still_text() {
        assert_eq!(
            best_text(&strings(&["text/html"])).as_deref(),
            Some("text/html")
        );
        assert!(is_text("text/uri-list"));
        assert!(!is_text("image/png"));
    }

    #[test]
    fn what_x11_says_about_a_selection_is_not_part_of_it() {
        // Announcing these offers a paste that hands back the word "TIMESTAMP".
        let out = offered(strings(&[
            "TARGETS",
            "TIMESTAMP",
            "text/plain;charset=utf-8",
            "SAVE_TARGETS",
        ]));
        assert_eq!(out, strings(&["text/plain;charset=utf-8"]));
    }

    #[test]
    fn a_form_offered_twice_is_announced_once() {
        let out = offered(strings(&["image/png", "image/png", "IMAGE/PNG"]));
        assert_eq!(out, strings(&["image/png"]));
    }

    #[test]
    fn small_text_is_pasted_without_asking_and_a_picture_is_not() {
        let held = Held {
            mime_types: strings(&["text/plain;charset=utf-8", "image/png"]),
            text: Some("hello".into()),
            bytes: 5,
        };
        assert_eq!(held.answers("text/plain;charset=utf-8"), Some(b"hello".to_vec()));
        assert_eq!(held.answers("UTF8_STRING"), Some(b"hello".to_vec()));
        assert_eq!(
            held.answers("image/png"),
            None,
            "a picture must be fetched, not answered with the caption"
        );
    }

    #[test]
    fn the_same_clipboard_arriving_back_is_recognised() {
        // Without this a single copy circulates between the session and two hosts for ever.
        let one = Held {
            mime_types: strings(&["text/plain;charset=utf-8"]),
            text: Some("hello".into()),
            bytes: 5,
        };
        let again = one.clone();
        let later = Held {
            text: Some("hello again".into()),
            ..one.clone()
        };
        assert!(one.same_as(&again));
        assert!(!one.same_as(&later));
    }

    #[test]
    fn a_client_that_asks_only_for_compound_text_is_still_answered() {
        // Firestorm and QTerminal ask for COMPOUND_TEXT and give up when refused, which is
        // why it is claimed despite not being UTF-8. See `X11_TEXT_NAMES`.
        let out = with_text_aliases(&strings(&["text/plain;charset=utf-8"]));
        assert!(out.iter().any(|m| m == "COMPOUND_TEXT"));
    }

    #[test]
    fn text_is_offered_by_every_name_an_x11_application_might_ask_for() {
        // The bug this exists for: qconsole and Firestorm asked for STRING and COMPOUND_TEXT,
        // Xwayland refused because the selection had not named them, and a paste that had
        // perfectly good text behind it produced nothing.
        let out = with_text_aliases(&strings(&["text/plain;charset=utf-8"]));
        for name in ["UTF8_STRING", "STRING", "TEXT", "text/plain"] {
            assert!(out.iter().any(|m| m == name), "{name} should be offered");
        }
    }

    #[test]
    fn a_picture_gains_no_aliases() {
        let out = with_text_aliases(&strings(&["image/png"]));
        assert_eq!(out, strings(&["image/png"]));
    }

    #[test]
    fn a_request_by_one_name_for_text_is_served_by_another() {
        let have = strings(&["text/plain;charset=utf-8", "image/png"]);
        assert_eq!(
            resolve("STRING", &have).as_deref(),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            resolve("COMPOUND_TEXT", &have).as_deref(),
            Some("text/plain;charset=utf-8"),
            "even a form we do not advertise is better answered than refused"
        );
        assert_eq!(resolve("image/png", &have).as_deref(), Some("image/png"));
        assert_eq!(resolve("audio/flac", &have), None);
        assert_eq!(
            resolve("STRING", &strings(&["image/png"])),
            None,
            "text cannot be conjured from a picture"
        );
    }

    #[test]
    fn carried_text_answers_any_name_for_plain_text() {
        let held = Held {
            mime_types: strings(&["text/plain;charset=utf-8"]),
            text: Some("hello".into()),
            bytes: 5,
        };
        for name in ["STRING", "TEXT", "UTF8_STRING", "text/plain"] {
            assert_eq!(held.answers(name), Some(b"hello".to_vec()), "{name}");
        }
        assert_eq!(held.answers("text/html"), None, "html is not this text");
    }

    #[test]
    fn everything_written_into_the_pipe_comes_back() {
        let (read, write) = std::io::pipe().expect("a pipe");
        let long: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let sent = long.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut write = write;
            let _ = write.write_all(&sent);
        });
        assert_eq!(drain(OwnedFd::from(read), LARGEST), Ok(long));
    }

    #[test]
    fn a_source_that_overruns_the_cap_is_abandoned_rather_than_truncated() {
        // Half a PNG is not a smaller PNG.
        let (read, write) = std::io::pipe().expect("a pipe");
        std::thread::spawn(move || {
            use std::io::Write;
            let mut write = write;
            // More than the cap, written in pieces, and the writer is allowed to fail: the
            // reader hangs up as soon as it has seen enough.
            for _ in 0..40 {
                if write.write_all(&[7u8; 4096]).is_err() {
                    return;
                }
            }
        });
        assert!(drain(OwnedFd::from(read), 64 * 1024).is_err());
    }
}
