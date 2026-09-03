# Spatiand

A 3D spatial desktop for the Steam Deck and XREAL Air glasses — a third mode
alongside Desktop and Game.

Windows hang on a sphere around you and stay where you left them when you turn
your head. Each one's sound comes from where the window is. The Deck's own
screen becomes a control panel for the session you are wearing.

**This is early.** It runs, it is used, and it is not finished. What follows is
an honest account of which parts do what.

---

## What works

**The desktop.** Stereo at 3840×1080@72 through the glasses, head-tracked, with
real Wayland clients as windows you can point at, move, resize and close. A 360°
environment behind them. A launcher, a HUD, and a virtual keyboard.

**X11 applications**, through XWayland — which matters more than it sounds,
because a great many programs have no Wayland support and never will. VLC is the
one this was built for.

**Spatial audio.** Each window gets its own audio sink; its channels are placed
around you and rendered to two ears through a measured head-related transfer
function. Turn towards a window and its sound turns with you. A 5.1 or 7.1 mix
keeps its shape: the front channels follow the picture, the surrounds swing
round behind you. Windows that make a sound grow a speaker on their title bar.

**Input.** The Deck's trackpads are two pointers with laser beams. The D-pad
types arrow keys into the focused window, A is enter, B is escape. A USB or
Bluetooth keyboard types; a mouse is a third pointer that fades when idle.

**The panel** is a touch sidecar: volume, brightness, audio device, a second
keyboard, and the way out.

## What does not work yet

Listed because finding out by hitting them is worse.

  * **Two-handed window gestures.** The geometry is written and tested; nothing
    consumes it, so moving and scaling with both thumbs does nothing.
  * **Fullscreen.** There is no fullscreen mode. Applications that expect one —
    Kodi, most players — lay out for the wrong size until told otherwise.
  * **Games.** The plan reserves a virtual gamepad and an escape gesture for
    this. Neither is built. A game will run and be unplayable.
  * **Per-application input mapping.** The controller mapping is one fixed
    table, not something you can change.
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

## How it is put together

Nine crates. The three that touch hardware — `spatiand-hmd`, `spatiand-input`,
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
| `spatiand-render` | GLES 3.2: stereo cameras, skybox, glass, text |
| `spatiand-shell` | scene graph, launcher, HUD, keyboard |
| `spatiand-audio` | spatial audio: geometry, HRTF, PipeWire |
| `spatiand-platform` | session install, launching, desktop settings |
| `spatiand-proto` | private Wayland protocols (scaffolded) |
| `spatiand` | the compositor itself |

`docs/xreal-air.md` is a protocol reference for the glasses, verified against
hardware. It is probably the most reusable thing here.

## Licence

MIT.
