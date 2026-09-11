// A Wayland client that declares itself stereoscopic, and nothing else.
//
// It paints ONE buffer with two halves -- left half red, right half blue -- and tells
// Spatiand, through spatiand_xr_v1, that they are the left and right eyes' views. There is no
// video, no GL and no head tracking here: the question being asked is only whether each eye
// is shown its own half of one buffer while the window keeps one position in the room.
//
// Rendered through the snapshot backend twice, once per eye, a correct compositor gives a red
// rectangle for the left eye and a blue one for the right. Anything else -- both red, both
// blue, or one window showing both halves squashed side by side -- is the bug.
//
//   cc -o stereo-probe stereo-probe.c xdg-shell-protocol.c spatiand-xr-v1-protocol.c
//      viewporter-protocol.c $(pkg-config --cflags --libs wayland-client)
//   (one command; the line is broken here only because a backslash ending a // comment
//   continues the comment, which the compiler warns about)
//   SPATIAND_BACKEND=snapshot SPATIAND_CLIENT=./stereo-probe SPATIAND_SNAPSHOT_EYE=left ...
//
// SPATIAND_STEREO=tb makes it top-and-bottom instead, and SPATIAND_STEREO=swap swaps the eyes,
// which is how the two remaining branches of the layout arithmetic get exercised against a
// real client rather than only in a unit test.
//
// SPATIAND_LAYER=equirect360 (or equirect180, or head_locked, or projection) asks for that
// layer instead of being an ordinary window. With an equirect layer the two halves become the
// sky rather than a panel, so the whole view goes red or blue depending on the eye -- which is
// a crude picture and an unambiguous test.
//
// SPATIAND_PROBE_SHRINK=1280x264 commits that shape three seconds in, which is what a media
// player does when its window becomes a transport bar. With SPATIAND_ANCHOR=bottom the
// compositor should keep the bottom edge where it was instead of shrinking about the middle.
//
// SPATIAND_IDLE_MS=800 asks for that idle threshold instead of the compositor's default.
//
// SPATIAND_VIEWPORT=1280x800 says, through wp_viewporter, that the surface is that size
// whatever the buffer is. With SPATIAND_PROBE_SIZE=2560x800 and SPATIAND_STEREO unset (side by
// side) it is exactly what a media player does to give each eye its full width: a buffer
// twice as wide, a window the ordinary shape. The compositor should draw a 1.6:1 window, not a
// 3.2:1 one, and each eye should still get its own half -- red and blue -- of the buffer.
// Built with viewporter-protocol.c alongside the others.
//
// SPATIAND_IDLE_FADE=1 asks the compositor to fade this surface out when the wearer stops
// paying attention to it, which is what a transport bar over a film wants. Paired with the
// harness's SPATIAND_IDLE_SECONDS it is the whole of that feature, end to end: the client says
// one word and the compositor does the rest.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "spatiand-xr-v1-client-protocol.h"
#include "viewporter-client-protocol.h"

// Buffer size. `SPATIAND_PROBE_SIZE=1280x264` makes it the shape a media player's transport
// bar takes -- same width as its window, a fifth of the height -- which is the case where a
// window's chrome stops being big enough to aim at. Anything that depends on a short wide
// surface is checked with that.
static int W = 640, H = 400;

static void read_size(void) {
    const char *spec = getenv("SPATIAND_PROBE_SIZE");
    int w, h;
    if (spec && sscanf(spec, "%dx%d", &w, &h) == 2 && w > 0 && h > 0) {
        W = w;
        H = h;
    }
}

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct spatiand_xr_v1 *xr;
static struct wp_viewporter *viewporter;
static struct wl_surface *surface;
static struct xdg_surface *xdg_surface_;
static struct spatiand_xr_surface_v1 *xr_surface;
static int painted = 0;
static int top_bottom_now = 0;
static int frames = 0;

// Counting frame callbacks, which is the only reliable way for this client to know the
// compositor has actually *drawn* its first shape. A wall clock does not work: the harness can
// take longer than any delay worth waiting to get round to its first frame -- XWayland starts
// first -- and a client that changes shape before the first shape was ever seen looks exactly
// like a compositor ignoring the change.
static void frame_done(void *d, struct wl_callback *cb, uint32_t t);
static const struct wl_callback_listener frame_listener = { .done = frame_done };
static void frame_done(void *d, struct wl_callback *cb, uint32_t t) {
    wl_callback_destroy(cb);
    frames++;
    struct wl_callback *next = wl_surface_frame(surface);
    wl_callback_add_listener(next, &frame_listener, NULL);
    wl_surface_commit(surface);
}

// Two halves of one buffer, in whichever packing was asked for.
static struct wl_buffer *make_split(int top_bottom) {
    int stride = W * 4, size = stride * H;
    int fd = memfd_create("stereo", 0);
    if (fd < 0 || ftruncate(fd, size) < 0) return NULL;
    unsigned int *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) return NULL;
    for (int y = 0; y < H; y++) {
        for (int x = 0; x < W; x++) {
            int first = top_bottom ? (y < H / 2) : (x < W / 2);
            px[y * W + x] = first ? 0xffe02020u : 0xff2040e0u;  // red, blue
        }
    }
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *b = wl_shm_pool_create_buffer(pool, 0, W, H, stride, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    return b;
}

static void refused(void *d, struct spatiand_xr_surface_v1 *s, uint32_t layer, const char *why) {
    fprintf(stderr, "probe: layer %u refused: %s\n", layer, why);
}
static void lost(void *d, struct spatiand_xr_surface_v1 *s, uint32_t layer) {
    fprintf(stderr, "probe: layer %u taken away\n", layer);
}
static const struct spatiand_xr_surface_v1_listener xr_surface_events = {
    .layer_refused = refused,
    .layer_lost = lost,
};

static void surface_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    xdg_surface_ack_configure(s, serial);
    if (painted) return;
    painted = 1;

    const char *mode = getenv("SPATIAND_STEREO");
    int top_bottom = mode && !strcmp(mode, "tb");
    top_bottom_now = top_bottom;
    int swap = mode && !strcmp(mode, "swap");

    if (xr) {
        struct spatiand_xr_surface_v1 *x = spatiand_xr_v1_get_xr_surface(xr, surface);
        xr_surface = x;
        spatiand_xr_surface_v1_add_listener(x, &xr_surface_events, NULL);

        const char *want = getenv("SPATIAND_LAYER");
        if (want) {
            uint32_t layer = SPATIAND_XR_SURFACE_V1_LAYER_WINDOW;
            if (!strcmp(want, "head_locked")) layer = SPATIAND_XR_SURFACE_V1_LAYER_HEAD_LOCKED;
            else if (!strcmp(want, "projection")) layer = SPATIAND_XR_SURFACE_V1_LAYER_PROJECTION;
            else if (!strcmp(want, "equirect180")) layer = SPATIAND_XR_SURFACE_V1_LAYER_EQUIRECT_180;
            else if (!strcmp(want, "equirect360")) layer = SPATIAND_XR_SURFACE_V1_LAYER_EQUIRECT_360;
            spatiand_xr_surface_v1_set_layer(x, layer);
            fprintf(stderr, "probe: asked for layer %s (%u)\n", want, layer);
        }
        spatiand_xr_surface_v1_set_eye_layout(x,
            top_bottom ? SPATIAND_XR_SURFACE_V1_EYE_LAYOUT_TOP_BOTTOM
                       : SPATIAND_XR_SURFACE_V1_EYE_LAYOUT_SIDE_BY_SIDE);
        if (swap) spatiand_xr_surface_v1_set_eye_swapped(x, 1);
        // Version 2. A compositor that only offers version 1 has the request but not this
        // one, and calling it there is a protocol error -- so it is guarded by what the
        // registry actually bound rather than by what the headers happen to declare.
        const char *anchor = getenv("SPATIAND_ANCHOR");
        if (anchor && wl_proxy_get_version((struct wl_proxy *) x) >=
                SPATIAND_XR_SURFACE_V1_SET_RESIZE_ANCHOR_SINCE_VERSION) {
            uint32_t which = SPATIAND_XR_SURFACE_V1_RESIZE_ANCHOR_CENTRE;
            if (!strcmp(anchor, "top")) which = SPATIAND_XR_SURFACE_V1_RESIZE_ANCHOR_TOP;
            else if (!strcmp(anchor, "bottom")) which = SPATIAND_XR_SURFACE_V1_RESIZE_ANCHOR_BOTTOM;
            spatiand_xr_surface_v1_set_resize_anchor(x, which);
            fprintf(stderr, "probe: anchored to %s\n", anchor);
        }
        const char *after = getenv("SPATIAND_IDLE_MS");
        if (after && wl_proxy_get_version((struct wl_proxy *) x) >=
                SPATIAND_XR_SURFACE_V1_SET_IDLE_AFTER_SINCE_VERSION) {
            spatiand_xr_surface_v1_set_idle_after(x, (uint32_t) atoi(after));
            fprintf(stderr, "probe: fade me after %sms\n", after);
        }
        if (getenv("SPATIAND_IDLE_FADE")) {
            if (wl_proxy_get_version((struct wl_proxy *) x) >=
                SPATIAND_XR_SURFACE_V1_SET_IDLE_FADE_SINCE_VERSION) {
                spatiand_xr_surface_v1_set_idle_fade(x, 1);
                fprintf(stderr, "probe: asked the compositor to fade me when unattended\n");
            } else {
                fprintf(stderr, "probe: this compositor is too old for set_idle_fade\n");
            }
        }
        fprintf(stderr, "probe: declared %s%s\n",
                top_bottom ? "top-bottom" : "side-by-side", swap ? ", swapped" : "");
    } else {
        fprintf(stderr, "probe: no spatiand_xr_v1 -- the compositor does not offer it\n");
    }

    // A destination, if one was asked for. Double-buffered like everything else, so it lands
    // in the same commit as the buffer it describes.
    const char *dest = getenv("SPATIAND_VIEWPORT");
    int dw, dh;
    if (dest && sscanf(dest, "%dx%d", &dw, &dh) == 2 && dw > 0 && dh > 0) {
        if (viewporter) {
            struct wp_viewport *vp = wp_viewporter_get_viewport(viewporter, surface);
            wp_viewport_set_destination(vp, dw, dh);
            fprintf(stderr, "probe: %dx%d buffer, %dx%d surface\n", W, H, dw, dh);
        } else {
            fprintf(stderr, "probe: no wp_viewporter -- the compositor does not offer it\n");
        }
    }

    // The buffer and the layout in one commit, which is the point of the layout being
    // double-buffered: there is never a frame of one without the other.
    struct wl_callback *cb = wl_surface_frame(surface);
    wl_callback_add_listener(cb, &frame_listener, NULL);
    wl_surface_attach(surface, make_split(top_bottom), 0, 0);
    wl_surface_damage(surface, 0, 0, W, H);
    wl_surface_commit(surface);
    fprintf(stderr, "probe: painted\n");
}
static const struct xdg_surface_listener surface_listener = { .configure = surface_configure };

static void ping(void *d, struct xdg_wm_base *b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_base_listener = { .ping = ping };

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver) {
    if (!strcmp(iface, "wl_compositor")) compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, "wl_shm")) shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, "spatiand_xr_v1")) {
        // Bind the newest version this client was built against that the compositor also has.
        // Binding a fixed 1 would silently give up set_idle_fade on a compositor that offers
        // it, which is the ordinary Wayland way to lose a feature without noticing.
        uint32_t want = spatiand_xr_v1_interface.version;
        xr = wl_registry_bind(r, name, &spatiand_xr_v1_interface, ver < want ? ver : want);
    }
    else if (!strcmp(iface, "wp_viewporter"))
        viewporter = wl_registry_bind(r, name, &wp_viewporter_interface, 1);
    else if (!strcmp(iface, "xdg_wm_base")) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm_base, &wm_base_listener, NULL);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = { global, global_remove };

int main(void) {
    read_size();
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) { fprintf(stderr, "probe: no display\n"); return 1; }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    if (!compositor || !shm || !wm_base) { fprintf(stderr, "probe: missing globals\n"); return 1; }
    surface = wl_compositor_create_surface(compositor);
    xdg_surface_ = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xdg_surface_, &surface_listener, NULL);
    struct xdg_toplevel *toplevel = xdg_surface_get_toplevel(xdg_surface_);
    xdg_toplevel_set_title(toplevel, "stereo probe");
    wl_surface_commit(surface);
    // Become a different shape part way through, which is what a media player does when its
    // window turns into a transport bar. This is the only way to exercise a resize anchor:
    // the anchor is about what happens *between* two shapes, so one shape proves nothing.
    //
    // Gated on frame callbacks rather than on elapsed time, for a reason worth writing down:
    // the first version slept, and the compositor's first frame came later than the sleep, so
    // the only shape it ever saw was the second one. From outside that is indistinguishable
    // from the anchor not working.
    const char *shrink = getenv("SPATIAND_PROBE_SHRINK");
    if (shrink) {
        // Wait until the first shape has been drawn a few times, not until a clock says so.
        while (frames < 3 && wl_display_dispatch(display) != -1) {
        }
        int w, h;
        if (sscanf(shrink, "%dx%d", &w, &h) == 2 && w > 0 && h > 0) {
            W = w;
            H = h;
            wl_surface_attach(surface, make_split(top_bottom_now), 0, 0);
            wl_surface_damage(surface, 0, 0, W, H);
            wl_surface_commit(surface);
            wl_display_flush(display);
            fprintf(stderr, "probe: became %dx%d\n", W, H);
        }
    }
    while (wl_display_dispatch(display) != -1) {}
    return 0;
}
