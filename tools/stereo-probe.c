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
//   cc -o stereo-probe stereo-probe.c xdg-shell-protocol.c spatiand-xr-v1-protocol.c \
//      $(pkg-config --cflags --libs wayland-client)
//   SPATIAND_BACKEND=snapshot SPATIAND_CLIENT=./stereo-probe SPATIAND_SNAPSHOT_EYE=left ...
//
// SPATIAND_STEREO=tb makes it top-and-bottom instead, and SPATIAND_STEREO=swap swaps the eyes,
// which is how the two remaining branches of the layout arithmetic get exercised against a
// real client rather than only in a unit test.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "spatiand-xr-v1-client-protocol.h"

#define W 640
#define H 400

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct spatiand_xr_v1 *xr;
static struct wl_surface *surface;
static struct xdg_surface *xdg_surface_;
static int painted = 0;

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
    int swap = mode && !strcmp(mode, "swap");

    if (xr) {
        struct spatiand_xr_surface_v1 *x = spatiand_xr_v1_get_xr_surface(xr, surface);
        spatiand_xr_surface_v1_add_listener(x, &xr_surface_events, NULL);
        spatiand_xr_surface_v1_set_eye_layout(x,
            top_bottom ? SPATIAND_XR_SURFACE_V1_EYE_LAYOUT_TOP_BOTTOM
                       : SPATIAND_XR_SURFACE_V1_EYE_LAYOUT_SIDE_BY_SIDE);
        if (swap) spatiand_xr_surface_v1_set_eye_swapped(x, 1);
        fprintf(stderr, "probe: declared %s%s\n",
                top_bottom ? "top-bottom" : "side-by-side", swap ? ", swapped" : "");
    } else {
        fprintf(stderr, "probe: no spatiand_xr_v1 -- the compositor does not offer it\n");
    }

    // The buffer and the layout in one commit, which is the point of the layout being
    // double-buffered: there is never a frame of one without the other.
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
    else if (!strcmp(iface, "spatiand_xr_v1")) xr = wl_registry_bind(r, name, &spatiand_xr_v1_interface, 1);
    else if (!strcmp(iface, "xdg_wm_base")) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm_base, &wm_base_listener, NULL);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = { global, global_remove };

int main(void) {
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
    while (wl_display_dispatch(display) != -1) {}
    return 0;
}
