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
| `spatiand_stereo_v1` — stereoscopic windows | **Specified. Not built.** |
| `spatiand_environment_v1` — application as the sky | **Specified. Not built.** |
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

## 4. Stereoscopic windows — one surface, two eyes

**Specified.** The renderer already samples a sub-rectangle of a window's
texture per eye; what does not exist is the protocol for saying which
sub-rectangle. Until it does, every window is drawn identically to both eyes.

### The model

Your window is a single Wayland surface with a single buffer, exactly as any
other window. A layout says how the two eyes' views are packed into it:

| Layout | Left eye samples | Right eye samples | Buffer aspect |
|---|---|---|---|
| `mono` | the whole buffer | the whole buffer | natural |
| `side_by_side` | `u ∈ [0, 0.5]` | `u ∈ [0.5, 1]` | double-wide |
| `top_bottom` | `v ∈ [0, 0.5]` | `v ∈ [0.5, 1]` | double-tall |

The quad's **3D transform does not change**. One window, one place in the room,
each eye seeing its own half of it. This is the whole feature: a 3D film in a
window looks like a window with depth in it, not like two windows.

Half-width and full-width SBS are the same thing here. A 1920×1080 `side_by_side`
buffer gives each eye 960×1080 stretched across the quad, and a 3840×1080 one
gives each eye 1920×1080; the sampling is identical and only the sharpness
differs. Send whatever your source is; do not unsqueeze it yourself.

### The protocol

```xml
<interface name="spatiand_stereo_v1" version="1">
  <request name="destroy" type="destructor"/>

  <!-- Get a stereo object for a surface. One per surface. -->
  <request name="get_stereo">
    <arg name="id" type="new_id" interface="spatiand_stereo_surface_v1"/>
    <arg name="surface" type="object" interface="wl_surface"/>
  </request>
</interface>

<interface name="spatiand_stereo_surface_v1" version="1">
  <request name="destroy" type="destructor"/>

  <!-- Takes effect on the next wl_surface.commit, like everything else. -->
  <request name="set_layout">
    <arg name="layout" type="uint" enum="layout"/>
  </request>

  <enum name="layout">
    <entry name="mono" value="0"/>
    <entry name="side_by_side" value="1"/>
    <entry name="top_bottom" value="2"/>
  </enum>

  <!-- Which half is the left eye. Some sources are swapped, and a viewer
       needs a control for it that does not mean re-encoding. -->
  <request name="set_swapped">
    <arg name="swapped" type="uint"/>
  </request>
</interface>
```

Double-buffered against `wl_surface.commit`, so the frame you change the layout
on is the first frame drawn with it. Switching layout mid-playback — a menu in
mono over a side-by-side film — is a supported thing to do and costs nothing.

### What to do today

**Nothing — none of this exists yet.** There is no `spatiand-proto` crate
contents, no global advertised, and nothing in the compositor that would answer
`set_layout`. What follows above is a design, not an interface.

That is deliberate. Rendering both eyes' views into your window
yourself would produce a squashed picture in *both* eyes, and unpicking that
later is worse than waiting. Ship mono until the protocol lands.

## 5. Immersive video — replacing the room

**Interim for files, specified for video.**

Spatiand's environment — the 360° image around the windows — is already
described by exactly the model a VR video needs:

  * **Projection:** `equirect_360` (full wrap) or `equirect_180` (front
    hemisphere; behind you there is no image, and the renderer says so rather
    than smearing the edge pixel round).
  * **Stereo packing:** `mono`, `over_under` (left eye on top — what almost
    every stereo 360 photograph and VR180 video uses, because it keeps full
    horizontal resolution), or `side_by_side`.
  * **Yaw offset:** rotate the panorama so its interesting part faces the
    wearer's forward.

Head tracking, per-eye sampling and the 180° edge handling are all working code
today. What is missing is a way for an application to be the source.

### What works today: files

Drop an image in `~/.local/share/spatiand/environments`, or point
`SPATIAND_ENVIRONMENTS` at a folder of your own, and it appears in the
environment picker. The projection and packing are guessed from the filename
first and the aspect ratio second:

| In the filename | Meaning |
|---|---|
| `180`, `vr180` | front hemisphere |
| `_ou`, `_tb`, `over-under`, `top_bottom` | left eye on top |
| `_sbs`, `side-by-side` | left eye on the left |

Failing a hint: an image near square is read as over-under, wider than 3:1 as
side-by-side, and anything between as mono. Name your files and you never have
to think about it.

This is enough to *test* a VR180 pipeline — decode a frame, write it out, look
at it — and nowhere near enough to play video with.

### The protocol

```xml
<interface name="spatiand_environment_v1" version="1">
  <request name="destroy" type="destructor"/>

  <!-- Ask to become the environment. The compositor answers with granted or
       denied; another application may already hold it. -->
  <request name="take">
    <arg name="id" type="new_id" interface="spatiand_environment_surface_v1"/>
    <arg name="surface" type="object" interface="wl_surface"/>
  </request>
</interface>

<interface name="spatiand_environment_surface_v1" version="1">
  <!-- Give the room back. Also happens automatically if the surface is
       destroyed, so a crash cannot leave the wearer inside a frozen frame. -->
  <request name="release" type="destructor"/>

  <request name="set_projection">
    <arg name="projection" type="uint" enum="projection"/>
  </request>
  <request name="set_stereo">
    <arg name="stereo" type="uint" enum="stereo"/>
  </request>
  <!-- Millidegrees, so a panorama can be turned to face forward without a
       floating-point argument in a protocol that has no floats. -->
  <request name="set_yaw_offset">
    <arg name="millidegrees" type="int"/>
  </request>

  <enum name="projection">
    <entry name="equirect_360" value="0"/>
    <entry name="equirect_180" value="1"/>
  </enum>
  <enum name="stereo">
    <entry name="mono" value="0"/>
    <entry name="over_under" value="1"/>
    <entry name="side_by_side" value="2"/>
  </enum>

  <event name="granted"/>
  <event name="denied">
    <arg name="reason" type="string"/>
  </event>
  <!-- The wearer took the room back through the HUD, or another application
       was granted it. Stop drawing to it; your window is still yours. -->
  <event name="revoked"/>
</interface>
```

The surface you hand over is an ordinary surface with ordinary buffers: post
frames to it and they become the sky. Your *window* stays where it is — the
common shape is a player whose window becomes the transport controls while the
film is all around, and hiding it is your decision, not the compositor's.

Two rules that are not negotiable, because they are what keeps a headset
comfortable:

  * **Frames are late-latched against head pose, not against your commit.** You
    supply a sphere; where the wearer is looking is not your business and must
    not be sampled by you.
  * **The environment is restored when you release it.** The wearer's previous
    choice comes back exactly as it was.

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
