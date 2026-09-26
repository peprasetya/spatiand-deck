# Spatiand

A 3D spatial desktop for the Steam Deck and XREAL Air glasses — a third mode
alongside Desktop and Game.

Windows hang on a sphere around you and stay where you left them when you turn
your head. Each one's sound comes from where the window is. The Deck's own
screen becomes a control panel for the session you are wearing. Applications
running on another Linux computer arrive as windows of their own, beside the
Deck's.

**This is early.** It runs, it is used, and it is not finished. What follows is
an honest account of which parts do what.

---

## What works

**The desktop.** Stereo at 3840×1080@72 through the glasses, head-tracked, with
real Wayland clients as windows you can point at, move, resize and close. A 360°
environment behind them. A launcher, a HUD, and a virtual keyboard.

**X11 applications run**, through XWayland — VLC, Kodi, Firefox — which matters
because a great many programs have no Wayland support and never will. They are
**not supported**: X11's model of menus and screen positions does not survive
being put in a room, every X11 window says so on its title bar, and
[docs/x11.md](docs/x11.md) says exactly what breaks.

**3D applications.** `spatiand_xr_v1` lets an application say how its two eye
views are packed into one buffer, whether it follows the view, and whether it is
the room itself — a 180° or 360° picture, mono or stereo, or its own two eye
views filling the view, drawn behind every window. It also hands over head and eye
poses through shared memory, so an application drawing its own views can read
them at the last moment before it draws. A surface can also ask the compositor
to fade it out while nobody is attending to it and bring it back on a glance,
which is a thing a client cannot do for itself. An application that ignores all
of it is an ordinary window and stays one. See [docs/apps.md](docs/apps.md).

**GPU buffers.** Clients can hand over decoded frames as dmabuf rather than
copying them through shared memory — which is what a hardware video decoder
produces natively, and the difference between a video player that works and one
that spends its budget on memcpy. The global carries the render node's identity,
which is also how a client's EGL finds the GPU at all.

**Spatial audio.** Each window gets its own audio sink; its channels are placed
around you and rendered to two ears through a measured head-related transfer
function. Turn towards a window and its sound turns with you. Mono through
7.1.4 keeps its shape: the front channels sit on the window's own edges wherever
you put it, the surrounds swing round behind you. Windows that make a sound grow
a speaker on their title bar.

**No fullscreen, and applications are told so.** The one output a client can see
reports the size of a *window*, so a player sizes itself for what it was
actually given. Asking for fullscreen is granted at that size, which is what
makes Kodi drop its chrome and fill the window instead of laying out for a
framebuffer twice as wide as the world.

**Input.** The Deck's trackpads are two pointers with laser beams. The D-pad
types arrow keys into the focused window, A is enter, B is escape; in
Spatiand's own menus a held direction keeps moving, as a held arrow key does. A
window switcher in the HUD brings any window to the centre of your view. A USB
or Bluetooth keyboard types; a mouse is a third pointer that fades when idle.

**Copy and paste** work between Wayland and X11 applications, and between the
Deck and a remote host in both directions. Tested end to end across seven
applications on two machines — every copy pasted into every other.

**Games.** Every control, the four back paddles included, is remappable per
application in Settings → Controller layout, and what a game sees is one
gamepad wearing Steam's own virtual gamepad identity, so Proton games launched
through Steam pick it up. A Steam game starts on a plain gamepad layout,
anything else on the desktop one. A game gets a stereo sink placed on its
window. See [docs/input-mapper.md](docs/input-mapper.md).

**Applications on another computer.** `spatiand-host` runs on any ordinary
Linux machine and hands each application to the glasses as a window of its own,
not as a remote desktop. Pictures are encoded on that machine's GPU and decoded
on the Deck's (VA-API, HEVC or H.264), and a window that is not changing sends
nothing. Sound comes from the window, as a local application's does; keys, both
pointers, the microphone and the controller go the other way — the controller
as a gamepad of the host's own, played through the application's own layout.
Applications keep running when the glasses go and are handed back when they
return. A headset is paired once, by a code confirmed on the host, and is known
by its certificate afterwards. Every host offers its own settings first, as an
ordinary window that works with the pointer or with the D-pad, A and B.

An application there can also be the room: a Second Life viewer draws the world
around you and turns it with your head, while local windows float in front of it.
If its host goes quiet, the room is taken away within three seconds rather than
left frozen round you. See [docs/remote.md](docs/remote.md).

**The panel** is a touch sidecar: volume, brightness, audio device, a second
keyboard, and the way out.

## What does not work yet

Listed because finding out by hitting them is worse.

  * **Two-handed window gestures.** The geometry is written and tested; nothing
    consumes it, so moving and scaling with both thumbs does nothing.
  * **OpenXR.** Spatiand is not an OpenXR runtime and does not pretend to be
    one. [docs/openxr.md](docs/openxr.md) sets out what would have to be true
    and which of the three possible routes is worth taking.
  * **Games stutter while you point at them.** A game plays with the virtual
    gamepad, but resting a thumb on a trackpad over it makes Stumble Guys
    stutter until a few seconds after the thumb lifts. The session log now
    measures the game's own frame gaps to find out why.
  * **Remote: no webcam, no stream to OBS, and no latency figures yet.** Glass
    to glass has not been measured against Sunshine and Moonlight. Copying a
    file between machines copies its path, which does not exist on the other
    one.
  * **Object audio.** Atmos and DTS:X never reach the OS as objects on Linux, so
    what arrives is channels. Real object audio needs a protocol an application
    would have to speak; it does not exist yet.
  * **Anything but this hardware.** The traits and a null backend exist so that
    other headsets are a table entry rather than a rewrite. Nobody has tried one.

## Getting it

Download a release, unpack it, double-click **Install Spatiand**. See
[docs/install.md](docs/install.md) for what it does and what to check when
something is wrong.

## Tested on

Exactly one machine. Everything below is what it was, not what is required —
but it is the only combination anyone has seen work.

| | |
|---|---|
| Hardware | Steam Deck LCD, AMD Custom APU 0405 (Van Gogh) |
| Glasses | XREAL Air, first generation, USB `3318:0424` |
| OS | SteamOS 3.8.16, build 20260716.1 |
| Kernel | 6.16.12-valve24.5 |
| Graphics | Mesa 25.3.0 radeonsi |
| Audio | PipeWire 1.6.4, libmysofa 1.3.3 |
| X11 | Xwayland 24.1.10 |

The glasses need a USB-C cable that carries video. Many do not, and one that
charges perfectly well will leave the display black.

## Building it

Source is developed on another machine and built on the Deck itself, in an Arch
container, because SteamOS has no toolchain and an atomic `/usr` is no place to
put one.

    tools/setup-buildbox.sh          # once: an Arch distrobox with the deps
    cargo build --release            # inside it
    tools/make-release.sh            # a folder somebody can click

There is a headless renderer for looking at layout without hardware:

    SPATIAND_BACKEND=snapshot SPATIAND_SNAPSHOT=/tmp/hud.png SPATIAND_VIEW=hud spatiand

And a running session will take a picture of itself:

    kill -USR1 $(pgrep -x spatiand)

The snapshot backend can also *click*, which is the only way to open a real
application's menu without wearing anything:

    SPATIAND_BACKEND=snapshot SPATIAND_CLIENT=dolphin \
      SPATIAND_CLICK=0.09,0.03 SPATIAND_SNAPSHOT=/tmp/menu.png spatiand

`tools/popup-probe.c` is a ninety-line Wayland client that opens a window and a
menu and nothing else. It exists because menus were broken for a reason no
amount of reading found, and because the first version of it — which painted at
the first configure, like no real toolkit does — worked perfectly and proved
nothing.

## How it is put together

Sixteen crates. The three that touch hardware — `spatiand-hmd`, `spatiand-input`,
`spatiand-platform` — are the only ones whose *logic* knows what a Steam Deck or
a pair of XREAL glasses is. Everything else sees traits, so a second headset
should be a table entry rather than a rewrite.

    grep -ril 'steamos\|steamdeck\|xreal\|gamescope' crates/

hits more files than that, and all but one of them are comments: the reasoning
behind a constant frequently refers to the hardware it was measured on, and
hiding that would make the code less honest rather than more portable. The
exception is the prompt that asks you to plug the glasses in, which names them
on purpose — it is shown precisely when there is no headset to ask.

| crate | what it is |
|---|---|
| `spatiand-hmd` | headset: IMU, display modes, buttons |
| `spatiand-track` | head tracking: filter, bias, magnetic anchor, prediction |
| `spatiand-input` | controller, touchscreen, gestures, scroll |
| `spatiand-pad` | the one virtual gamepad a game sees, here or on a host |
| `spatiand-mapper` | controller layouts: bindings, templates, the editor's model |
| `spatiand-render` | GLES 3.2: stereo cameras, skybox, glass, text |
| `spatiand-shell` | scene graph, launcher, HUD, keyboard |
| `spatiand-audio` | spatial audio: geometry, HRTF, PipeWire |
| `spatiand-platform` | session install, launching, desktop settings |
| `spatiand-proto` | private Wayland protocols |
| `spatiand-stream` | the wire to a remote host: messages, pairing, video packets, the clipboard |
| `spatiand-video` | decoding a host's pictures on the Deck |
| `spatiand-host` | the remote application host, a small compositor of its own |
| `spatiand-host-catalog` | what a host keeps on disk: its applications and settings |
| `spatiand-host-config` | the host's settings, as an application the headset opens |
| `spatiand` | the compositor itself |

## Documentation

| | |
|---|---|
| [docs/apps.md](docs/apps.md) | Writing an application for Spatiand: window sizing, audio layouts, and the stereo and immersive-video protocols as they are specified so far |
| [docs/x11.md](docs/x11.md) | Why X11 runs and is not supported |
| [docs/input-mapper.md](docs/input-mapper.md) | Controller layouts and the virtual gamepad a game sees |
| [docs/openxr.md](docs/openxr.md) | Why Spatiand is not an OpenXR runtime, what it would take, and the order to do it in |
| [docs/remote.md](docs/remote.md) | Applications on another computer: what was measured, and what each measurement decided |
| [docs/install.md](docs/install.md) | Installing a release, and what to check when it goes wrong |
| [docs/xreal-air.md](docs/xreal-air.md) | The glasses' protocol, verified against hardware. Probably the most reusable thing here |
| [docs/steam-deck-controller.md](docs/steam-deck-controller.md) | The controller's HID reports |
| [docs/state.md](docs/state.md) | Where the session keeps things |

## Licence

MIT.
