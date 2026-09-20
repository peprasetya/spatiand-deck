//! Drawing a window — all of it — into one buffer of our own choosing.
//!
//! A window is a surface with subsurfaces under it: a player's video with its controls, a
//! browser's own compositing, a menu drawn beside its parent. Encoding the top-level surface's
//! buffer alone would send the wearer a window with pieces missing. So the host does what a
//! compositor does, once per frame, into a buffer it allocated itself.
//!
//! Allocating it ourselves is the other half of the point. A client hands over whatever its
//! driver felt like — a tiled modifier, a swizzled format, ten different things across ten
//! applications — and the encoder wants one specific thing on one specific device. A buffer we
//! made is a buffer we can describe.
//!
//! The result never touches the CPU: it is a dmabuf, handed straight to VA-API in
//! [`crate::encode`].

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::import_surface_tree;
use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};
use smithay::utils::{Physical, Rectangle, Size, Transform};

/// Draws a window's surface tree into a buffer somebody else owns.
///
/// The buffer comes from the encoder (see [`crate::encode`]), which is the only way libavutil
/// will take part: it allocates the VA-API surface and lends it out as a dmabuf, and this draws
/// into that. Nothing here allocates, and nothing is copied.
#[derive(Default)]
pub struct Composer {
    /// How many menus were drawn last time, so a change is said once.
    menus: usize,
}

impl Composer {
    pub fn new() -> Composer {
        Composer::default()
    }

    /// Draw a window — its surface tree and any menus open on it — into `target`, at `size`.
    ///
    /// **Drawn from the window's geometry, not its surface.** A client that draws its own
    /// shadow (Chrome does) has a surface larger than the window, and says where the window
    /// is inside it. Drawing the surface at the origin put that shadow margin on the top and
    /// left of the picture and cut the same amount off the right and bottom.
    pub fn compose_into(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &smithay::desktop::Window,
        target: &mut Dmabuf,
        size: (u32, u32),
    ) -> Result<(), String> {
        use smithay::backend::renderer::element::AsRenderElements;
        let Some(surface) = crate::state::surface_of(window) else {
            return Err("a window with no surface has nothing to encode".into());
        };
        if size.0 == 0 || size.1 == 0 {
            return Err("a window with no size has nothing to encode".into());
        }
        // Everything the client committed becomes a texture on this renderer. A surface that
        // has not been imported draws as nothing at all, silently, which is worth failing on
        // rather than sending a black rectangle.
        import_surface_tree(renderer, &surface).map_err(|e| format!("could not import the window's buffers: {e}"))?;
        let mut menus = 0;
        for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(&surface) {
            menus += 1;
            import_surface_tree(renderer, popup.wl_surface())
                .map_err(|e| format!("could not import a menu's buffers: {e}"))?;
        }
        if menus != self.menus {
            log::debug!("drawing {menus} menu(s) on a window");
            self.menus = menus;
        }

        let origin = window.geometry().loc;
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
            renderer,
            (-origin.x, -origin.y).into(),
            1.0.into(),
            1.0,
        );

        let physical: Size<i32, Physical> = (size.0 as i32, size.1 as i32).into();
        let whole = Rectangle::from_size(physical);

        let mut framebuffer = renderer
            .bind(target)
            .map_err(|e| format!("could not draw into the encoder's buffer: {e}"))?;
        {
            let mut frame = renderer
                .render(&mut framebuffer, physical, Transform::Normal)
                .map_err(|e| format!("could not start a frame: {e}"))?;
            // Black, not transparent: what the encoder takes has no alpha, and a window
            // smaller than its buffer should be surrounded by something deliberate.
            frame
                .clear(Color32F::BLACK, &[whole])
                .map_err(|e| format!("could not clear: {e}"))?;
            // Back to front. `render_elements_from_surface_tree` gives them front first,
            // which is the order a damage tracker wants and the reverse of the order a plain
            // painter's algorithm does.
            for element in elements.iter().rev() {
                let src = element.src();
                let dst = element.geometry(1.0.into());
                // The damage is the element's own, relative to where it is drawn — not the
                // canvas's. Passing `dst` itself worked while every window was drawn at the
                // origin; once a window drawn from its geometry moved its surface to a negative
                // offset, it clipped the right and bottom edges to black, and a menu, drawn
                // well inside the canvas, was clipped away entirely.
                let own = Rectangle::from_size(dst.size);
                RenderElement::draw(element, &mut frame, src, dst, &[own], &[])
                    .map_err(|e| format!("could not draw part of the window: {e}"))?;
            }
            // Wait for the GPU here, because the encoder reads this buffer the moment this
            // returns and a half-drawn picture would encode as a torn one. It is a real cost —
            // this thread does nothing while the GPU finishes — and the proper fix is to hand
            // the fence to VA-API so the two queue up behind each other instead. That needs a
            // second buffer to be worth anything, so it waits until there is a network to keep
            // busy.
            frame
                .finish()
                .map_err(|e| format!("could not finish the frame: {e}"))?
                .wait()
                .map_err(|e| format!("the GPU did not finish drawing: {e}"))?;
        }
        drop(framebuffer);
        Ok(())
    }
}
