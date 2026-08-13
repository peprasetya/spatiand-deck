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
consume the other's and that screen then never presents again, silently. The volume and
brightness bars are touchable (below).

**Tooling.** `SPATIAND_BACKEND=snapshot` renders a frame to a PNG from a render node, with no
display, session or headset, and can host a real Wayland client while doing it. Almost every
layout bug in this project was found by looking at its output. `tools/probe-*.py` established
the controller map, the haptic command and the IMU axes.

294 tests. Build on the Deck in the `holo` distrobox; the Mac has no Linux graphics stack.

---

**Sidecar touch.** The panel's digitiser, read through evdev — `spatiand_input::touch`. Found
by capability (`INPUT_PROP_DIRECT` plus the MT slot axes) rather than by name, so it is not a
Deck quirk. Protocol B decoded into contacts, with nothing acted on before `SYN_REPORT`.
Opened through libseat, because the event node is `root:input` with no ACL and only logind can
hand it over. Volume and brightness are real sliders; the bars went from 30 px to 56 px
because a landscape pixel here is 0.118 mm and a fingertip is 8–10 mm.

**Window resize.** The frame is a grab target — left, right, bottom and both bottom corners,
each with a turned double-arrow cursor. Pixels follow the world size at constant density, so
the client gets more buffer rather than a bigger picture of the same buffer: text keeps its
angular size and more of it fits. The opposite edge stays put.

294 → 337 tests.

---

## Not tested by a human yet

Everything above has tests and builds; these two have never had a finger or a thumb on them.

- **Sidecar touch.** Discovery, opening and the `EVIOCGABS` ranges are confirmed on the real
  panel (`cargo run --example touch-probe`, as root). What is not confirmed is the quarter
  turn, and the libseat open inside a live session. The sidecar draws a dot under every
  contact, which is the fastest way to tell the two apart: **no dot at all** means the device
  never opened; **a dot that mirrors the finger along one axis** means the turn in
  `Sidecar::touch_to_layout` has a sign wrong. Those are different bugs and they look
  identical from a description.
- **Window resize.** The frame renders at the right thickness (checked with the snapshot
  renderer) and the arithmetic is tested, but no edge has been dragged.

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
- **Build in the `holo` distrobox, not `spatiand`.** Both were made from `archlinux:latest`,
  but at different times: `holo` has glibc 2.41 and `spatiand` has 2.44, against SteamOS's
  2.41. A binary from the newer box builds perfectly and then dies on launch with
  `GLIBC_2.43 not found`, which reads as a broken build rather than an old host.
  `tools/setup-buildbox.sh` now checks this instead of printing it. `holo` needs
  `PATH=$HOME/.cargo/bin:$PATH` and its own `CARGO_TARGET_DIR` — the two boxes cannot share
  one, the fingerprints collide and the second gets a permission error.
- Rotating a basis by reusing the axis you just rotated is not a rotation, it is a skew. Both
  new axes have to come from the old pair.
