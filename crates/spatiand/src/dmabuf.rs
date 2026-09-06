//! Letting a client hand us a picture instead of a copy of one.
//!
//! Without this a client's only way to give the compositor a frame is `wl_shm`: shared memory
//! that the application fills with the CPU and the compositor then uploads to a texture. For a
//! terminal that is nothing. For 1080p video it is about four megabytes memcpy'd and uploaded
//! per frame, sixty times a second, on a machine with eight compute units that is already
//! drawing the world twice — and it is entirely avoidable, because the frame was decoded on
//! the GPU and never needed to leave it.
//!
//! `zwp_linux_dmabuf_v1` is how it stays there. The client exports the decoded frame as a dmabuf
//! and sends us a file descriptor; we import it as a texture and sample it. Nothing is copied.
//! It is also what a hardware video decoder produces natively, so this is less an optimisation
//! than the removal of a detour.
//!
//! ## Version 4, and why version 3 was the wrong call
//!
//! This global used to be created at version 3, on the reasoning that version 4's per-surface
//! *feedback* exists to tell a client its buffer could go straight to scanout — and nothing
//! here can ever go straight to scanout, because every window is a texture on a quad in a 3D
//! scene. That reasoning is correct and it is beside the point.
//!
//! The thing version 4 also carries is `main_device`, and `main_device` is one of exactly two
//! ways a client can find out **which GPU to render on**. Mesa's Wayland EGL learns the render
//! node from the `wl_drm` global or from dmabuf feedback at version 4 or later. Spatiand has
//! never offered `wl_drm` — it is a Mesa-specific relic — so at version 3 it offered neither,
//! and Mesa did the thing that costs the most and says the least: it printed
//!
//! ```text
//! libEGL warning: failed to get driver name for fd -1
//! libEGL warning: MESA-LOADER: failed to retrieve device information
//! ```
//!
//! to the client's stderr and returned a perfectly working **software** context. Every
//! capability query then answers the way a real GPU would, because answering that way is what
//! a software rasteriser is for. Nothing in the client can see it coming.
//!
//! Measured, by the first application written against this compositor: a 3840x1920 immersive
//! film took **550% of a core** — five and a half of the Deck's eight threads — and stuttered.
//! The same client, same film, same session, on the GPU: **20%**. The author spent a day
//! looking for the fault in their own renderer, which is exactly where the symptom points and
//! exactly the wrong place.
//!
//! So the global is built with a default feedback naming the renderer's own device, which is
//! what makes it a version 4-or-later global — smithay advertises 5, and 4 is the version that
//! matters because 4 is where `main_device` arrives. Clients that bind at version 3 or lower
//! still get the format list from the main tranche and behave as before; nothing that worked
//! stops working. The scanout tranches that feedback also allows are still not offered,
//! because that part of the original reasoning stands: there is nothing here to scan out.
//!
//! Verified rather than assumed. Under the snapshot harness, `eglinfo` run as a client reports
//! `radeonsi` with the global as built here, and `llvmpipe` preceded by both of those warnings
//! when the device is withheld and the global falls back to version 3.
//!
//! ## Why the import is not tested here
//!
//! The protocol asks the compositor to say whether a buffer is usable, and answering that
//! honestly means asking the renderer to import it. The renderer lives in the backend's frame
//! loop, not in the compositor state, and a Wayland callback has no access to it — so the
//! request is parked in [`crate::state::Spatiand::pending_dmabufs`] and answered from
//! [`settle`] on the next frame, one round trip later.
//!
//! Accepting blindly instead would be one line and is the obvious shortcut. It is also how you
//! get a client that believes its buffer was accepted, draws nothing for the rest of its life,
//! and reports it as "the application does not open" -- which is a failure this project has
//! already chased more than once from the wrong end.

use smithay::backend::allocator::Format;
use smithay::backend::drm::DrmNode;
use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;

use crate::state::Spatiand;

/// Offer dmabuf to clients, in whatever formats this renderer can actually import.
///
/// Called once per backend, after the renderer exists — the format list comes from the
/// renderer and there is nothing truthful to advertise before that.
pub fn advertise(state: &mut Spatiand, renderer: &GlesRenderer) {
    if state.dmabuf_global.is_some() {
        return;
    }
    let formats: Vec<Format> = renderer.dmabuf_formats().into_iter().collect();
    if formats.is_empty() {
        log::warn!("this renderer imports no dmabuf formats; clients will fall back to shm");
        return;
    }

    // The device is asked of the renderer rather than passed in by the backend. Every backend
    // has one to give -- the DRM backend knows its GPU, the others could ask EGL themselves --
    // but the number that matters is the device the *renderer* is on, and asking the renderer
    // is the only way to be sure those are the same thing.
    let node = render_node(renderer);
    let global = node.and_then(|node| {
        match DmabufFeedbackBuilder::new(node.dev_id(), formats.clone()).build() {
            Ok(feedback) => {
                log::info!(
                    "offering dmabuf v4 to clients, main device {:?}, {} format/modifier pair(s)",
                    node.dev_path().unwrap_or_else(|| "?".into()),
                    formats.len()
                );
                Some(
                    state
                        .dmabuf_state
                        .create_global_with_default_feedback::<Spatiand>(
                            &state.display_handle,
                            &feedback,
                        ),
                )
            }
            Err(e) => {
                log::warn!("could not build dmabuf feedback ({e})");
                None
            }
        }
    });

    let global = match global {
        Some(global) => global,
        None => {
            // Worth shouting about. Without the device name every GL client on the session
            // silently runs on llvmpipe -- see the module docs -- and the only side that can
            // see it happening is this one.
            log::warn!(
                "no render node for the dmabuf global; falling back to version 3. GL clients \
                 will have no way to find the GPU and will render in software"
            );
            state
                .dmabuf_state
                .create_global::<Spatiand>(&state.display_handle, formats.clone())
        }
    };
    state.dmabuf_global = Some(global);
}

/// Which DRM render node this renderer is drawing on.
///
/// Via EGL rather than via the backend's own device handle, because a backend's handle is a
/// *primary* node and clients need the *render* node — and because the nested and headless
/// backends have no device handle at all.
fn render_node(renderer: &GlesRenderer) -> Option<DrmNode> {
    let display = renderer.egl_context().display();
    match EGLDevice::device_for_display(display) {
        Ok(device) => match device.try_get_render_node() {
            Ok(node) => {
                if node.is_none() {
                    log::warn!("EGL knows this display's device but names no render node for it");
                }
                node
            }
            Err(e) => {
                log::warn!("could not get a render node from EGL ({e})");
                None
            }
        },
        Err(e) => {
            log::warn!("could not identify this renderer's EGL device ({e})");
            None
        }
    }
}

/// Answer the buffers a client offered since the last frame.
///
/// Each is imported for real, because the answer is a promise: a client told its buffer is
/// good will keep sending that kind and stop offering anything else.
pub fn settle(state: &mut Spatiand, renderer: &mut GlesRenderer) {
    if state.pending_dmabufs.is_empty() {
        return;
    }
    for (buffer, notifier) in std::mem::take(&mut state.pending_dmabufs) {
        match renderer.import_dmabuf(&buffer, None) {
            Ok(_) => {
                // Said once, because "dmabuf is advertised" and "a client is actually using
                // it" are different claims and only the second one is worth anything. After
                // that it would be a line per frame per window.
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| log::info!("a client is sending frames as dmabuf; no copies"));
                // The texture is dropped here and imported again when the surface commits.
                // Wasteful in principle; in practice smithay caches the import against the
                // buffer, and doing it now is the only way to answer honestly.
                let _ = notifier.successful::<Spatiand>();
            }
            Err(e) => {
                log::warn!("a client offered a dmabuf we cannot import ({e}); telling it so");
                notifier.failed();
            }
        }
    }
}
