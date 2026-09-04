# Writing an application for Spatiand

What a program can rely on, what it has to do for itself, and which parts of
this are a specification rather than a promise. Written for a media player,
because that is the hardest case: it needs the picture in two eyes, the room
replaced, and eleven channels of sound put somewhere sensible.

Every section is marked:

  * **Works** — shipped, on hardware, today.
  * **Interim** — there is a way to do it, but it is not the way it will be
    done.
  * **Specified** — designed and written down here, **not built**. There is no
    code behind it. Do not write against it yet; do tell me if the shape is
    wrong, because nothing has been committed to.

At a glance, because the difference matters more than anything else in this
document:

| | State |
|---|---|
| Window sizing, fullscreen, no-fullscreen semantics | **Works** |
| Launching, audio routing, per-window sinks, mono→7.1.4 | **Works** |
| GPU buffers (`zwp_linux_dmabuf_v1`) | **Works** |
| Menus, popups, X11 compatibility | **Works** |
| Environment from image files | **Interim** |
| `spatiand_xr_v1` — stereo layouts, head-locked, equirect | **Works** |
| `spatiand_xr_v1` — the shared-memory pose channel | **Works** |
| `spatiand_xr_v1` — the `projection` layer | Refused, not built |
| OpenXR | Not a runtime — see [openxr.md](openxr.md) |

---

## 1. What kind of screen this is

**Works.**

There is one output. Its mode is the size of a *window*, not the size of the
glasses' framebuffer, and this is the single most important thing to know about
writing for Spatiand.

A window here is a quad hanging in a room. It has a pixel size, which is how
many pixels get stretched across it, and a world size, which is how large it
looks — and those two are unrelated. There is no screen for a window to fill,
so:

  * **`wl_output` reports 1280×800** at the rate the world is being redrawn
    (72 Hz on the glasses). That is the default surface size, not a display.
  * **Fullscreen is granted and changes nothing.** Ask for it and you will get
    a configure with the fullscreen state set and the size you already had. Most
    players respond by dropping their chrome and hiding the cursor, which is the
    useful half of the request and the reason it is granted rather than refused.
    Maximise behaves identically.
  * **You choose your own resolution** by committing a buffer of the size you
    want. Spatiand reads what you committed, not what it offered. A player that
    wants 1920×1080 for a 1080p film should simply commit 1920×1080; the quad
    keeps its width in metres and takes its aspect ratio from your surface.

The resolution worth committing is set by optics, not by the source. Each eye
sees 1920 pixels across about 40°, and a comfortable window occupies roughly a
third of that — call it 640 display pixels across. Committing 3840 wide buys
nothing you can see and costs fill rate on eight compute units. 1280–1920 is the
useful range.

## 2. Being launched, and being found

**Works.**

Launch through the Spatiand launcher (or the HUD) and your process is given an
environment that ties it to its window before it starts. Two variables matter:

```
PIPEWIRE_PROPS={ target.object = "spatiand.window.<slot>" }
PULSE_SINK=spatiand.window.<slot>
```

Both are set, because an application picks its audio protocol without telling
anyone: anything speaking PipeWire reads the first, and everything built on the
PulseAudio client libraries — Chrome, Firefox, a good deal else — reads only the
second. **Both are inherited by child processes**, which is what makes a browser
work: the audio process is not the process that owns the window.

Consequences worth designing around:

  * **Start your helpers as children.** A player that forks a decoder or a
    second process for audio gets the routing for free. One that talks to a
    system-wide daemon does not, and its sound will go to the machine's default
    output and never be placed.
  * **An application started from a terminal is not spatialised.** It gets a
    window and no sink. This is normal during development; run it from the
    launcher when testing audio.

`DISPLAY`, `QT_QPA_PLATFORM=wayland;xcb` and `GDK_BACKEND=wayland,x11` are also
set, so a toolkit prefers Wayland and falls back to X11 rather than the other way
round. See [x11.md](x11.md) for why you should want the Wayland path.

## 3. Frames on the GPU

**Works.**

`zwp_linux_dmabuf_v1` is offered, version 3, in whatever formats the renderer
can import — 321 format/modifier pairs on the Deck's Van Gogh. Use it. It is the
single largest thing you can do for a player's frame budget here.

The alternative is `wl_shm`, which means every decoded frame is copied by the
CPU into shared memory and then uploaded to a texture by us: about four
megabytes per frame at 1080p, sixty times a second, on eight compute units that
are already drawing the world twice. A hardware decoder produces a dmabuf
natively, so going through shm is not a fallback so much as a detour with a
copy at each end.

Nothing special is needed on your side beyond using it: EGL with
`EGL_WL_bind_wayland_display`, Vulkan WSI, GStreamer's `waylandsink`, mpv's
`gpu` output with a Wayland context, or VA-API surfaces exported with
`vaExportSurfaceHandle` all end up here. Verified with a Vulkan client
(`vkcube`) whose frames reach a quad in the room without a copy.

Three things worth knowing:

  * **Version 3, not 4.** Version 4's per-surface feedback exists to tell a
    client which formats would let its buffer go straight to the display
    controller without compositing. Nothing here can ever do that — every window
    is a texture on a quad sampled by a shader — so there is no feedback to give
    that would not be a lie. Your buffer is always composited.
  * **The import is tested before it is accepted.** If we cannot import a
    format you will be told `failed` rather than silently shown nothing, so a
    fallback path in your player will actually be reached. The answer comes one
    frame later than the request, because the renderer that decides it lives in
    the frame loop.
  * **Implicit sync only.** There is no `linux-drm-syncobj-v1` (explicit sync)
    yet, so the usual implicit fences on amdgpu are what order your rendering
    against our sampling. This has not been stress-tested against a decoder
    running flat out; if you see tearing inside a window, say so, because that
    is the shape it would take.

## 4. Being stereoscopic, head-locked, or the room itself

**Works.** `spatiand_xr_v1`, defined in
`crates/spatiand-proto/protocol/spatiand-xr-v1.xml` — that file is the
specification and this is the summary. `tools/stereo-probe.c` is a ~150-line
client that does all of it.

Bind the global, get an object for your surface, and set two things. Both are
double-buffered against `wl_surface.commit`, like everything else about a
surface, so layout and buffer land on the same frame and never one without the
other.

### Eye layout

`set_eye_layout(mono | side_by_side | top_bottom)` and `set_eye_swapped(0|1)`.

The two eyes sample opposite halves of the **same buffer** while the surface
keeps one position, size and orientation in the world. That is the whole
feature: one window with depth in it, not two windows.

Half-width and full-width side-by-side are not distinguished, because they are
not different — each eye gets its half stretched across the surface and only
the sharpness changes. Send the source as it is; do not unsqueeze it yourself.

Verified per eye rather than by eye: a probe paints one buffer red-left,
blue-right, the compositor renders each eye separately, and the centre pixel
reads (224, 32, 32) and (32, 64, 224) — the probe's own two colours.

### Layer

`set_layer(...)` says what the surface *is*:

| Layer | Meaning | State |
|---|---|---|
| `window` | An ordinary panel the wearer moves and keeps. The default. | Works |
| `head_locked` | Follows the view, keeping its angular size and position. | Works |
| `equirect_180` / `equirect_360` | The surface **is the room**. | Works |
| `projection` | You have rendered the two eye views to fill the view. | Refused |

`head_locked` moves the window in the *layout*, not just the drawing — so the
pointer, a drag and the pixels all agree. It is also what makes a client doing
its own head tracking possible: without it the compositor would track as well
and apply it twice.

The equirect layers are **exclusive** — there is one room. A second client
asking gets `layer_refused` with a reason. Ownership is claimed when the
request arrives rather than when it commits, because two clients asking in the
same frame have to get different answers. Post frames to the surface and they
become the sky; `set_yaw_offset` (microradians) turns a panorama to face
forward. Stereo works here too, so a VR180 over-under video is `equirect_180`
plus `top_bottom` and nothing else.

Nothing is remembered: the compositor looks for the environment surface every
frame. A client that crashes, stops posting, or gives the layer up cannot leave
the wearer inside a frozen image — the wearer's own environment simply comes
back on the next frame.

**A layer is either honoured or refused out loud.** A layer accepted and then
drawn as something else would be worse than one declined, because you would lay
yourself out for something you are not getting. There is a test asserting that.

## 5. Where the head is

**Works**, and it is the part to read carefully if you draw your own views.

`get_pose_channel` hands you a read-only file descriptor. Map it once, and the
head and per-eye poses are a memory read from then on — no round trips, no
per-frame protocol traffic. Read it as late as you can before submitting a
frame; that is late-latching, and it is most of what makes a head-tracked world
stay still.

The layout is in the XML and in `spatiand_proto::pose` if you are writing Rust.
It is a seqlock ring: read `write_index`, take the newest slot, read `seq`,
read the body, read `seq` again; if either read is odd or they differ, the
writer was in that slot — step back or retry. The short history is there so you
can interpolate to a predicted display time, which is exactly what
`xrLocateViews` needs.

Each slot carries `sample_ns`, a `predicted_ns` for the frame you are about to
draw, the head pose, and for each eye a pose and an `XrFovf`.

**The frame here is OpenXR's, not the compositor's**: +X right, +Y up, −Z
forward, quaternions in (x, y, z, w). The structures are field-compatible with
`XrPosef` and `XrFovf` on purpose, so an adapter is a memcpy. Spatiand's own
frame is different and is converted once, at this boundary.

If the session has no head tracking — no headset plugged in, or a nested
development session — you get `unavailable` with a reason instead of a channel.
Carry on as a flat window; do not wait.

You do **not** need this to show pre-packed stereo video, or to be the
environment. Those are the compositor's tracking, not yours. This is for
drawing your own geometry.

## 6. Audio

**Works**, and it is the part most likely to already do what you want.

Each window gets its own PipeWire sink, twelve channels wide. Your channels are
placed as speakers in the room around the wearer, and the whole ring is rotated
to face the window: put the window on your right and the front channels come
from the right while the surrounds swing round to the left. The result is
rendered to two ears through a measured head-related transfer function.

### Layouts

Recognised from the channel count of the format you negotiate:

| Channels | Layout | Wire order |
|---|---|---|
| 1 | mono | M |
| 2 | stereo | L R |
| 6 | 5.1 | L R C LFE Ls Rs |
| 8 | 7.1 | L R C LFE Ls Rs Lss Rss |
| 10 | 5.1.4 | L R C LFE Ls Rs Ltf Rtf Ltr Rtr |
| 12 | 7.1.4 | L R C LFE Ls Rs Lss Rss Ltf Rtf Ltr Rtr |

Anything else is refused and logged rather than guessed at — placing channels in
the wrong order is inaudible on a test tone and unmistakable on a film, so a
count nobody recognises is not worth a guess.

The sink is always twelve channels and **upmixing is off**, so a stereo song
stays two channels and a 5.1 film stays six. What a window's title bar shows is
not how wide the connection is but how many channels currently have anything in
them — a film's title bar says 5.1 during the film and stereo over the menu, on
its own.

### Where the channels go

  * **The front stage is the picture.** Front left and right sit on the
    window's own edges, wherever the window is and however wide it has been
    made. That is what keeps ordinary stereo sounding ordinary: the left channel
    comes from the left of the picture, not from a notional speaker beside a
    sofa you are not sitting on. Centre is dead ahead of the window; the front
    height channels are treated as front stage too, so a stretched stage stays
    coherent from top to bottom.
  * **Everything else is the room.** Sides, rears and rear heights keep the
    angles a listening room would give them, rotated with the window.
  * **LFE has no direction** and is mixed to both ears.

### Objects

**Not supported, and not for want of trying.** Atmos and DTS:X do not reach a
Linux application as objects — every decoder available renders them to channels
first. Send 7.1.4 or 5.1.4 and the height channels are placed properly, which is
most of what object audio buys on headphones. A protocol for per-object
positions would be straightforward to add to the geometry that already exists;
what does not exist is anything to feed it.

### Mute and status

The wearer can mute a window from its title bar. Your stream is not paused, told
about it, or otherwise disturbed — the mute is applied where the window's audio
is rendered. Do not try to reflect it in your own interface; you cannot see it,
and a player showing "muted" while the machine's volume is up would be wrong.

### One tuning knob that needs ears

How much plain stereo is folded back in alongside the spatialised render —
`audio_directness_centred` and `audio_directness_off_axis` in `prefs.toml`. Too
little and a centred window sounds hollow; too much and nothing moves when you
turn your head. This is a matter of taste and of which headphones, and it is not
solved.

## 7. Things that will bite you

  * **A window that never commits a buffer is invisible and counted.** It
    appears in the window count and the switcher and draws nothing. If your
    application launches and you see no window, that is the first thing to
    check — the log says which of the three reasons it is.
  * **There is no cursor confined to your window.** The pointer is a ray cast
    from the wearer's eye through a trackpad position; it can be pointing at
    another window, at the keyboard, or at nothing. Pointer grabs are not
    honoured. Design menus that dismiss on a click elsewhere rather than ones
    that trap the pointer until dismissed.
  * **Popups are drawn on your window's own plane**, a millimetre in front. A
    menu that hangs off the edge of its window is fine and normal.
  * **Text has to survive optics.** A window a third of the view wide has about
    640 display pixels across it. Interfaces designed for a monitor at arm's
    length are unreadable here; ones designed for a television across a room are
    about right.
  * **The session may be running with no head tracking at all** (glasses
    unplugged, calibration never run). Nothing about your application should
    depend on the wearer being able to turn their head to find something.

## 8. Asking for changes

The two protocols above are drafts, and the person most likely to find out that
they are wrong is whoever writes the first player against them. That is the
intended order: build the player, say what the protocol should have been, and
the protocol follows the player rather than the other way round.
