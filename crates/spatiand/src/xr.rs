//! `spatiand_xr_v1`: how an application says what it is in the world.
//!
//! The protocol lives in `crates/spatiand-proto/protocol/spatiand-xr-v1.xml` and that file is
//! the specification — this one only implements it. Read the XML first; the reasoning is
//! there, not here.
//!
//! What this module holds is the compositor half: the global, the per-surface state, and the
//! rule that state takes effect on commit and not before.
//!
//! ## Why the state is stored on the surface
//!
//! Not in a map keyed by the object, which was the obvious thing. A surface can outlive the
//! `spatiand_xr_surface_v1` that configured it, a client may destroy the extension object
//! while keeping the window, and the renderer has a `wl_surface` in its hand and nothing else.
//! Putting the state in the surface's own `data_map` means it is found by whoever has the
//! surface, which is the only party who ever needs it.

use std::sync::Mutex;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;

use spatiand_proto::server::spatiand_xr_surface_v1::{
    self, EyeLayout as WireLayout, Layer as WireLayer,
};
use spatiand_proto::server::spatiand_xr_v1::{self, SpatiandXrV1};
use spatiand_proto::server::spatiand_xr_pose_channel_v1::{self, SpatiandXrPoseChannelV1};

use crate::state::Spatiand;

/// How the two eyes' views are packed into one buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EyeLayout {
    #[default]
    Mono,
    SideBySide,
    TopBottom,
}

/// What a surface is in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Window,
    HeadLocked,
    Projection,
    Equirect180,
    Equirect360,
}

/// Everything `spatiand_xr_v1` says about one surface.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct XrState {
    pub layout: EyeLayout,
    pub swapped: bool,
    pub layer: Layer,
    pub yaw_offset_urad: i32,
    /// The client has handed its visibility to the compositor — see [`crate::attention`].
    pub idle_fade: bool,
}

impl XrState {
    /// The sub-rectangle of the buffer one eye should sample, as `(u0, u1, v0, v1)`.
    ///
    /// The whole buffer for a mono surface, half of it otherwise. This is the entire rendering
    /// consequence of the protocol: the quad's position, size and orientation are identical
    /// for both eyes, and only the texture coordinates differ. One window, one place in the
    /// room, each eye seeing its own half of it.
    pub fn eye_rect(&self, left_eye: bool) -> [f32; 4] {
        let first = left_eye != self.swapped;
        match self.layout {
            EyeLayout::Mono => [0.0, 1.0, 0.0, 1.0],
            EyeLayout::SideBySide if first => [0.0, 0.5, 0.0, 1.0],
            EyeLayout::SideBySide => [0.5, 1.0, 0.0, 1.0],
            EyeLayout::TopBottom if first => [0.0, 1.0, 0.0, 0.5],
            EyeLayout::TopBottom => [0.0, 1.0, 0.5, 1.0],
        }
    }

    /// Whether this surface stays where the wearer put it.
    ///
    /// False only for `head_locked` today, which is the one layer that moves itself. The
    /// backend reads this every frame; the distinction has to be about *placement* rather
    /// than drawing, because a window that follows the head must follow it for the pointer
    /// and for a drag as well as for the pixels.
    pub fn is_window(&self) -> bool {
        matches!(self.layer, Layer::Window)
    }

    /// Whether this surface has become the room rather than a thing in it.
    ///
    /// Distinct from `!is_window()`, which is also true of a head-locked panel — a head-locked
    /// panel is still a window, it just moves. This one is not a window at all: it has no
    /// frame, cannot be pointed at, cannot be switched to, and must not be counted as one.
    /// A player that is the room always has two surfaces, because the sky takes no input and
    /// something has to stay the transport, so this is the ordinary case for immersive video
    /// rather than an odd one.
    pub fn is_environment(&self) -> bool {
        matches!(self.layer, Layer::Equirect180 | Layer::Equirect360)
    }
}

/// The state a client has asked for but not yet committed.
#[derive(Debug, Default)]
struct Pending {
    surface: Option<WlSurface>,
    next: XrState,
}

/// What the surface is currently being drawn as.
#[derive(Debug, Default)]
struct Applied(Mutex<XrState>);

/// Read a surface's committed stereo state.
///
/// `XrState::default()` for a surface nobody has configured, which is a mono window — so the
/// renderer never needs to ask whether a surface has this extension.
pub fn state_of(surface: &WlSurface) -> XrState {
    with_states(surface, |states| {
        states
            .data_map
            .get::<Applied>()
            .and_then(|a| a.0.lock().ok().map(|s| *s))
            .unwrap_or_default()
    })
}

/// Apply whatever a client asked for since its last commit.
///
/// Called from the compositor's commit handler, which is what makes every request here
/// double-buffered in the ordinary Wayland way: a client that changes layout and attaches a
/// buffer in one go gets both at the same frame, and never a frame of one without the other.
pub fn commit(surface: &WlSurface, object: &spatiand_xr_surface_v1::SpatiandXrSurfaceV1) {
    let Some(pending) = object.data::<Mutex<Pending>>() else {
        return;
    };
    let Ok(pending) = pending.lock() else {
        return;
    };
    with_states(surface, |states| {
        states.data_map.insert_if_missing_threadsafe(Applied::default);
        if let Some(applied) = states.data_map.get::<Applied>() {
            if let Ok(mut applied) = applied.0.lock() {
                *applied = pending.next;
            }
        }
    });
}

/// Forget a surface's stereo state, as though it had never been configured.
pub fn reset(surface: &WlSurface) {
    with_states(surface, |states| {
        if let Some(applied) = states.data_map.get::<Applied>() {
            if let Ok(mut applied) = applied.0.lock() {
                *applied = XrState::default();
            }
        }
    });
}

// --- the global ---

impl GlobalDispatch<SpatiandXrV1, ()> for Spatiand {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<SpatiandXrV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<SpatiandXrV1, ()> for Spatiand {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &SpatiandXrV1,
        request: spatiand_xr_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            spatiand_xr_v1::Request::GetXrSurface { id, surface } => {
                // One per surface for its lifetime: two objects setting the layout of one
                // surface have no defined resolution, so the protocol makes it an error
                // rather than picking a winner.
                let taken = state.xr_surfaces.iter().any(|(s, _)| *s == surface);
                if taken {
                    resource.post_error(
                        spatiand_xr_v1::Error::SurfaceExists,
                        "this surface already has an xr object",
                    );
                    return;
                }
                let object = data_init.init(
                    id,
                    Mutex::new(Pending {
                        surface: Some(surface.clone()),
                        next: XrState::default(),
                    }),
                );
                state.xr_surfaces.push((surface, object));
            }
            spatiand_xr_v1::Request::GetPoseChannel { id } => {
                let channel = data_init.init(id, ());
                state.pose_clients.push(channel);
                // Answered from the frame loop, which is the only place that knows whether
                // there is a head to track. See `crate::pose`.
                state.pose_channels_to_open = true;
            }
            spatiand_xr_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<spatiand_xr_surface_v1::SpatiandXrSurfaceV1, Mutex<Pending>> for Spatiand {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &spatiand_xr_surface_v1::SpatiandXrSurfaceV1,
        request: spatiand_xr_surface_v1::Request,
        data: &Mutex<Pending>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Ok(mut pending) = data.lock() else {
            return;
        };
        match request {
            spatiand_xr_surface_v1::Request::SetEyeLayout { layout } => {
                match layout.into_result() {
                    Ok(WireLayout::Mono) => pending.next.layout = EyeLayout::Mono,
                    Ok(WireLayout::SideBySide) => pending.next.layout = EyeLayout::SideBySide,
                    Ok(WireLayout::TopBottom) => pending.next.layout = EyeLayout::TopBottom,
                    _ => resource.post_error(
                        spatiand_xr_v1::Error::BadLayout,
                        "not an eye layout this version knows",
                    ),
                }
            }
            spatiand_xr_surface_v1::Request::SetEyeSwapped { swapped } => {
                pending.next.swapped = swapped != 0;
            }
            spatiand_xr_surface_v1::Request::SetLayer { layer } => {
                // Kept in the protocol's own type as well as ours: `layer_refused` has to
                // name what was asked for, and a client that asked for two things needs to
                // know which one came back.
                let Ok(wire) = layer.into_result() else {
                    resource.post_error(
                        spatiand_xr_v1::Error::BadLayer,
                        "not a layer this version knows",
                    );
                    return;
                };
                let wanted = match wire {
                    WireLayer::Window => Layer::Window,
                    WireLayer::HeadLocked => Layer::HeadLocked,
                    WireLayer::Projection => Layer::Projection,
                    WireLayer::Equirect180 => Layer::Equirect180,
                    WireLayer::Equirect360 => Layer::Equirect360,
                    _ => {
                        resource.post_error(
                            spatiand_xr_v1::Error::BadLayer,
                            "not a layer this version knows",
                        );
                        return;
                    }
                };
                // Refused rather than half-honoured. A layer that is accepted and then drawn
                // as something else is worse than one that is declined: the client believes
                // it is immersive and lays itself out accordingly.
                if let Some(reason) = refusal(wanted) {
                    resource.layer_refused(wire, reason.into());
                    return;
                }
                // The environment is exclusive: there is one room and it can only be one
                // thing. Claimed here rather than on commit, because two clients asking in
                // the same frame must get different answers and a commit is too late to be
                // one of them.
                let surface = pending.surface.clone();
                if matches!(wanted, Layer::Equirect180 | Layer::Equirect360) {
                    match (&state.sky_owner, &surface) {
                        (Some(owner), Some(mine)) if owner != mine => {
                            resource.layer_refused(
                                wire,
                                "another application is already the environment".into(),
                            );
                            return;
                        }
                        (_, Some(mine)) => state.sky_owner = Some(mine.clone()),
                        (_, None) => {}
                    }
                } else if let (Some(owner), Some(mine)) = (&state.sky_owner, &surface) {
                    // Leaving the sky for something else gives it back.
                    if owner == mine {
                        state.sky_owner = None;
                    }
                }
                pending.next.layer = wanted;
            }
            spatiand_xr_surface_v1::Request::SetYawOffset { microradians } => {
                pending.next.yaw_offset_urad = microradians;
            }
            spatiand_xr_surface_v1::Request::SetIdleFade { enable } => {
                pending.next.idle_fade = enable != 0;
            }
            spatiand_xr_surface_v1::Request::Destroy => {
                if let Some(surface) = pending.surface.take() {
                    if state.sky_owner.as_ref() == Some(&surface) {
                        state.sky_owner = None;
                    }
                    reset(&surface);
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &spatiand_xr_surface_v1::SpatiandXrSurfaceV1,
        _data: &Mutex<Pending>,
    ) {
        state.xr_surfaces.retain(|(_, object)| object != resource);
    }
}

impl Dispatch<SpatiandXrPoseChannelV1, ()> for Spatiand {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &SpatiandXrPoseChannelV1,
        _request: spatiand_xr_pose_channel_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &SpatiandXrPoseChannelV1,
        _data: &(),
    ) {
        state.pose_clients.retain(|c| c != resource);
    }
}

/// Why a layer cannot be had, or `None` if it can.
///
/// Only the two immersive layers are refusable today, and only because they are not built
/// yet. Saying so is the whole point: a client that asks for the sky and is silently drawn as
/// a window has no way to find that out.
fn refusal(layer: Layer) -> Option<&'static str> {
    match layer {
        Layer::Window | Layer::HeadLocked | Layer::Equirect180 | Layer::Equirect360 => None,
        // The one layer still missing. A client that renders its own two eye views can have
        // them shown as a window today -- what it cannot yet have is them presented filling
        // the view, which is what this layer means.
        Layer::Projection => Some("projection layers are not implemented yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_nobody_configured_is_a_mono_window() {
        // The renderer asks every surface for this, so the default has to be the ordinary
        // case rather than something that needs checking for.
        let plain = XrState::default();
        assert_eq!(plain.eye_rect(true), [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(plain.eye_rect(false), [0.0, 1.0, 0.0, 1.0]);
        assert!(plain.is_window());
    }

    #[test]
    fn side_by_side_gives_each_eye_its_own_half() {
        let sbs = XrState {
            layout: EyeLayout::SideBySide,
            ..Default::default()
        };
        assert_eq!(sbs.eye_rect(true), [0.0, 0.5, 0.0, 1.0]);
        assert_eq!(sbs.eye_rect(false), [0.5, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn top_bottom_puts_the_left_eye_on_top() {
        // Which way round this goes is not a matter of opinion: over-under stereo is
        // left-on-top, and getting it backwards is a picture that looks nearly right and
        // gives people a headache in ten minutes.
        let tb = XrState {
            layout: EyeLayout::TopBottom,
            ..Default::default()
        };
        assert_eq!(tb.eye_rect(true), [0.0, 1.0, 0.0, 0.5]);
        assert_eq!(tb.eye_rect(false), [0.0, 1.0, 0.5, 1.0]);
    }

    #[test]
    fn swapping_exchanges_the_eyes_and_nothing_else() {
        for layout in [EyeLayout::SideBySide, EyeLayout::TopBottom] {
            let straight = XrState {
                layout,
                ..Default::default()
            };
            let swapped = XrState {
                layout,
                swapped: true,
                ..Default::default()
            };
            assert_eq!(straight.eye_rect(true), swapped.eye_rect(false));
            assert_eq!(straight.eye_rect(false), swapped.eye_rect(true));
        }
    }

    #[test]
    fn swapping_a_mono_surface_does_nothing() {
        let swapped = XrState {
            swapped: true,
            ..Default::default()
        };
        assert_eq!(swapped.eye_rect(true), swapped.eye_rect(false));
    }

    #[test]
    fn the_layers_that_are_not_built_say_so() {
        // The rule this holds: a layer is either honoured or refused out loud. Silently
        // drawing a client's sky as a window is the failure mode worth a test.
        for built in [
            Layer::Window,
            Layer::HeadLocked,
            Layer::Equirect180,
            Layer::Equirect360,
        ] {
            assert!(refusal(built).is_none(), "{built:?} is refused");
        }
        assert!(refusal(Layer::Projection).is_some());
    }

    #[test]
    fn an_equirect_surface_is_not_also_a_window() {
        // It is the room, not a panel in it. `is_window` is what the backend and the window
        // collector both read to decide that.
        for sky in [Layer::Equirect180, Layer::Equirect360] {
            let state = XrState {
                layer: sky,
                ..Default::default()
            };
            assert!(!state.is_window(), "{sky:?} would be drawn as a panel too");
        }
    }
}
