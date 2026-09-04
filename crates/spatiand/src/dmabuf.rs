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
//! **Version 3, deliberately.** Version 4 adds per-surface *feedback* — telling each client
//! which device and formats would let its buffer be scanned out directly, without
//! compositing. That is worth having on a desktop, where a fullscreen video can be handed
//! straight to the display controller. It is worth nothing here: every window is a texture on
//! a quad in a 3D scene, sampled by a shader, so nothing a client sends can ever go direct to
//! scanout. Version 3 says "here are the formats I can import", which is the entire truth.
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
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;

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
    let global = state
        .dmabuf_state
        .create_global::<Spatiand>(&state.display_handle, formats.clone());
    state.dmabuf_global = Some(global);
    log::info!(
        "offering dmabuf to clients in {} format/modifier pair(s)",
        formats.len()
    );
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
