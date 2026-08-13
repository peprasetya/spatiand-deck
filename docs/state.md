# Where Spatiand is

A working record: what is finished, and what is not. Kept short on the finished side — the
commit messages carry the reasoning, and `git log` is the place to look for *why* something is
the way it is. The unfinished side is deliberately detailed, because that is where the next
session starts and the thinking behind it is otherwise lost.

Last updated after the sidecar landed.

---

## Working

Verified on hardware unless noted.

**Display.** Owns the GPU with no desktop underneath. Glasses negotiate side-by-side by
modesetting at the native mode first (the link must be up before the MCU switch means
anything), forcing mono, then stereo. The double-width mode is matched by *shape*, not against
3840x1080 — the One Pro is 1920x1200 per eye. 72 fps sustained. Unplugging holds the session
and shows a waiting screen; replugging hands back.

**Head tracking.** Ported from HoloFrame. The axis map is measured, not guessed: gravity in
three poses gives up = sensor +Z, left = +X, forward = −Y. That is `AxisMap::XREAL_AIR` and it
is the default. Calibration exists in-world (never a terminal) with B to cancel; the HUD can
cycle the four valid pitch/roll interpretations, since a wrong one is a valid rotation and
nothing in the measurement can catch it.

**Controller.** Raw hidraw, every button confirmed by pressing it. Both pads point; right pad
click is left mouse, left pad right mouse, A/B/X duplicate them. Haptics work —
`ID_TRIGGER_HAPTIC_PULSE` (0x8f), pad byte 0 is right, 1 is left.

**World.** 360 environment (generated studio, plus five NOIRLab observatory panoramas), glass
bubble launcher with real Breeze icons and freedesktop categories, STEAM HUD, status bar with
clock and battery, on-screen keyboard, screenshots to `~/Pictures/Screenshots`.

**Windows.** Real Wayland clients as quads on a sphere, facing the viewer in yaw and pitch.
Title bar drag to move, left thumb for depth, two-thumb gesture to move and scale, D-pad to
cycle focus. Chromium-family apps get `--ozone-platform=wayland`, and Flatpak `@@u … @@` markers
are stripped.

**Sidecar.** Second `DrmCompositor` on the Deck's panel: CPU/GPU/memory graphs, clock, battery,
volume and brightness. Flip completions are tracked per-CRTC — a single flag lets one screen
consume the other's and that screen then never presents again, silently.

**Tooling.** `SPATIAND_BACKEND=snapshot` renders a frame to a PNG from a render node, with no
display, session or headset, and can host a real Wayland client while doing it. Almost every
layout bug in this project was found by looking at its output. `tools/probe-*.py` established
the controller map, the haptic command and the IMU axes.

294 tests. Build on the Deck in the `holo` distrobox; the Mac has no Linux graphics stack.

---

## Not done

### Touch input on the sidecar — next

The panel is a touchscreen and nothing reads it, which makes the volume and brightness
readouts exactly as useful as a photograph of a slider. This is also what an on-screen keyboard
*there* depends on.

The device is `i2c-FTS3528:00`, visible in `/proc/bus/input/devices`; the same hardware appears
as `hidraw3` but evdev is the right layer here because the kernel already assembles the
multitouch protocol.

Protocol B, so: `ABS_MT_SLOT` (0x2f) selects a finger, `ABS_MT_TRACKING_ID` (0x39) with −1
means that finger lifted, `ABS_MT_POSITION_X/Y` (0x35/0x36) move it, and `SYN_REPORT` ends a
packet — nothing should be acted on until then, or a half-updated position gets used. An
`input_event` is 24 bytes on 64-bit: 16 for the timeval, then `u16 type`, `u16 code`,
`i32 value`. Ranges come from `EVIOCGABS`, which is `_IOR('E', 0x40 + axis, …)` into six `i32`s
(value, min, max, fuzz, flat, resolution) — do not assume the panel's pixel size, because the
digitiser's coordinate space is its own.

Discovery should parse `/proc/bus/input/devices` for a name matching the panel and take its
`eventN` handler, the same approach `spatiand_hmd::hid` uses for hidraw. Match on the name, not
the number: `eventN` is not stable across boots.

The sidecar is laid out in landscape and rotated at the projection, so touch coordinates need
the same quarter turn applied — and that rotation is the thing most likely to be wrong in a way
that looks like a calibration problem. Worth a test that a touch at a known corner lands on the
widget drawn in that corner.

Once touch works, the volume and brightness bars become real controls
(`crate::system::Backlight::set` and `wpctl set-volume`), and the keyboard can move to the
panel where it does not eat a third of the field of view.

### Resizable windows — next

Distinct from the two-thumb gesture, which scales the *quad in the world*. Resize means sending
the client a new `xdg_toplevel` size so it gets more real estate — more terminal rows, not
bigger letters.

Design as discussed: thicken the window border so it is a grabbable target, and give the left,
right, bottom and bottom-corner zones their own cursor shapes, the way a 2D desktop does. The
top is the title bar and already means "move".

The zone test is arithmetic on the hit's UV and belongs next to `pointer::surface_position`,
where it can be tested. Sizing it in *degrees* rather than UV fraction matters: the same
fraction of a small window is a much smaller angle, and the border has to stay hittable with a
head-anchored ray — the title bar already had to be widened from 7.5% to 11% for exactly this
reason.

A resize drag then wants `toplevel.with_pending_state(|s| s.size = Some(…))` plus
`send_configure()`, and the world-space width should follow so the window's apparent size does
not jump when the client commits its new buffer.

### Games

The uinput virtual Xbox pad and gamescope. Designed for from the start — the input router owns
the controller exclusively and synthesises a pad, so a game never contends with us — but none
of it is built. `docs/steam-deck-controller.md` §3 explains why exclusivity is forced rather
than chosen.

### Smaller, and genuinely optional

- **Stereo per window** (`spatiand_stereo_v1`): the shader already takes a UV range, so an SBS
  source needs the protocol and a per-window layout, not new rendering.
- **Media player**: `spatiand-media` on libmpv. `Scene::set_sky` is the seam — playing a 360
  video is "swap the sky texture each frame and put the projection back afterwards".
- **`EvdevGeneric` / `NullHmd`**: the plan wanted a second backend early to prove the traits are
  not Deck-shaped. `NullHmd` exists; the generic input backend does not.
- **Curved window panels**, window persistence across sessions, per-game input profiles.

---

## Things that were true and cost time

Worth keeping, because each was invisible from the outside and none would be guessed twice.

- Smithay delivers `event.location - loc` to a client. Passing the surface-local position as
  both makes every delivered position (0, 0): motion appears to work and nothing is ever under
  the cursor.
- `Space::elements()` reorders when a window is raised, so an index into it is not an identity.
- A client texture arrives with GL's default `NEAREST_MIPMAP_LINEAR` filter. With no mipmaps
  that texture is *incomplete*, and an incomplete texture samples as opaque black — with no
  error anywhere.
- `XrealGlasses::drop` stops the IMU stream, and `hmd = open_any()` evaluates the new handle
  before dropping the old. Opening twice leaves a device that looks healthy and never sends.
- The glasses swap EDID when entering side-by-side. Nothing re-probes the connector unless
  something asks, and with no desktop running there is nobody to ask.
- MCU length fields count themselves. The controller's do not.
- One degree is about 2% of a window's height at 2.2 m. Anything sized as a fraction of
  something else needs checking in degrees before it is called a target.
