# Remote applications and games

Spatiand as the client for applications running on another machine: one window each, not a
remote desktop. The server is `spatiand-host`, an independent, portable Linux daemon that owns
the applications and outlives the connection.

The design is in the plan; this file is the record of what has been **measured**, and of the
decisions those measurements forced. Everything below marked **[verified]** was run on the real
hardware on the date given.

Code: `spatiand-stream` (the wire, no GPU, no network — tested on any machine), `spatiand-host`
(the server), `spatiand::remote` (the client, inside the compositor).

---

## The two machines

| | Deck (client) | the host machine |
|---|---|---|
| OS | SteamOS 3.8.16 | Ubuntu 26.04, kernel 7.0 |
| GPU | AMD Van Gogh, Mesa 25.3 | AMD Strix Halo (Radeon 8060S), Mesa 26.0.3 |
| VAAPI decode | H.264, **HEVC, AV1** | all of them |
| VAAPI encode | — (not needed) | **H.264, HEVC Main/Main10, AV1** |

## What the hardware can do — 2026-09-17/18

**The Deck decodes HEVC and AV1 in hardware.** **[verified]** by feeding it x265 and SVT-AV1
streams and watching libavcodec choose the hardware path (`Format vaapi chosen by
get_format()`), not a software fallback. This was the open question that decided the codec
list.

**Decode is cheap, and the limit is a total, not a per-stream one.** **[verified]** decoding a
300-frame 1080p HEVC clip *n* times at once on the Deck:

| Streams | Wall time | Frames/s in total |
|---|---|---|
| 1 | 0.91 s | 330 |
| 2 | 1.63 s | 369 |
| 4 | 3.08 s | 390 |
| 6 | 4.55 s | 396 |

So the block saturates at **about 390 fps of 1080p**, wherever those frames come from: roughly
five windows at the glasses' 72 Hz, or one much larger picture. That is the number the
head-aware bitrate rule has to live inside — a room with a dozen remote windows in it works
only because the ones nobody is looking at are not being sent.

**Encoding is not the bottleneck.** **[verified]** on the host machine, 300 frames each, no B-frames:

| Codec | 1920×1080 | 2304×1296 |
|---|---|---|
| H.264 | 1.5 ms/frame | 2.0 ms/frame |
| HEVC | 1.4 ms/frame | 2.0 ms/frame |
| AV1 | 1.4 ms/frame | 1.9 ms/frame |

Those include a CPU→GPU upload the real path will not do, so they are an upper bound. HEVC and
AV1 cost the same as H.264 here, which means the codec can be chosen for what it does to the
*bitrate* rather than for what it costs to produce.

## What that settles

- **HEVC by default, AV1 where both ends have it, H.264 as the fallback.** The Deck decodes all
  three, this host encodes all three, and none of them is slow enough to matter.
- **There is no reason to send a window nobody is looking at.** 390 fps is a budget, and a
  headset knows exactly which windows are worth spending it on.

## The library question

**The Deck can build VAAPI code with nothing installed.** **[verified]** `cros-libva` compiles
and runs inside the `holo` container, which has neither `va.h` nor `libva.so`, by pointing it
at vendored headers and at the host's own libraries through the distrobox mount:

```text
CROS_LIBVA_H_PATH=<vendored libva 2.22 headers>
RUSTFLAGS="-L <dir with libva.so -> /run/host/usr/lib/libva.so.2>"
```

The probe printed the Deck's own driver string. Headers are pinned to **libva 2.22**, which is
what SteamOS ships; Ubuntu's 2.23 headers do not compile against `cros-libva` at all (its VP9
encode struct gained two fields), so pinning is required rather than tidy.

**`cros-codecs` cannot encode HEVC.** It has stateless encoders for H.264, VP9 and AV1, and
decoders for H.264, HEVC, AV1, VP8 and VP9. So the two ends use different libraries, each where
it fits:

- **Client decode: `cros-libva` + `cros-codecs`.** Pure Rust over libva, and it needs nothing
  installed on the Deck or in its build container.
- **Host encode: libavcodec's VAAPI encoders.** HEVC included, mature rate control, and an
  ordinary package on the ordinary Linux machines the host is meant to run on.

Both are VAAPI underneath, so the pipeline is the same shape either way. Writing an HEVC
encoder against `cros-codecs`' own traits would remove the second library and is worth
reconsidering if libavcodec ever becomes awkward to ship.

## The host runs applications — 2026-09-18

`spatiand-host` is a headless Wayland compositor with no shell, no window management and no
screen. **[verified]** on the host machine: it opened a socket, built a GLES renderer on
`/dev/dri/renderD128`, offered dmabuf v4 in **312 format/modifier pairs**, launched Chrome from
its catalogue, and saw the window appear and draw — all with no display, no seat and no desktop
session involved.

```text
spatiand-host --list          # what the catalogue holds
spatiand-host --run chrome    # start it and report what its windows do
```

**The frame callback is the throttle, and forgetting it looks exactly like a frozen
application.** The first run showed Chrome drawing two frames and then nothing, on a page with
a running animation. Nothing was stuck: a client draws again only when the compositor says it
may, and this one never did. With `spatiand_host::pace` wiring that up, the same page committed
**~125 frames per two seconds** — 62 fps at the placeholder 60 Hz.

That makes the callback the single place where "how often may this draw" is decided, which is
where it belongs:

| Window | Woken |
|---|---|
| The one being used | the session's own refresh (72 Hz on the glasses) |
| In view, not in use | half of it |
| Behind the wearer | about once a second |
| Nobody attached at all | about once a second, never zero |

Never zero, because an application that is never woken never finishes starting, never notices
it was resized, and shows a stale picture the moment the wearer turns back to it.

**The rate has to be counted from when a frame was *due*, not from when the loop got to it.**
The first version allowed a wake three quarters of an interval early, so a coarse loop would
not skip frames — and produced 84 wakes a second from a 72 Hz setting. Counting from the due
time keeps both the rate and the phase. There is a test for each of those mistakes.

## Pictures leave the GPU — 2026-09-18

The whole path runs: Chrome's window, composited, converted and encoded to HEVC without a
single pixel crossing the CPU. **[verified]** on the host machine — 480 frames written, decoded
afterwards, and the picture is the window, right way round and the right colours.

```text
spatiand-host --run chrome --encode /tmp/win.hevc
```

**Cost, at 1280×800:** **1.2 ms per frame**, of which ~0.9 ms is the GL composite and the rest
is handing the frame to the encoder. Occasional spikes to 45 ms, which is the thing to fix
next (below). A still page produces nothing at all; the numbers above are an animating one.

### The buffer belongs to the encoder, not to the compositor

The obvious arrangement — allocate a buffer with GBM, composite into it, hand it to the
encoder — **does not work**, and the failure is worth writing down because it cost an hour.
libavutil will not create a pool of dmabuf-backed frames at all: `av_hwframe_ctx_init` on a DRM
device answers `ENOSYS`, because a dmabuf is something it expects to *receive* from elsewhere,
not to allocate.

Turned around, every step is supported:

1. VA-API allocates a BGRA surface (libavutil's own pool).
2. `av_hwframe_map` lends it out as a dmabuf.
3. The dmabuf becomes a smithay `Dmabuf`, which the GLES renderer binds as a render target.
4. The window's whole surface tree is drawn into it.
5. `scale_vaapi` converts BGRA → NV12 on the GPU's postprocessor.
6. The VA-API encoder produces HEVC.

One allocation for the life of a window, no import per frame, and nothing copied in either
direction.

### Two other things that had to be right

- **A hardware filter graph cannot be configured from text.** `avfilter_graph_create_filter`
  with `pix_fmt=vaapi` returns `EINVAL`, because what a VA-API frame *is* lives in a frames
  context, not in an argument string. The source has to be allocated, given
  `av_buffersrc_parameters` carrying the frames context, and only then initialised.
- **The composite waits for the GPU before the encoder reads.** That wait is a real cost and
  the reason for the spikes: the thread does nothing while the GPU finishes. The fix is to hand
  the fence to VA-API instead, and it needs a second buffer to be worth anything, so it waits
  until there is a network to keep busy.

## Frames cross the network — 2026-09-18

**[verified]** host and probe on one machine: paired, connected, launched
Chrome by name, and received **773 frames with none lost**, reassembled from datagrams and
written to a file that decodes to the window.

```text
spatiand-host --fingerprint          # what this host is
spatiand-host --pair                 # trust the next session to connect, for two minutes
spatiand-host --paired / --trust / --forget
cargo run --release -p spatiand-host --example probe-session -- <host:port> <fingerprint> \
    --launch chrome --out /tmp/seen.hevc
```

`probe-session` is a session with no headset: it does what Spatiand will do, and is the way to
check the link without a compositor, a GPU or anybody wearing anything.

### Pairing is done over ssh, on purpose

`--pair` trusts the **next** session to connect and then closes the window. There is no PIN and
no web interface, because the shell you type that command into has already proved who you are —
anyone who can run it has an account on the host. A PIN exists to bootstrap trust between
machines with no shared context; ssh is that context, and it is stronger.

What that leaves for later is somebody with no shell — a person installing a package on a
machine with a screen — and for them a PIN shown on that screen slots into the same trust store
without changing anything else.

**Trust is a list of certificate fingerprints, and an empty list means nobody.** Not an address
range: `100.64.0.0/10` is shared carrier space, which a phone tether hands out, so "it is on
that network" says nothing about who it is.

### TLS is not optional and costs nothing

QUIC has TLS 1.3 in the protocol; there is no unencrypted mode and quinn will not start without
one. That is convenient rather than a burden — the certificate *is* the identity pairing pins,
so there is no separate authentication scheme to invent.

### Two things that cost time

- **A QUIC endpoint must be created inside its runtime.** Building one before entering the
  tokio runtime fails with "no async runtime found", from a line that does not look
  asynchronous at all.
- **A TLS 1.3 client finishes its handshake before the server has checked its certificate.** So
  an unpaired machine's `connect` *succeeds*, and the refusal arrives a moment later as the
  connection closing. Nothing may treat a connected socket as an authorised one; the first
  exchange is the proof. There is a test for exactly this.

## Over a real network, to the Deck — 2026-09-18

**[verified]** host on the other machine, `probe-session` on the Deck, over Tailscale on wireless:

| | Quiet page | Busy page (400 moving rectangles) |
|---|---|---|
| Frames | 1712 in 30 s (~57 fps) | ~40 fps |
| Bitrate | 1.5 Mbit/s | **24 Mbit/s**, at a 25 Mbit ceiling |
| Lost | **0** | **0** |
| Round trip | 15.7 ms idle | 30.5 ms under load |

Both decoded on the Deck afterwards and are the window, sharp, with the browser's own text
legible.

**The round trip doubles when the link is busy** — 15.7 ms to 30.5 ms — which is queueing on
the wireless path, not the host. It is the argument for the bitrate ceiling being a real
setting rather than decoration: the difference between those two numbers is felt by a head.

### The frame rate is the rate controller's divisor, and leaving it out quietly ruins the picture

The first run over the network sat at **1.5 Mbit/s against a 25 Mbit ceiling** and looked like
a bad video call: smeared blocks, unreadable text. Nothing reported an error.

`AVCodecContext.framerate` was left at zero, so libavcodec fell back to the time base — a
thousand frames a second — and gave every frame a thousandth of the ceiling. Setting it to the
session's display rate produced 24 Mbit/s and a sharp picture from the same code.

Worth knowing what that field *means* here: it is the number the ceiling is divided by, not a
promise about how often a window draws. Windows draw when they have something to show, which is
usually less often.

## The Deck decodes what the host encoded — 2026-09-18

**[verified]** end to end: the host composited Chrome, encoded it, sent it over Tailscale;
the Deck received 818 frames and decoded every one on its own GPU, and the picture is the
window.

**3.73 ms per frame** at 1280×800 (median 3.19, worst 11.18), decoded one frame at a time, the
way the link delivers them.

```text
probe-decode /tmp/deck-heavy2.hevc --out /tmp/frame.nv12 --frame 200
```

### The decoder is libavcodec too, after a false start

The first attempt used `cros-codecs` over libva: pure Rust, and it needs no ffmpeg at all,
which is exactly what a machine with no headers wants. It cannot run on this GPU.

`VaapiBackend::new` opens its decoder by creating a **16×16** probe context, and radeonsi
refuses any video context smaller than **64×64**. Measured directly, both codecs:

| Context | HEVC | H.264 |
|---|---|---|
| 16×16 | refused | refused |
| 64×64 and up | fine | fine |

Working around it means forking a 5 MB dependency over one constant. Using libavcodec at both
ends instead costs nothing extra, because the same trick that gave the Deck libva gives it
ffmpeg: **the libraries are there and only the headers are missing**. SteamOS ships the whole
of ffmpeg 7.1.1 with its `.so` symlinks, so the headers are vendored at that exact version and
the system's own libraries are linked:

```text
FFMPEG_INCLUDE_DIR=<vendored ffmpeg 7.1.1 headers>
FFMPEG_LIBS_DIR=<dir of symlinks into /run/host/usr/lib>
FFMPEG_LINK_MODE=dynamic          # without this it looks for static libraries and fails
```

The host builds against ffmpeg 8, the Deck against 7.1, and neither has to agree with the
other: the two ends agree on a *codec*, which is the only agreement a video format needs.

### A decoder must be told to stay on the GPU

`AVCodecContext.get_format` has to pick `AV_PIX_FMT_VAAPI`. Left alone, libavcodec chooses a
software format whenever one is offered and everything still works — the picture is correct and
every frame is being copied out of the GPU and back again.

### A file is not a stream of frames

Handing the whole file to the decoder produced **one** picture and "two slices reporting being
the first in the same frame". Nothing was wrong with it: a file has no frame boundaries, and
the decoder wants one access unit per call. `spatiand_video::split` finds them with
libavcodec's own parser. The link never needs this — a frame arrives as a frame — but a capture
written for inspection does.

## A remote window in the room — 2026-09-19

**[verified]** a Chrome window running on the host machine, drawn as an ordinary Spatiand window:
the observatory environment behind it, the status bar reading "1 window", a title bar saying
*chrome on <the host>:47600*, a close button, and the page animating inside the frame.

The client lives **inside the compositor** as an in-process Wayland client over a socket pair
(`crates/spatiand/src/remote/`), so a remote window is a real `wl_surface` and gets the title
bar, moving, resizing, focus and per-application layouts for nothing.

### What the wearer found that the logs did not

The first session in the glasses reported: the window appears, it can be moved, the picture is
frozen, and both CPU and GPU are pegged. The log said frames were arriving. Both were true,
and the cause was one bug:

**Buffers were only reclaimed on the path that shows a frame** — and that path is skipped while
the compositor is holding two of them. So it wedged at two after the first second, dropped
every frame after that (1759 in two seconds), and spun. Reclaiming on every pass of the loop
fixed it. Three related faults came out of the same session:

- **A frame dropped for being late was thrown away.** Safe only if another follows; for a window
  that has just gone still, none does, and the wearer keeps whatever was on screen for ever.
- **A session attaching to a still window was sent nothing at all,** because nothing had
  changed. Attaching now forces one fresh frame of every window.
- **A reattaching session was never told what its windows were streaming,** so it showed a
  window that could never paint.

### The unfinished part: handing a decoded picture to the compositor

This is not finished and the write-up is the honest state of it.

The pictures are right. Read back from the decoder, and again after the colour conversion, they
are pixel-perfect — there is a `SPATIAND_REMOTE_DUMP` that writes out the exact picture being
handed over, and it is the window. What the compositor samples from the same surface is often
**flat grey, or a smeared diagonal band of colour**. Its import succeeds, the format (`AR24`)
and modifier are ones it advertises, the stride matches, and the surface is synchronised with
`vaSyncSurface` before it is handed over.

Four arrangements were tried:

| | Result |
|---|---|
| Map the surface to a dmabuf every frame | wrong |
| Map once per surface and keep the mapping | **right — but leaks** |
| Keep a bounded cache and evict the oldest | wrong |
| Map, copy the descriptors, free the mapping at once | wrong |

The second works and cannot be kept: a mapping pins its surface, so the filter never reuses one
and allocates a fresh four-megabyte surface for every frame — 589 in one short run. And it is
not reliably reproducible, which points at synchronisation rather than lifetime.

**The way out is to stop exporting VA-API surfaces per frame at all**: hand the compositor the
*decoder's* NV12 buffers, which come from a fixed pool that is reused, and let the scene sample
them with an external YUV sampler. That deletes the colour conversion entirely — a whole GPU
pass per frame — and bounds the buffers by the decoder's own pool. It needs a second shader in
`spatiand::gl` (`samplerExternalOES`) and a flag on `WindowQuad`, and it is the next thing to
do.

### Input

Pointer, buttons, scroll and keys are carried and delivered to the window they were aimed at
(`spatiand_host::input`) — the host is a compositor, so there is no injection and no guessing
which window a click belongs to. Written and building; not yet confirmed by a click landing in
an application, because the picture had to be right first.

## Still to measure

- The zero-copy path at both ends: a client's dmabuf imported straight into the encoder, and a
  decoded frame handed to a `wl_surface` without a copy.
- Glass-to-glass latency, against Sunshine and Moonlight on the same machine and network.
- Whether a still window really does cost nothing.

## What the grey window actually was — 2026-09-19

**Not the buffer handover.** A day went into that theory, and it was wrong.

The picture on the glasses was flat mid-grey with faint square blocks in it. That is exactly
what an HEVC decoder draws when it decodes a run of frames *without the one they refer to*:
YUV 128/128/128 is what a missing reference comes out as, and the blocks are the changes each
later frame adds on top. The session loop had a rule — "if several frames arrive while one is
being decoded, the last one wins and the rest are dropped" — and it was applied to frames that
had **not been decoded yet**. A compressed frame is a difference from the one before it, so
dropping any of them corrupts every frame after it until the next keyframe. The log said so the
whole time: "142 dropped", "235 dropped".

It hid for so long because every check read back the **first** picture, which is a keyframe
and therefore perfect. Reading back the fortieth, and looking at what the compositor's texture
actually held, showed it. Now:

* every frame is decoded, in order; only *showing* a picture a newer one has overtaken is
  skipped;
* after a loss or a decode error nothing is decoded until a keyframe arrives, and the window
  keeps its last good picture meanwhile;
* a backlog past half a second is thrown away in favour of a fresh keyframe.

**And a host bug made it worse.** `refresh_all`, meant to send one frame of every window when a
session attaches, was never reset — so from then on every window was encoded on every turn of
the host's loop, changed or not: ~220 pictures a second at ~90 Mbit/s against a 25 Mbit
ceiling. On Tailscale that is loss, and every loss was a wait for a keyframe. It is reset after
one pass now, and the encode rate is also capped at the display rate per window, because
Chrome commits more often than it is asked to draw.

Measured afterwards, Chrome on a busy test page:

| | before | after |
|---|---|---|
| host frames per second | ~220 | ~58 |
| bitrate | ~90 Mbit/s | ~24 Mbit/s (25 ceiling) |
| shown on the Deck | ~6–30 fps, grey | ~57 fps, correct |
| skipped waiting for keyframes | ~70/s | none after the first |
| decode | 3–5 ms | 3.5 ms |

### What else changed on the way, and stays

* **The colour conversion writes into surfaces the session owns**: a fixed pool of five,
  linear, exported once, each lent to the compositor and returned on `wl_buffer.release`. The
  old `scale_vaapi` path leaked a four-megabyte surface a frame, and exported DCC-compressed
  surfaces (`DCC` + `DCC_RETILE`) as a single plane where the layout needs three. Whether that
  alone would have drawn wrongly was never settled, because the real fault was found first; the
  pool removes the question. See `spatiand-video/src/convert.rs`.
* **Exports go through `vaExportSurfaceHandle` directly**, with separate layers, the way mpv
  does, instead of libavutil's mapping.
* **`dmabuf::settle` warns when a client's buffer would import as an external texture**, which
  the scene cannot sample. It has never fired; it is one comparison and it would otherwise be
  invisible.

## Configuring a host from the headset — 2026-09-19

Every host offers **Host settings** first, before anything is configured. It is
`spatiand-host-config` (egui), installed beside `spatiand-host`, and it runs inside the host like
any other application, so it arrives in the headset as an ordinary window:

* **Applications** — the catalogue, each entry with its icon, and Open / Edit / Remove. Open asks
  the running host to start it (`launch <id>` on the control socket), so it is attributed and
  streamed exactly as if the headset had asked.
* **Add installed** — the machine's desktop entries, searchable, one press to add.
* **Edit** — name, program, arguments, working folder, environment, icon, window or VR, flat or
  3D, sound layout, controller profile, what happens when the headset leaves. Files and folders
  are chosen with a built-in browser, because a host has no file-chooser service and its screen
  is not where the person choosing is looking.
* **Network** — the bitrate ceiling (shared between open windows), frame-rate cap, idle rate,
  port.
* **Pair a headset** — the same code comparison as `spatiand-host --pair`, without a terminal.

It writes `apps.toml` and `host.toml` atomically; the host checks both once a second, re-serves
the catalogue to a connected headset, and restarts streams when the bandwidth changes. Measured:
an entry added to the file reached the headset's launcher in about two seconds, and its removal
likewise.

**Icons** are found by `spatiand-host-catalog::icons`, in order: the one chosen in the settings
app (a path or a theme name — this is what makes them replaceable), the application's desktop
entry matched by the program *after following links* (Chrome's two names are two links to one
file), and otherwise an image beside the program or one folder down (Firestorm's
`res-sdl/firestorm_icon128.png`). They cross the wire as 128-pixel PNGs; the headset keeps them
in `~/.cache/spatiand/remote-icons/` and uses them for the launcher bubble and the title bar.
The settings app carries its own icon, since no two icon themes agree on one.

**Titles** are the window's own, followed by the host's name: "heavy.html - Google Chrome —
workshop". They follow the application as it renames the window.

Protocol version is now **2**: catalogue entries carry the chosen icon beside the rendered one.

## First wearer test of the settings app — 2026-09-19

What was found wearing it, and what each turned out to be:

| Seen | Cause | Fix |
|---|---|---|
| Chrome grey, only animated parts drawn | On reattach the host sends one keyframe of a still window; it can overtake the message announcing the stream and be dropped. The session never asked again. | A stream with no keyframe asks every half second until it has one. |
| Black bar top and left, cut off right and bottom | The host drew each surface at its origin, ignoring the window geometry inside Chrome's shadow; and passed canvas-relative damage where element-relative is expected, which clipped the edges. | Drawn from the geometry; damage is each element's own. The host now also offers `xdg-decoration` and answers server-side, so Chrome and winit stop drawing shadows and frames at all. |
| Right-click and menus did nothing | Popups were never configured, so never shown, never drawn, and not clickable; a press on a menu's subsurface counted as outside it. | Configured on first commit, kept inside the window, drawn with the window, hit-tested before it, and dismissed by a press elsewhere. |
| L2 mapped as right-click missed | A layout's mouse button lands at the layout's own cursor, which started mid-window and never followed the laser. | The laser moves that cursor too. |
| Chrome stuck on an invisible dialog | Its update bubble is a popup (see above); GTK file dialogs could also go to the desktop's portal on the host's own screen. | Popups are shown; apps get `GTK_USE_PORTAL=0` and `GDK_DEBUG=no-portals`. |
| Host settings: black bars right and bottom | The damage clip above. | Same fix. |

Also found on the way: **holding Y to force-quit a remote window would have killed the session
itself**, because the remote client's credentials are the session's own. `pid_of` never answers
with the session's own pid now, and a remote window is force-quit on its host
(`ClientMessage::ForceQuit`), which kills the application's process group — apps are started
in their own group for this. And the host never reaped its children: every app that quit left
a zombie and a stale pid a later force-quit could have signalled.

The settings app gained **Running** (each window, with Close and Force quit) and **Restart the
host**; the control socket gained `list`, `close <window>`, `kill <app>` and `restart`.

Checked from the headset side with scripted clicks: a right-click opens Chrome's menu in the
picture, choosing Inspect opens DevTools; a second Chrome window arrives as its own window; Host
settings' Running and Applications pages fit their window with every button visible.

## Keys arrived two rows off — 2026-09-19

P typed 8. The wire carries evdev codes (what `wl_keyboard.key` already is), and smithay's
`KeyboardHandle::input` takes XKB codes, eight higher — its own libinput backend adds the 8.
The host subtracted 8 instead, so every key landed sixteen codes low: evdev 25 (P) became XKB
17, which is evdev 9, the 8 key. It adds 8 now.

## Sound — 2026-09-19

Each application the host launches gets a sink of its own, `spatiand-host.<app>`, and is told to
play into it through `PULSE_SINK` and `PIPEWIRE_PROPS` — inherited by children, so a browser's
audio process follows. The sink is a `pw-record` that declares itself `media.class = Audio/Sink`:
applications can pick it, and what they play comes out on its standard output. Nothing linked
into the host, nothing to install where PipeWire runs.

Silence is dropped on the host. Everything else goes as raw s16 48 kHz stereo (1.5 Mbit/s) on one
QUIC stream per application, opened when it first makes a sound (`spatiand_stream::audio`). On the
Deck each stream is played by a `pw-cat` aimed at that application's window sink — the same kind
of sink a local window gets — so it is placed at the window. At most 120 ms may wait; older sound
is dropped rather than letting it lag the picture.

Checked with `probe-session`: three seconds played into `spatiand-host.chrome` arrived as three
seconds of sound. Chrome clears its own `/proc/<pid>/environ`; its children show the variables.

## The grey came back: whole frames lost — 2026-09-19

The reassembler notices a frame with pieces missing, but not one that never arrived at all. Over
Tailscale on WiFi that happens, and the next frame went to the decoder with its reference gone:
`Could not find ref with POC …`, and the grey picture made of nothing but changes. Frames are
numbered per window, one after another, so any gap in the numbering now stops decoding until a
keyframe, exactly as a partial frame does. The log line says `N lost whole`. The QUIC datagram
buffers are larger too (4 MB out, 8 MB in); quinn drops datagrams silently when they fill.

## Sound on the speakers, stale pictures, a test page — 2026-09-19

- **Sound went to the Deck's speakers, not the window.** A toplevel is announced before its app
  id is set, so a remote window always arrived with none and never got its keyed sink. Windows
  that arrive without one are now looked at again each frame until they have one.
- **Flicker.** Sound over WiFi arrives in bursts, and was played the moment it came; every gap
  was a dropout. The player now gathers 60 ms before playing and again after running dry, and
  drops back to 60 ms in one step past 250 ms.
- **A picture that stayed broken.** The host skipped any window that had not changed, even when
  the session asked for a keyframe, so a broken stream on a still page waited until the page
  moved. A keyframe request is now always answered.
- **Chrome opened a test page.** The catalogue entry was still the one used for testing (a
  profile in `/tmp`, `file:///tmp/heavy.html`). It is plain `google-chrome --ozone-platform=wayland`
  now, on the user's own profile.

## Typing one keystroke late — 2026-09-19

In a terminal, "l" appeared when "s" was typed. Keys were delivered on time; the picture was
not. FFmpeg's VA-API encoders default to `async_depth=2` and hand a frame's packet out only when
the next frame is submitted — invisible on anything moving, one keystroke late on a still
terminal, because only the next keystroke made a next frame. The host sets `async_depth=1`
(ignored by FFmpeg before 5.0). Smithay's `send_keymap` log line on every key is its tracing
span, not a keymap being resent.

## X11 applications on the host — 2026-09-19

Firestorm's Linux build draws through GLX. Told `SDL_VIDEODRIVER=wayland` with no X server, SDL
made it a Wayland window and it crashed in `initGL` ("We're not running under X11? Wild."). The
host now runs its own XWayland and window manager (`crates/spatiand-host/src/xwayland.rs`, cut
down from Spatiand's), gives launched applications `DISPLAY` with Wayland still preferred by Qt
and GTK, and leaves SDL to choose. An X11 window is tracked, encoded, titled, closed and focused
like any other — focus as the X11 window itself, per `docs/x11.md`. Override-redirect windows
(X11 menus, tooltips) are not drawn yet. Checked: Firestorm reaches its login screen and streams
at ~50 fps.

The first deploy crash-looped: XWayland is a client smithay inserts with its own client data,
and `client_compositor_state` unwrapped the host's, panicking inside a Wayland callback that
cannot unwind. systemd restarted it into the same panic every three seconds for about a minute.

Sound: the Deck's player keeps its output for a minute of silence instead of three seconds —
the host sends nothing during silence, so every pause used to restart `pw-cat`, which glitches —
and logs `ran dry N time(s)` when it underruns, so a flicker leaves evidence.

## A green bar, unreachable buttons, and sound that stuttered worse — 2026-09-20

**The green bar down Firestorm's right edge.** A decoder's surface is padded to the codec's
alignment: 1421x954 lives in 1424x960. The Deck's colour conversion set no regions on its VA-API
pipeline, and no region means "the whole surface" — so the padding, uninitialised NV12 and
therefore green, was scaled into the picture. It now passes the picture's own rectangle as both
the source and the output region (offsets 8 and 24 of `VAProcPipelineParameterBuffer`, measured
from the libva 2.22 headers).

**Firestorm's Log In button could not be clicked.** X11's pointer lives on a screen and cannot
leave it, and XWayland took its screen from the host's output: 1280x800, while Firestorm's window
is 1421x954. Everything below 800 was unreachable, which is exactly where the login fields and
button are — and a double-click higher up landed fine, which is why it looked like selection was
working. The host now grows that screen in steps to hold the largest X11 window, as Spatiand does.

**Sound stuttering.** The jitter buffer added the previous round was the cause: it gathered 60 ms
before playing and did it again every time its queue emptied — but the queue empties constantly,
because everything in it goes straight into the pipe to `pw-cat`. The log said "ran dry 99 times
in 10 s". The pipe is the buffer (32 KB, ~170 ms) with `pw-cat` holding 40 ms; the queue only
bounds the delay. What remains is a counter for gaps over 250 ms, which are gaps that were heard.

## Resizing a window resizes the application — 2026-09-20

Dragging a remote window's frame only stretched the picture: the session sent
`ClientMessage::Configure`, which the host did not act on. It does now — an xdg toplevel is
configured, an X11 window gets a configure — so the application lays itself out for its new
shape, exactly as a local one does.

Two things had to follow it. The host already builds a new encoder when a window's size changes,
but only told the session about the new picture size when the window was *new*, so a session
would have kept a decoder and a quad sized to a stream that no longer existed; the size is now
announced whenever the encoder is rebuilt. And the session holds resizes back to one every
150 ms, always sending the last: a drag configures the window on every frame, and each size
would otherwise cost the host a new encoder and a keyframe.

Checked against a second host in a sandbox (its own `XDG_RUNTIME_DIR`, config and port, so the
running one was undisturbed): asking for 900x620 mid-session resized the terminal and the host
announced `Stream … 900x620`.

## The sound of an application that picks its own device — 2026-09-20

Firestorm had no sound at all, and the reason was in its own log:

```text
LLAudioEngine_FMODSTUDIO::init(): r_name="Ryzen HD Audio Controller Line Output"
```

FMOD enumerates the sound devices itself and opens one by name. `PULSE_SINK` — which is how
every application here is told where to play — only reaches a program that asks for "the
default output" and lets the system decide; it is invisible to one that has already chosen.
Both its own sink and Chrome's were in the list FMOD printed, and it took neither.

Nothing in an environment can reach that. The graph can: PipeWire will move a stream that is
already playing, which is what a volume-control panel does, and the session manager honours
it. So the host now watches the graph — `pw-dump` once a second — finds the streams belonging
to processes it started, and moves any that ended up somewhere else with `pw-metadata`. Only
its own applications' streams, so nothing the person at that machine is listening to is
touched, and three attempts per stream, so a disagreement with the session manager is one
warning rather than a fight.

**The value must be the sink's `object.serial`.** A node *name* is accepted by `pw-metadata`,
printed back as though it had been set, and quietly ignored by WirePlumber 0.5 — the stream
stays exactly where it was.

The same pass answers "is anything here recording?", which is what asks for the microphone.

## The wearer's microphone — 2026-09-20

A viewer with voice in it needs a microphone, and the only useful one is on the headset. It
goes up on a stream of its own with the same header sound coming down carries, mono at 48 kHz,
and becomes a **source in the host's graph**: `pw-cat --playback` declaring itself
`Audio/Source`, the mirror of the `pw-record` sink trick. Applications find it as
`PULSE_SOURCE`, and one that picks its own input device is moved to it the same way its sound
is.

**The device exists from the moment the host starts, and is quiet until the wearer speaks.**
It did not at first, and that was the whole bug: the source was built when the session began
sending, and nothing sends until something records, and nothing records from a device it
cannot see. Firestorm's own log has the miss in it, a second wide:

```text
15:48:28  LLWebRTCVoiceClient::addCaptureDevice : 'Ryzen HD Audio Controller Stereo Microphone'
15:48:28  LLWebRTCVoiceClient::addCaptureDevice : 'Ryzen HD Audio Controller Digital Microphone'
15:48:29  microphone: an application here is listening; asking the session for the wearer's
15:48:29  microphone: microphone at 48000 Hz x 1
```

Chrome showed the same thing from the other side: every *output* was in its list, including
all four of the host's own window sinks, and there was no extra input at all — because sinks
are made when an application starts and lasted, and the source was made on demand and did not.
So the source is opened at startup and fed silence when there is nothing else to feed it. This
is the rule the virtual gamepad already follows, for the same reason: **a program reads the
list of devices once.**

**A device in the graph is not an open microphone in somebody's room.** The two questions stay
separate. The source here is always present; the headset is only asked to *capture* while an
application is actually recording, which is still what the graph watcher decides, and
`remote_microphone = false` in the preferences means it never is at all. Silence costs 96 kB a
second on a pipe inside one machine and nothing on the link.

Nothing is left behind. `pw-cat` reads a pipe the host holds, so when the host goes the pipe
closes and it leaves with it, rather than staying in the desktop's sound menu as a microphone
that hears nothing.

**An application still has to choose it.** Firestorm reads its capture list at startup and
when its voice preferences open, so it wants a fresh launch and then *Preferences → Sound &
Media → Voice → Input device*. Left on the default it is caught anyway: a capture stream
belonging to an application the host started is moved to this source within a second by the
same watcher that moves sound.

**And the headset has to be recording something.** The device appearing on the host was only
half of it: the Deck was sending a perfectly good stream of digital silence. Every microphone
on this machine is listed twice — once as the ALSA device and once as a loopback copy of it
under `Filters:` — and **only the copy delivers frames**. Recording from the ALSA node gives a
header and not one sample, from either microphone, every time. The sound server's own default
pointed at a copy until Spatiand's picker overwrote it, because the picker lists the ALSA
nodes: they are the ones with a readable name. So choosing "Glasses Microphone" silently
turned off every microphone on the Deck, for anything, not just for this. `system::recordable`
is the fix and carries the measurement; the picker still shows the readable name and now sets
the default to the node behind it.

The lesson is the one worth keeping: **a device that a sound server lists is not necessarily a
device that yields sound.** Nothing in the graph said so — neither node was muted, both sat in
the same state, the stream opened, the header arrived and the host built its source. The only
symptom was silence. So `remote::microphone` now says in the log when what it is capturing is
all zeroes, rather than leaving that to be found by measuring the graph from another machine.

**Late sound is worse than missing sound.** The first working version arrived about half a
second behind the speaker — far enough that the gauge in Firestorm moved visibly after the
word. It was tempting to look for it in the audio graph, and it was not there: measured on
both machines with `pw-top`, every node in the path runs at a quantum of 512 samples or less,
about ten milliseconds. The delay was in the queues in front of the graph.

A pipe feeding a reader that consumes in real time **never catches up**. One burst — the link
delivering a lump it was holding, a scheduling hiccup, a retransmission — and every word after
it is late by that much for the rest of the session, because the far end takes one second of
sound per second and not a byte more. Nothing drains it. The old arrangement made this as easy
as possible: a queue thirty-two deep on the host, sixteen on the Deck, and a `blocking_send`
that would rather wait than lose anything.

So both ends now throw sound away instead of falling behind. The host keeps its own reckoning
of how much it has written that nobody has played yet and stops writing past sixty
milliseconds of it; the Deck keeps four chunks, forty milliseconds, and drops the overflow
rather than waiting on it. A gap in a sentence is easy to talk over. A voice half a second
behind a face is not.

**It is Opus now, not samples.** Mono at 48 kHz raw is 768 kbit/s spent on one person talking,
on the uplink, which is the direction with the least to spare; the same voice in Opus is about
24. Nothing was installed for it: the ffmpeg both ends already link carries Opus through
`libopus` — the Deck's `libavcodec` links `libopus.so.0` and so does this machine's — and the
two ends never had to agree on a library, only on a codec, which is the arrangement HEVC
already uses. The Deck builds against ffmpeg 7.1 and the host against 8, and neither cares.

Twenty milliseconds a frame, `application=voip`, and `packet_loss=5` so the encoder protects
itself rather than relying on a retransmission that would arrive after the moment for it had
passed. The delay a codec adds is one frame; that is nothing beside what a queue adds when a
link cannot keep up with raw. A machine whose ffmpeg cannot encode Opus says so in its log and
sends samples, and the header carries which it is, so neither end has to assume.

An application's sound going the other way stays raw. That is the downlink, which has room,
and a codec on it would cost delay on every window that makes a noise.

One thing this does not do yet: there is no indicator in the view saying the microphone is
live — the log says so at both ends, and nothing else does.

## One sink per application, not per window

Two YouTube videos in two Chrome windows are one sink and so one place in the room: Chrome
mixes them itself and the host sees a single stream. Which window that sink is aimed from is
decided by `audio::Source::rank`, and since 2026-09-20 by the window's **own pixels** rather
than by how big it looks — pulling a window smaller or pushing it away is something the wearer
does to see it better, and it must not hand the sound to a caption bubble standing next to it.
The log says which window each sink is aimed from whenever the answer changes.

## A controller for an application on another machine — 2026-09-20

Firestorm saw no joystick, because there was none: the wearer's controller is on the Deck and
nothing on the host could see it. The host now creates a gamepad of its own — a uinput device
wearing Steam's virtual gamepad identity, the same device `spatiand-pad` makes for local games
— and the session sends it the very report its local pad is given. So a remote application is
played exactly as a local one is: its own controller layout, the thumbsticks, and the head on
the four spare axes.

`virtual_pad` moved out of `spatiand-input` into a crate of its own for this. Both ends need
it and they have nothing else in common; the host is not going to depend on a crate full of
hidraw helpers for headsets.

**Eight axes, and the D-pad as buttons.** Second Life's joystick library, `libndofdev`, copies
SDL's axes into a fixed `axes[8]` and does not check the bound. A full pad has ten axes and a
hat — twelve values — so four of them are written past the end of that array, over the buttons
that follow it; the button loop then overwrites them, which is why nothing looked wrong and the
D-pad was simply invisible. The host's pad publishes exactly eight, which is also exactly the
order such a viewer expects to bind:

| axis | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|---|
| | left X | left Y | left trigger | right X | right Y | right trigger | rudder | wheel |

Rudder and wheel are `Report::extra[0]` and `[1]`, which is where an absolute head yaw and
pitch go — the two axes Firestorm's flycam layout wants. The other two spares have nowhere to
be on this shape and are not sent. The D-pad becomes four ordinary buttons after the eleven a
gamepad has, which makes it usable in a viewer for the first time.

**It is called "Spatiand Gamepad."** The name is what an application shows in its joystick
list, so it should say what the thing is. It keeps Steam's virtual-gamepad *identity* — the
vendor and product numbers, not the name — because those are what decide whether a runtime
hands the device to the application or keeps it for itself.

Three more things that had to be right:

- **The pad is created when the host starts**, not when the first report arrives. A program
  reads the list of joysticks once — Firestorm's `libndofdev` among them — and a device that
  appears afterwards is invisible to it.
- **The report goes only to the host whose window is in front**, and a host that loses focus is
  told the pad is at rest. A stick left pushed over walks an avatar into a wall.
- **Rumble is collected every turn of the loop**, whether or not a session is attached: the
  kernel blocks a game's force-feedback upload until it is answered, so a host that ignored
  them would hang the first game that rumbled. What comes back goes to the session, which owns
  the only motors here.

Launched applications get `SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1` — without it SDL
skips that identity — and `..._IGNORE_DEVICES_EXCEPT`, so an application here can never find a
controller plugged into the host and play with the wrong one.

**[verified]** the device appears on the host as `Spatiand Gamepad`, `/dev/input/js1`, and
`/proc/bus/input/devices` says it publishes X, Y, Z, RX, RY, RZ, RUDDER, WHEEL and no hat —
eight axes, in that order. What Firestorm then does with it is Firestorm's: it wants its
joystick switched on in its own preferences, and `Pads::apply` logs the first report that is
not at rest, which is the line that tells the two cases apart.

## A key that would not come up — 2026-09-20

A single click in Firestorm became a drag that never ended, and a single keystroke repeated
without stopping. Both were the same bug, and it was in the protocol rather than in either
end's input handling.

Everything the session said went out on **a stream of its own**: `connection.open_uni()`, one
message, finish. QUIC delivers each of those reliably — and, between streams, in whatever
order it likes. A key's press and its release are two messages. Under load, with video
datagrams filling the link, the release could arrive first; the host then applied the press
last and the key stayed down for ever. XWayland repeats a key it has not been told about, so
"down for ever" reads as "typing for ever". A mouse button that never came up is a drag.

The module's own comment said "reliable and ordered, because a key that arrives twice or out
of turn is worse than one that is late". It was reliable. It was never ordered.

Now there is **one stream, and everything the session says goes down it in order**, marked by
`CONTROL_MAGIC` and length-prefixed, written by one task from one queue — the mirror of the
control stream the host has always had coming the other way. `say` no longer waits for the
network, which also takes the write off the frame loop.

Two belts to go with the braces, because an input path that can leave something held down
should not depend on a single mechanism:

- The session lets go of every key and button it is holding when focus leaves the window.
  Wayland says no further key events arrive after a leave, so the release would never come.
- The host lets go of everything when a session disconnects, rather than leaving an
  application holding a button because someone walked out of range mid-click.

**[verified]** against a sandboxed host with `probe-session`: hello, catalogue, launch and a
mid-session resize all arrive over the one stream, and the host logs
`window 1 resized: 1280x800 -> 900x620`.

## Reading the link: what the cadence line says — 2026-09-21

Every two seconds the session writes one line per host to
`~/.local/share/spatiand-session.log`. It used to describe only the pictures. It now describes
the link as well, because a window that stops updating looks identical whether the network gave
up or the far end simply had nothing to send, and telling those two apart by hand cost an
evening:

```text
remote <host>: 100 shown in 2.0s, 2.3 ms decoding, 1 overtaken, 0 lost whole,
  10 skipped for a keyframe, rtt 11.1 ms, 12.4 Mbit/s down 0.03 up,
  0/9021 packets lost, 0 congestion, buffers held [3:1 2:2], 0 waiting to decode
```

`rtt`, throughput, packets lost and congestion events come from QUIC itself and are per
interval, not since the session began. **A stall with the round trip and loss unchanged is not
a network fault** — it is the host, or the application, having nothing to send.

### What seventy-six minutes of Firestorm actually measured

The first long session read with this in mind, and it is worth writing down because almost
none of it was where it was expected to be.

- **No disconnects at all.** Not one in seventy-six minutes, where the day before there had
  been eighteen. Fifteen seconds of tolerance was the whole of that fix.
- **The link is very good.** 159,349 frames shown, **12 lost whole**. Round trip `p50` 11.2 ms,
  `p90` 18.5 ms.
- **But it is not always good.** `p99` was 671 ms and the worst 2204 ms — 2.1% of samples over
  100 ms. Twelve of the thirty stalls line up with exactly those spikes. That is real, it is
  ours, and it is the thing still to fix: at a ceiling of 25 Mbit/s the link is being driven
  into a queue somewhere and the round trip goes with it.
- **The other eighteen stalls were not the link at all, and not this program either.** Every
  one sampled lines up with Firestorm's own trouble reaching Second Life — `Timeout was
  reached` from its HTTP layer, region crossings, teleports. Sixteen such timeouts during one
  cluster of them. Its own statistics for the session: 238 ms ping to the simulator and 5,262
  dropped packets. The viewer stops drawing, so there is nothing to encode, so the window
  freezes and the sound stops with it, and every number on our side stays perfect.

The lesson worth keeping is the third one. **A remote window freezing is not evidence about
the remote link.** Three separate networks are in play — the headset to the host, the host to
the internet, and the application to whatever it talks to — and only the first is ours.
