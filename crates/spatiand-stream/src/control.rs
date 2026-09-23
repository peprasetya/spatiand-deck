//! The conversation: what the session asks for, and what the host says is happening.
//!
//! Everything here is reliable and ordered. It is small and infrequent — a window opening, a
//! title changing, a bandwidth ceiling being moved — and none of it is worth losing to save a
//! packet. Pictures and sound do not come this way; see [`crate::video`].
//!
//! ## Windows belong to the host
//!
//! The host invents [`WindowId`]s and the session never does. An application can open a window
//! nobody asked for, a splash screen can come and go before its game appears, and the session
//! has to cope either way — so "the host tells you what exists" is the only rule that does not
//! fall apart. The same list arrives again after a reconnection, which is what makes a
//! reattach indistinguishable from a first connection on this side.
//!
//! ## The session says what it can see
//!
//! [`ClientMessage::Visibility`] is the unusual one, and it is the whole reason a headset can
//! afford several remote windows at once. The session knows how big each window looks and
//! whether the wearer is facing it; the host knows nothing of the kind. Sent that, the host
//! can spend its budget on the window being looked at and let the one behind the wearer cost
//! nothing at all.

use serde::{Deserialize, Serialize};

/// What the session's control stream starts with, so the host can tell it from sound.
///
/// **One stream, in order, for everything the session says.** It used to be a stream per
/// message — which QUIC delivers reliably and, between streams, in whatever order it likes.
/// A key's press and its release are two messages, so under load the release could arrive
/// first and the key stayed down for ever, repeating; the same for a mouse button, which is
/// how a single click in a viewer became a drag that never ended. Order is not a detail here,
/// it is the whole meaning of a press and a release.
pub const CONTROL_MAGIC: [u8; 4] = *b"SPct";

use crate::catalog::{App, Eyes, Layer};
use crate::video::Codec;

/// A window on the host, named by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub u32);

/// What the session sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Always first. Carries what this end can decode, so the host never offers a codec that
    /// would arrive as a black rectangle.
    Hello {
        version: u32,
        /// In preference order, best first.
        codecs: Vec<Codec>,
        /// The largest picture this end's decoder will take, in pixels.
        max_size: (u32, u32),
        /// The display's own rate. The host paces applications to it rather than to a number
        /// it chose, so frames arrive just in time instead of just too late.
        refresh_mhz: u32,
        /// Empty on a first connection; the session's own name for itself when reattaching.
        session: String,
    },
    /// Start an application, or adopt the windows it already has.
    Launch { app: String },
    /// The wearer closed the window. The application is asked to close, not killed.
    Close { window: WindowId },
    /// The wearer resized the window, in the pixels it should now render.
    Configure {
        window: WindowId,
        width: u32,
        height: u32,
    },
    /// Keyboard focus moved. Exactly one window has it, or none.
    Focus { window: Option<WindowId> },
    /// How big each window looks from where the wearer is, and whether it is in view at all.
    ///
    /// Sent when it changes by enough to matter, not every frame.
    Visibility { windows: Vec<Seen> },
    /// Move the ceiling. Everything under it is the host's decision.
    Bandwidth(Bandwidth),
    /// Something was lost or a window is about to be looked at. Answered with a keyframe.
    WantKeyframe { window: WindowId },
    /// Something the wearer did to a window. See [`Input`].
    Input { window: WindowId, input: Input },
    /// The whole state of the gamepad, for whatever is being played there. See [`Pad`].
    Pad(Pad),
    /// The wearer's head, as an absolute view rather than a nudge.
    ///
    /// Sent unreliably in the real thing; it is here because it is part of the same
    /// conversation and has to be defined once. See [`Viewport`].
    Viewport(Viewport),
    /// The session's clipboard changed. See [`Clipboard`].
    Clipboard(Clipboard),
    /// Goodbye. Applications keep running; only the viewer goes away.
    Detach,
    /// Kill an application outright — every process it started — when asking it to close has
    /// not worked. Named by catalogue id, because that is what owns the processes; a window
    /// is only one of the things an application has open.
    ForceQuit { app: String },
}

/// What the host sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    /// The answer to `Hello`. A refusal carries both version numbers rather than a silence.
    Welcome {
        version: u32,
        host: String,
        /// True when this host already had windows for this session and is handing them back.
        reattached: bool,
    },
    Refused { version: u32, reason: String },
    /// Everything this host can run. Sent after `Welcome`, and again whenever it changes —
    /// which is what makes the configurator's edits appear in the launcher without a restart.
    Catalog { apps: Vec<App> },
    Opened(WindowInfo),
    Closed { window: WindowId },
    Retitled { window: WindowId, title: String },
    /// How this window's pictures are encoded from now on. Always before the first packet, and
    /// again whenever the size or the codec changes.
    Stream {
        window: WindowId,
        codec: Codec,
        width: u32,
        height: u32,
        eyes: Eyes,
    },
    /// What the pointer should look like over this window, as premultiplied BGRA.
    Cursor {
        width: u32,
        height: u32,
        hotspot: (u32, u32),
        pixels: Vec<u8>,
    },
    /// The host's clipboard changed.
    Clipboard(Clipboard),
    /// Whether anything here is listening for a microphone.
    ///
    /// True when an application the host started has opened a capture stream, and false when
    /// the last of them closes it. The session sends the wearer's microphone only while it is
    /// true: a microphone in someone's room should not be open because a program on another
    /// machine happens to be running. See the host's `route` and `microphone`.
    Microphone { wanted: bool },
    /// The application exited by itself.
    Exited { app: String, status: Option<i32> },
    /// A game asked the pad to rumble. Two motor strengths, as a force-feedback effect gives
    /// them; the headset end decides what its own hardware does with them.
    Rumble { strong: u16, weak: u16 },
    /// What a window has become in the room, whenever that changes.
    ///
    /// Not a property of the catalogue entry, because the application decides it, and at a
    /// moment of its own choosing: a viewer that logs in on an ordinary window and only then
    /// becomes the world. Its eye layout travels separately, in [`HostMessage::Stream`] — an
    /// application that starts drawing two eyes changes the shape of its picture at the same
    /// moment, so the stream is re-announced then anyway.
    ///
    /// Last in this enum, and must stay so: a session built before it reports it as unknown
    /// rather than mistaking it for something else. See `crate::VERSION`.
    Layer { window: WindowId, layer: Layer },
}

/// A window the host has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window: WindowId,
    /// Which catalogue entry it belongs to. The session builds the layout's app id from this,
    /// so every window of one application shares its controller layout.
    pub app: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Set for a menu or a tooltip: the window it hangs off, and where on it.
    #[serde(default)]
    pub parent: Option<(WindowId, i32, i32)>,
}

/// Something done to a remote window.
///
/// Deliberately thin, and in the units the other end already thinks in: surface coordinates
/// for the pointer, evdev codes for keys and buttons. A host is a compositor, so it delivers
/// these to *that window* rather than typing them at whatever happens to be focused — which is
/// the thing a screen-capture streamer cannot do.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Input {
    /// Where the pointer is, in the window's own pixels.
    Motion { x: f64, y: f64 },
    /// The pointer left this window; nothing is under it any more.
    Leave,
    /// An evdev button code (`BTN_LEFT` is 0x110).
    Button { button: u32, pressed: bool },
    /// A scroll, in the same units a wheel notch produces.
    Scroll { horizontal: f64, vertical: f64 },
    /// An evdev key code — what a keyboard reports, before any layout is applied.
    ///
    /// The layout stays on the host, which is the only end that knows what the application
    /// expects. Sending characters instead would break every game that reads scancodes.
    Key { code: u32, pressed: bool },
}

/// The gamepad, whole, as a snapshot rather than as changes.
///
/// A snapshot because that is what a pad *is*: a game reads a state, not a history, and a
/// state that arrives late is still the truth while a missed change is a stick left pushed
/// over for ever. It is sent whenever it differs from the last one sent, and once more, at
/// rest, when the wearer stops playing — so nothing is ever left held down.
///
/// The shape is `spatiand_pad::Report`'s, which is the device the host creates: an Xbox pad's
/// buttons and axes, plus four spare axes a head can be put on. This crate deliberately does
/// not depend on that one — the wire is the wire — so the two are converted at each end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Pad {
    /// Bit `i` is `spatiand_pad::BUTTON_CODES[i]`.
    pub buttons: u16,
    /// Bits 0..3: up, down, left, right.
    pub dpad: u8,
    /// Sticks, -1..1, +y up.
    pub left: (f32, f32),
    pub right: (f32, f32),
    /// Triggers, 0..1.
    pub triggers: (f32, f32),
    /// The four spare axes, -1..1.
    pub extra: [f32; 4],
}

impl Pad {
    pub const UP: u8 = 1;
    pub const DOWN: u8 = 2;
    pub const LEFT: u8 = 4;
    pub const RIGHT: u8 = 8;

    /// Nothing held and every stick centred.
    pub fn at_rest(&self) -> bool {
        *self == Pad::default()
    }
}

/// One window, as the wearer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    pub window: WindowId,
    /// How wide it looks, in milliradians. A window across the room is worth fewer bits than
    /// the same window at arm's length.
    pub angular_width_mrad: u32,
    /// Whether any of it is in the view at all.
    pub in_view: bool,
    /// Whether it is the one being used.
    pub focused: bool,
}

/// The ceiling the wearer sets. Everything below it is decided by the host, frame by frame.
///
/// There is no target bitrate here, and that is the point: a window whose pixels have not
/// changed should cost nothing, and a still one should cost almost nothing, without anybody
/// choosing a number for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bandwidth {
    /// The most this host may use, across every window, in kbit/s.
    pub max_kbit: u32,
    /// The most any one window may be encoded at.
    pub max_fps: u8,
    /// What a window that has stopped changing drops to. Zero means "send nothing at all",
    /// which is the honest answer for a window that is genuinely still.
    pub idle_fps: u8,
    /// How long a window must be unchanged to count as still.
    pub idle_after_ms: u32,
}

impl Default for Bandwidth {
    /// What a session starts with before anyone has an opinion: enough for a game on a good
    /// wireless link, and nothing for a window that is not moving.
    fn default() -> Self {
        Bandwidth {
            max_kbit: 40_000,
            max_fps: 72,
            idle_fps: 0,
            idle_after_ms: 250,
        }
    }
}

/// Where the wearer is looking, as an absolute view.
///
/// Sent as a whole view rather than as movement, because movement cannot be resynchronised: a
/// lost or late nudge leaves the two ends disagreeing about where the head is, for ever. An
/// absolute view corrects itself on the next packet, which is why it is worth sending the
/// bigger message.
///
/// The frame is `spatiand_xr_v1`'s, which is OpenXR's: +X right, +Y up, −Z forward,
/// right-handed, metres, quaternions in (x, y, z, w).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// Counts up. Each picture says which viewport it was drawn for, so the session can tell
    /// how stale what it is looking at really is — and correct the difference.
    pub seq: u32,
    /// The session's clock, in microseconds.
    pub time_us: u64,
    pub orientation: [f32; 4],
    pub position: [f32; 3],
    /// Where each eye is, index 0 the left, in the same frame as `position`.
    ///
    /// Sent rather than worked out at the other end, because an eye is not simply half an IPD
    /// either side of the head: the session puts the eyes on the end of a neck lever, so they
    /// move as the head turns. A host that rebuilt them from the head would draw from somewhere
    /// slightly different from where the session draws the room, and the two would disagree by
    /// a few millimetres that nobody could explain. Both eyes share the head's orientation.
    pub eye_position: [[f32; 3]; 2],
    /// Per eye: angleLeft, angleRight, angleUp, angleDown, in radians, signed — `XrFovf`
    /// exactly, the same four numbers the pose channel carries, so a host can copy them across
    /// without converting.
    pub fov: [[f32; 4]; 2],
    /// What the host should render, in pixels, including the overscan the session wants for
    /// correcting rotation.
    pub render_size: (u32, u32),
}

/// A clipboard that changed, on either side.
///
/// Spatiand holds the clipboard for the whole session, so this travels both ways and means the
/// same thing in both directions. Small text is carried; anything large is announced and
/// fetched only if somebody actually pastes, because a copied image should not cost the same
/// as a second of video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Clipboard {
    /// What is on the clipboard now, with the text included when it is small enough to be
    /// worth sending unasked.
    Offer {
        mime_types: Vec<String>,
        /// `text/plain;charset=utf-8`, when it is under the eager limit.
        text: Option<String>,
        /// How big the largest form is, so the other end can decide whether to ask.
        bytes: u32,
    },
    /// Somebody pasted and the data was not sent with the offer.
    Want { mime_type: String },
    /// The answer to `Want`.
    Data { mime_type: String, bytes: Vec<u8> },
}

/// Text at or under this size travels with the offer; anything bigger waits to be asked for.
pub const CLIPBOARD_EAGER_BYTES: u32 = 256 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_survives_the_wire() {
        let client = [
            ClientMessage::Hello {
                version: crate::VERSION,
                codecs: vec![Codec::H265, Codec::Av1, Codec::H264],
                max_size: (3840, 2160),
                refresh_mhz: 72_000,
                session: "deck".into(),
            },
            ClientMessage::Launch {
                app: "firestorm".into(),
            },
            ClientMessage::Visibility {
                windows: vec![Seen {
                    window: WindowId(3),
                    angular_width_mrad: 700,
                    in_view: true,
                    focused: false,
                }],
            },
            ClientMessage::Bandwidth(Bandwidth::default()),
            ClientMessage::Viewport(Viewport {
                seq: 9,
                time_us: 1_234_567,
                orientation: [0.0, 0.0, 0.0, 1.0],
                position: [0.0, 1.6, 0.0],
                eye_position: [[-0.032, 1.6, 0.0], [0.032, 1.6, 0.0]],
                fov: [[-1.0, 1.0, 1.0, -1.0]; 2],
                render_size: (2304, 1296),
            }),
            ClientMessage::Input {
                window: WindowId(1),
                input: Input::Motion { x: 12.5, y: 400.0 },
            },
            ClientMessage::Input {
                window: WindowId(1),
                input: Input::Key {
                    code: 30,
                    pressed: true,
                },
            },
            ClientMessage::Clipboard(Clipboard::Want {
                mime_type: "image/png".into(),
            }),
            ClientMessage::Detach,
        ];
        for message in client {
            let bytes = crate::to_bytes(&message).expect("encodes");
            let back: ClientMessage = crate::from_bytes(&bytes).expect("decodes");
            assert_eq!(back, message);
        }

        let host = [
            HostMessage::Welcome {
                version: crate::VERSION,
                host: "workshop".into(),
                reattached: true,
            },
            HostMessage::Opened(WindowInfo {
                window: WindowId(1),
                app: "firestorm".into(),
                title: "Firestorm".into(),
                width: 1920,
                height: 1080,
                parent: None,
            }),
            HostMessage::Stream {
                window: WindowId(1),
                codec: Codec::H265,
                width: 1920,
                height: 1080,
                eyes: Eyes::Mono,
            },
            HostMessage::Clipboard(Clipboard::Offer {
                mime_types: vec!["text/plain;charset=utf-8".into()],
                text: Some("hello".into()),
                bytes: 5,
            }),
            HostMessage::Exited {
                app: "firestorm".into(),
                status: Some(0),
            },
            HostMessage::Stream {
                window: WindowId(1),
                codec: Codec::H265,
                width: 3840,
                height: 1080,
                eyes: Eyes::SideBySide,
            },
            HostMessage::Layer {
                window: WindowId(1),
                layer: Layer::Projection,
            },
        ];
        for message in host {
            let bytes = crate::to_bytes(&message).expect("encodes");
            let back: HostMessage = crate::from_bytes(&bytes).expect("decodes");
            assert_eq!(back, message);
        }
    }

    #[test]
    fn the_default_ceiling_sends_nothing_for_a_still_window() {
        let b = Bandwidth::default();
        assert_eq!(b.idle_fps, 0);
        assert!(b.idle_after_ms > 0, "or a window would stop mid-animation");
    }

    #[test]
    fn a_viewport_is_small_enough_to_send_often() {
        let v = ClientMessage::Viewport(Viewport {
            seq: u32::MAX,
            time_us: u64::MAX,
            orientation: [0.5; 4],
            position: [1.5; 3],
            eye_position: [[1.5; 3]; 2],
            fov: [[1.0; 4]; 2],
            render_size: (4096, 4096),
        });
        // 250 a second has to be unremarkable next to the pictures.
        let bytes = crate::to_bytes(&v).expect("encodes").len();
        assert!(bytes < 128, "a viewport is {bytes} bytes");
    }
}
