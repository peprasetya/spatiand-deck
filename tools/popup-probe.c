// A Wayland client that opens a window and a menu, and does nothing else.
//
// Written to answer one question without a person in a headset: when an application asks for
// an xdg_popup, does Spatiand draw it?
//
//   window: dark blue, 640x400
//   popup:  bright orange, 240x160, in the middle of the window
//
// An orange rectangle in the snapshot means popups work.
//
//   cc -o popup-probe popup-probe.c xdg-shell-protocol.c $(pkg-config --cflags --libs wayland-client)
//   SPATIAND_BACKEND=snapshot SPATIAND_CLIENT=./popup-probe SPATIAND_SNAPSHOT=/tmp/p.png spatiand
//
// (generate the two xdg-shell files with wayland-scanner; see the README note below)
//
// **It repositions the popup before painting, and that is the point.** The first version of
// this probe painted at the first configure and worked perfectly -- which is exactly why the
// bug it was written to find survived it. Every real toolkit creates a menu and then
// immediately repositions it, and a compositor that does not answer `reposition` with
// `repositioned` leaves the client waiting for permission to draw for ever. Qt does this;
// GTK does this; a hand-written client only does it if told to. So this one is told to.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct wl_surface *surface, *popup_surface;
static struct xdg_surface *xdg_surface_, *popup_xdg_surface;
static struct xdg_popup *popup_;
static int popup_made = 0;
static int repositioned = 0;
static int painted = 0;

static struct wl_buffer *make_buffer(int w, int h, unsigned int colour) {
    int stride = w * 4, size = stride * h;
    int fd = memfd_create("probe", 0);
    if (fd < 0 || ftruncate(fd, size) < 0) return NULL;
    unsigned int *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) return NULL;
    for (int i = 0; i < w * h; i++) px[i] = colour;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *b = wl_shm_pool_create_buffer(pool, 0, w, h, stride, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    return b;
}

// Wait for the answer to `reposition` before painting, exactly as a toolkit does.
static void popup_repositioned(void *d, struct xdg_popup *p, uint32_t token) {
    repositioned = 1;
    fprintf(stderr, "probe: repositioned (token %u)\n", token);
}
static void popup_position(void *d, struct xdg_popup *p, int32_t x, int32_t y,
                           int32_t w, int32_t h) {}
static void popup_done(void *d, struct xdg_popup *p) {
    fprintf(stderr, "probe: the compositor dismissed the popup\n");
}
static const struct xdg_popup_listener popup_events = {
    .configure = popup_position,
    .popup_done = popup_done,
    .repositioned = popup_repositioned,
};

static void popup_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    xdg_surface_ack_configure(s, serial);
    if (!repositioned) {
        fprintf(stderr, "probe: configured, waiting to be repositioned before painting\n");
        return;
    }
    if (painted) return;
    painted = 1;
    wl_surface_attach(popup_surface, make_buffer(240, 160, 0xffff8800), 0, 0);
    wl_surface_damage(popup_surface, 0, 0, 240, 160);
    wl_surface_commit(popup_surface);
    fprintf(stderr, "probe: popup painted\n");
}
static const struct xdg_surface_listener popup_listener = { .configure = popup_configure };

static void make_popup(void) {
    if (popup_made) return;
    popup_made = 1;
    struct xdg_positioner *pos = xdg_wm_base_create_positioner(wm_base);
    xdg_positioner_set_size(pos, 240, 160);
    xdg_positioner_set_anchor_rect(pos, 200, 120, 1, 1);
    xdg_positioner_set_anchor(pos, XDG_POSITIONER_ANCHOR_BOTTOM_LEFT);
    xdg_positioner_set_gravity(pos, XDG_POSITIONER_GRAVITY_BOTTOM_RIGHT);
    popup_surface = wl_compositor_create_surface(compositor);
    popup_xdg_surface = xdg_wm_base_get_xdg_surface(wm_base, popup_surface);
    xdg_surface_add_listener(popup_xdg_surface, &popup_listener, NULL);
    popup_ = xdg_surface_get_popup(popup_xdg_surface, xdg_surface_, pos);
    xdg_popup_add_listener(popup_, &popup_events, NULL);
    xdg_positioner_destroy(pos);
    wl_surface_commit(popup_surface);
    fprintf(stderr, "probe: asked for a popup\n");

    // And straight away move it, which is what every toolkit does and what this probe exists
    // to exercise. Nothing is painted until the compositor answers.
    struct xdg_positioner *again = xdg_wm_base_create_positioner(wm_base);
    xdg_positioner_set_size(again, 240, 160);
    xdg_positioner_set_anchor_rect(again, 200, 120, 1, 1);
    xdg_positioner_set_anchor(again, XDG_POSITIONER_ANCHOR_BOTTOM_LEFT);
    xdg_positioner_set_gravity(again, XDG_POSITIONER_GRAVITY_BOTTOM_RIGHT);
    xdg_popup_reposition(popup_, again, 7);
    xdg_positioner_destroy(again);
    fprintf(stderr, "probe: asked to reposition it (token 7)\n");
}

static void surface_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    xdg_surface_ack_configure(s, serial);
    wl_surface_attach(surface, make_buffer(640, 400, 0xff102040), 0, 0);
    wl_surface_damage(surface, 0, 0, 640, 400);
    wl_surface_commit(surface);
    fprintf(stderr, "probe: window painted\n");
    make_popup();
}
static const struct xdg_surface_listener surface_listener = { .configure = surface_configure };

static void ping(void *d, struct xdg_wm_base *b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_base_listener = { .ping = ping };

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver) {
    if (!strcmp(iface, "wl_compositor")) compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, "wl_shm")) shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    else if (!strcmp(iface, "xdg_wm_base")) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 3);
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
    xdg_toplevel_set_title(toplevel, "popup probe");
    wl_surface_commit(surface);
    while (wl_display_dispatch(display) != -1) {}
    return 0;
}
