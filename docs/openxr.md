# OpenXR and Spatiand

Short answer: **Spatiand is not an OpenXR runtime, and making it one is not the
next thing to do.** This is what would have to be true, which of three routes is
worth taking, and why the media player should not wait for any of it.

## What "conformant" actually means

OpenXR conformance is a formal process, not a quality claim. A runtime passes
the Khronos Conformance Test Suite, submits the results, and signs the Adopter
Agreement; only then may it call itself conformant or use the mark. The CTS is
large and tests corners — swapchain format negotiation, session state machine
transitions, action-set binding, time conversion — that a runtime written to
support one application will get wrong without ever noticing.

So there are two very different goals, and it is worth being clear which one is
wanted:

  * **"OpenXR applications run under Spatiand."** Achievable. Discussed below.
  * **"Spatiand is a conformant OpenXR runtime."** A project in its own right,
    comparable in size to everything here so far, and one that should not be
    started before the first goal has been reached by the cheap route.

## What a runtime has to do

For scale, the parts an application actually exercises:

| Area | What it means |
|---|---|
| Instance and system | `xrCreateInstance`, extension negotiation, `xrGetSystem` — the loader's ABI, a JSON manifest, and a shared object with the right entry point |
| Graphics binding | Vulkan **and** OpenGL, because applications pick. Importing the application's images, or handing it ours |
| Swapchains | Allocation, format negotiation, acquire/wait/release, with the application drawing into images we own |
| Frame loop | `xrWaitFrame` / `xrBeginFrame` / `xrEndFrame`, frame pacing, predicted display time — this is where latency is won or lost |
| Spaces | View, local, stage; reference space recentring; `xrLocateViews` at a predicted time |
| Composition layers | Projection layers at minimum; quad, cylinder and equirect layers are what a media player would actually want |
| Actions | The input system: action sets, suggested bindings, interaction profiles |
| Session lifecycle | Idle → ready → synchronized → visible → focused, and every transition an application may be asleep through |

The tracker and the stereo camera model already exist here and are the parts
most projects find hardest. Everything above is the part nobody enjoys.

## Three routes

### A. Monado in a window — the cheap one

[Monado](https://monado.freedesktop.org) is the open-source OpenXR runtime, it
is conformant, and it already has an XREAL Air driver. It can present into a
Wayland surface. So: run Monado as an ordinary Spatiand client, and an OpenXR
application's output becomes a window in the room.

Two things make this work rather than half-work, and both are small:

  * **`spatiand_stereo_v1`** — the per-window side-by-side layout specified in
    [apps.md](apps.md). Without it Monado's stereo output is drawn identically
    to both eyes, which is to say flat.
  * **A head-locked window.** Monado does its own head tracking. If Spatiand
    also moves the window with the head, tracking is applied twice and the
    result is unusable. Pinning that one window to the view means Monado's
    tracking is the only tracking, which is correct.

The cost is latency. Two compositors in series — the application's frame reaches
Monado, Monado's frame reaches us, we late-latch and scan out — adds at least
one frame, 13.9 ms at 72 Hz, on top of whatever the application spends. For
seated media, a 360 player, a viewer, that is fine. For anything you move
quickly in, it is not.

There is also a device fight to settle: both Monado's XREAL driver and
`spatiand-hmd` want the same hidraw nodes. Either Monado's driver is disabled
and Spatiand feeds it poses (Monado has a remote/data-source driver for exactly
this), or Monado owns the glasses and Spatiand runs headless — which it cannot,
because it owns the display.

### B. Spatiand as a Monado target — the right one

Monado's compositor is pluggable: it has targets for direct-mode DRM, for
Wayland, for a debug window. A Spatiand target would let Monado do all the
OpenXR work while Spatiand keeps the display, the tracker and the room. One
compositor, one late-latch, no doubled tracking, no second frame of latency.

This is more work than A and much less than C, and it is where this should end
up. It also means the conformance question is answered by Monado rather than by
us, permanently.

### C. Our own runtime — only with a reason

Worth doing only if A and B both prove unworkable, or if something about the
spatial desktop turns out to need a runtime that knows about it. Writing it
would be a large project, and starting it before A has been tried would be
choosing the hardest route on a guess.

## The order to do this in

1. **`spatiand_stereo_v1`.** Needed by route A, by the media player, and by any
   3D content at all. Specified in [apps.md](apps.md); the renderer already
   samples a sub-rectangle per eye, so this is a protocol and a plumbing job
   rather than a rendering one.
2. **Head-locked windows.** A window property, small, and independently useful:
   a video you want to keep in front of you while you turn round wants it too.
3. **Monado in a window.** Measure the latency honestly before deciding it is
   good enough.
4. **`spatiand_environment_v1`.** The application-as-sky protocol. This is what
   makes 180/360 video a real feature rather than a folder of images.
5. **Reassess.** With 1–4 done there will be a real answer to "how bad is the
   extra frame", and that answer decides between B and living with A.

## The media player should not wait for this

Worth stating plainly, because it is the most likely wrong turn: a media player
for this desktop **should not be built on OpenXR**.

Everything it needs — a stereo window, a 180/360 environment, channel audio
placed in the room — is either shipped or specified in [apps.md](apps.md), and
reaching it through OpenXR would mean going through a runtime, a swapchain and a
composition layer to ask for something the compositor can be told directly. It
would also make the player depend on the one part of this that does not exist.

OpenXR matters for *other people's* applications — a VR viewer somebody else
wrote, a game, a tool. That is a real goal and worth the work in the order
above. It is not the path to your own player.
