# XREAL Air — low-level notes

Everything learned about talking to XREAL Air glasses directly, from work on a native
Android media player for the XREAL Beam Pro. Written so a separate project — a macOS or
Linux tool that switches the glasses between 2D and 3D and reads their gyroscope — can
start from facts rather than from scratch.

Every claim is tagged:

- **[verified]** — observed on real hardware (XREAL Air, PID `0x0424`, on a Beam Pro,
  August 2026). Bytes quoted are captures, not reconstructions.
- **[driver]** — read out of the open-source Linux driver's source. Reliable, not
  personally exercised.
- **[inferred]** — reasoning from the above. Treat as a hypothesis with a rationale.

---

## 1. What is actually possible

The glasses are a **USB HID device**. Both interesting capabilities are reachable from
ordinary userspace, with no kernel driver and no root:

| Capability | How | Status |
|---|---|---|
| Read the IMU (gyro, accel, magnetometer) | HID interface 3 | **[verified]** |
| Switch between 2D and side-by-side 3D | HID interface 4 | **[verified]** |
| Brightness, sleep, firmware version, glasses ID | HID interface 4 | **[driver]** |

They are also a **DisplayPort-Alt-Mode monitor**. That part is not controllable here: this
document is about the HID side channel that changes what the monitor does with the pixels.

**The one thing this does not get you** is compositing. Setting the mode is necessary and
not sufficient — see §8.

---

## 2. USB identity

**Vendor id `0x3318`** (13080 decimal) for every XREAL/Nreal device. **[driver]**

| Product | PID | IMU interface | MCU interface | IMU max payload |
|---|---|---|---|---|
| XREAL Air | `0x0424` | 3 | 4 | 64 |
| XREAL Air 2 | `0x0428` | 3 | 4 | 64 |
| XREAL Air 2 Pro | `0x0432` | 3 | 4 | 64 |
| XREAL Air 2 Ultra | `0x0426` | 2 | 0 | 512 |

**[driver]** — from `interface_lib/src/hid_ids.c`. The Air row is **[verified]**.

Descriptor strings on the Air: `product = "Air"`, `manufacturer = "Vendor"`. Do not match
on those. **[verified]**

---

## 3. Interface map (XREAL Air, `0x0424`)

Exactly as enumerated on the device — 9 interface entries including alternate settings:

```
iface#0 alt=0  class=1 (audio control)    sub=1   no endpoints
iface#1 alt=0  class=1 (audio streaming)  sub=2   no endpoints
iface#1 alt=1  class=1                    sub=2   OUT isoc 0x03 max 192
iface#2 alt=0  class=1 (audio streaming)  sub=2   no endpoints
iface#2 alt=1  class=1                    sub=2   IN  isoc 0x82 max 104
iface#3 alt=0  class=3 (HID)              sub=0   IN int 0x84 max 64, OUT int 0x05 max 64   <- IMU
iface#4 alt=0  class=3 (HID)              sub=0   IN int 0x86 max 64, OUT int 0x07 max 64   <- MCU
iface#5 alt=0  class=3 (HID)              sub=0   IN int 0x88 max 64, OUT int 0x09 max 64   <- unknown
iface#6 alt=0  class=3 (HID)              sub=0   IN int 0x8a max 3                          <- unknown
```

**[verified]**

Interfaces 0–2 are the built-in speakers and microphone; the OS claims them as a normal USB
audio device. **Interfaces 3 and 4 are the ones that matter.**

Interface 5 accepted a claim and an IMU-style start message but returned nothing in 20
reads. Interface 6 has no OUT endpoint and a 3-byte IN — most likely a HID consumer-control
endpoint for the physical buttons. Neither was investigated further. **[verified]**

---

## 4. Transport

Both protocols are HID reports over the interrupt endpoints of their interface. The
reference driver uses **hidapi**, so anything hidapi runs on works.

**Linux** — `hidraw`. Needs a udev rule for non-root access:

```
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="3318", MODE="0666"
```

Match the interface by `bInterfaceNumber`, not by usage page. hidapi exposes it as
`hid_device_info.interface_number`. **[driver]**

**macOS** — IOKit HID. Both worries below turned out to be unfounded.
**[verified on macOS 26.5, Intel MacBookPro16,1, XREAL Air, Aug 2026]**

- **No seize needed.** Plain `IOHIDDeviceOpen(dev, kIOHIDOptionsTypeNone)` succeeds on
  both interface 3 and interface 4. Nothing on macOS claims them.
- **No Input Monitoring permission needed.** Input reports arrive from an ordinary
  unsandboxed binary with no TCC prompt and no entitlement.
- **No hidapi needed.** `IOHIDDeviceSetReport(dev, kIOHIDReportTypeOutput, 0, …)` with
  report id `0` reaches the device, and `IOHIDDeviceRegisterInputReportCallback` +
  `IOHIDDeviceScheduleWithRunLoop` delivers the 64-byte reports.
- **Match on `bInterfaceNumber`**, which is *not* a property of the `IOHIDDevice` — it
  lives on the `IOUSBHostInterface` ancestor. Walk up with `IORegistryEntryGetParentEntry`
  in the `IOService` plane. Usage page will not discriminate: interfaces 3, 4 and 5 all
  report usage page `0x0041`, usage `0x0000`.

As enumerated on this machine — note interface 6 confirms the §3 guess that it is the
buttons:

```
iface 3  usagePage 0x0041  usage 0x0000  maxIn 64  maxOut 64   <- IMU
iface 4  usagePage 0x0041  usage 0x0000  maxIn 64  maxOut 64   <- MCU
iface 5  usagePage 0x0041  usage 0x0000  maxIn 64  maxOut 64   <- unknown
iface 6  usagePage 0x000c  usage 0x0001  maxIn  3  maxOut  1   <- Consumer Control
```

**Android** — `UsbManager`, no root. `openDevice`, then
`claimInterface(iface, force = true)`. The force flag is required: the system already has
the interfaces bound. **[verified]**

Writing: use the interrupt OUT endpoint when present (all of 3, 4, 5 have one). Fall back
to a control transfer `SET_REPORT` — `bmRequestType 0x21, bRequest 0x09, wValue 0x0200,
wIndex = interface number` — which is what hidapi does when there is no OUT endpoint.
**[verified for the OUT path]**

### Checksums

Both protocols use **CRC-32/ISO-HDLC** — the ordinary zlib/PNG CRC32. Polynomial
`0x04C11DB7` reflected, init `0xFFFFFFFF`, reflected in and out, final xor `0xFFFFFFFF`.
Java's `java.util.zip.CRC32` and Python's `zlib.crc32` both produce it unmodified.

Confirmed by construction: packets built with it were acted on by the glasses. **[verified]**

---

## 5. IMU — interface 3

### Starting the stream

Nothing arrives until asked. Write this to the OUT endpoint:

```
AA | CRC32(rest, 4 bytes LE) | 04 00 | 19 | 01
```

| Field | Bytes | Value |
|---|---|---|
| head | 1 | `0xAA` |
| checksum | 4 | CRC32 of everything after it, little-endian |
| length | 2 | `0x0004` — counts **itself + msgid + data**, exactly like the MCU's §6 length |
| msgid | 1 | `0x19` = `START_IMU_DATA` |
| data | 1 | `0x01` to start, `0x00` to stop |

The CRC covers the 4 bytes `04 00 19 01`. Total written: **9 bytes**.
**[verified on macOS, XREAL Air, Aug 2026]**

Real capture, accepted by the device — checksum is deterministic and reproducible:

```
aa c5 d1 21 42 04 00 19 01      # start
aa 53 e1 26 35 04 00 19 00      # stop
```

> **Correction — this section previously said length `0x0003`, "counts msgid + data".**
> Both halves were wrong, and a length of 3 is *rejected*. The device answers every
> malformed packet with one constant 12-byte reply:
>
> ```
> aa c1 7d 41 a9 05 00 ff 01 00 00 00
> ```
>
> Byte-identical for every msgid, so `0xFF` is "did not parse", not an ack — and its own
> checksum is plain zlib CRC-32 over `reply[5:10]`, five bytes, with the length field
> reading 5. That is what pins the rule: **length counts the whole body including its own
> two bytes**, the same convention §6 already documents for the MCU. Sweeping the field
> confirms it — lengths 0–3 return the canned reject, length 4 starts the stream.
> A correct packet is acked by echoing the msgid: `aa … 04 00 19 01`. **[verified]**

Other IMU message ids, all **[driver]**:

```
0x14 GET_CAL_DATA_LENGTH      0x17 WRITE_CAL_DATA_SEGMENT   0x1A GET_STATIC_ID
0x15 CAL_DATA_GET_NEXT_SEG    0x18 FREE_CAL_BUFFER          0x1D UNKNOWN
0x16 ALLOCATE_CAL_DATA_BUFFER 0x19 START_IMU_DATA
```

### Packet layout

64-byte reports arrive continuously on the IN endpoint — 20 reads returned 20 packets with
no gaps. **[verified]**

Real capture, glasses resting on a desk:

```
01 02 94 04 28 0a 4d ee 3a 02 00 00 a0 0f 00 00 00 01 e0 07 00 80 f2 ff e0 f4 ff 20 00 ...
```

**The complete map — every field now exercised on hardware. [verified]**

| Offset | Size | Field | Encoding |
|---|---|---|---|
| 0 | 2 | signature | `01 02` — a data packet |
| 2 | 2 | temperature | int16 LE |
| 4 | 8 | timestamp | uint64 LE |
| 12 | 2 | gyro multiplier = 4000 | int16 LE |
| 14 | 4 | gyro divisor = 16777216 | int32 LE |
| 18 | 9 | gyro X, Y, Z | 3 × int24 LE signed |
| 27 | 2 | accel multiplier = 32 | int16 LE |
| 29 | 4 | accel divisor = 16777216 | int32 LE |
| 33 | 9 | accel X, Y, Z | 3 × int24 LE signed |
| 42 | 2 | mag multiplier = 128 | int16 **BE** |
| 44 | 4 | mag divisor = 262144 | int32 **BE** |
| 48 | 6 | mag X, Y, Z | 3 × int16 LE, **offset binary — XOR `0x8000` first** |
| 54 | 10 | tail / unused | |

Scaling is `value = raw × multiplier ÷ divisor` per group. Gyro is **deg/s**, accel is
**g**, mag is **gauss**.

**Three independent confirmations that this layout is right**, glasses resting on a desk:

- accel magnitude reads **1.005 g** — gravity, to within half a percent
- mag magnitude reads **0.300 G = 30 µT**, mid-range for Earth's field (25–65 µT)
- gyro reads sub-degree rates (a stationary bias of about `+0.55, −0.80, −0.72` deg/s)

Getting any offset wrong destroys all three at once, so this also settles the ordering
question: **multiplier and divisor precede their values.** A summary of the driver source
says the reverse; the bytes and the physics both say otherwise.

Measured sample rate **~1067 Hz** (3203 packets in 3.0 s), continuous, no gaps.

The magnetometer's XOR mask is **`0x8000`** — the values are offset binary, so
`Int16(bitPattern: raw ^ 0x8000)` converts to two's complement. Without it every axis
pins near −32768 and the magnitude is nonsense. Its multiplier and divisor are
**big-endian** where every other group is little-endian. **[verified]**

The first one or two data packets after `START_IMU_DATA` arrive with **every sensor field
zeroed**. Discard them rather than feeding them to a filter. **[verified]**

A second signature, `AA 53`, appears on initialisation packets. **[driver]**

### Turning it into orientation

Raw rates only. Integrating them alone drifts. The reference driver feeds them to a
**Fusion** AHRS filter (gyro + accel, magnetometer optional) to get a stable orientation.
For a "look around a 360 video" use case, gyro-only integration with periodic recentre is
usually enough; for anything world-locked, use a proper filter. **[inferred]**

---

## 6. MCU — interface 4

Different framing from the IMU. Do not reuse the IMU's.

```
FD | CRC32 (4, LE) | length (2, LE) | timestamp (8, LE) | msgid (2, LE) | reserved (5) | data
```

| Field | Bytes | Notes |
|---|---|---|
| head | 1 | `0xFD` |
| checksum | 4 | CRC32, little-endian, over everything from `length` onward |
| length | 2 | **counts itself onward**: `2 + 8 + 2 + 5 + len(data)` = 18 for one data byte |
| timestamp | 8 | little-endian. Milliseconds since epoch works; not validated by the device |
| msgid | 2 | little-endian |
| reserved | 5 | zeroes |
| data | n | payload |

Total written for a one-byte payload: **23 bytes** (5 header + 18 counted). **[verified]**

### Setting the display mode

`msgid 0x0008` = `W_DISP_MODE`, one data byte:

| Value | Mode |
|---|---|
| `0x1` | 1920×1080 @ 60 — 2D |
| `0x3` | **3840×1080 @ 60 — side-by-side 3D** |
| `0x4` | 3840×1080 @ 72 — side-by-side 3D |
| `0x5` | 1920×1080 @ 72 — 2D |
| `0x8` | 1920×1080 @ 60 side-by-side |
| `0x9` | 3840×1080 @ 90 — side-by-side 3D |
| `0xA` | 1920×1080 @ 90 — 2D |
| `0xB` | 1920×1080 @ 120 — 2D |

`0x07` = `R_DISP_MODE` reads it back. **[driver for the table, verified for `0x1` and `0x3`]**

Two real captures, both acted on — the host display resolution changed within a second:

```
into 3D:  fd 10 3b 4b e1 12 00 91 fc f2 d0 9f 01 00 00 08 00 00 00 00 00 00 03
back to 2D: fd 0f 3e 93 6d 12 00 81 8b f6 d0 9f 01 00 00 08 00 00 00 00 00 00 01
             ^  ^---------- ^---- ^---------------------- ^---- ^------------- ^
             |  crc32       len   timestamp               msgid reserved       mode
            head                                          0x0008
```

The device replies on the IN endpoint. The second reply echoes the msgid at the same
offset, which is how to confirm it landed rather than guessing:

```
fd e5 d4 bf b7 12 00 81 8b f6 d0 32 e7 58 30 08 00 00 00 ...
                                             ^^ ^^ msgid echo
```

**[verified]**

### Other MCU messages

All **[driver]**, from `interface_lib/include/device_mcu.h`:

```
0x03 R_BRIGHTNESS          0x1A P_HEARTBEAT           0x29 R_ACTIVATION_TIME
0x04 W_BRIGHTNESS          0x1E W_SLEEP_TIME          0x2A W_ACTIVATION_TIME
0x07 R_DISP_MODE           0x21 R_DSP_APP_FW_VERSION  0x3C R_DP7911_FW_IS_UPDATE
0x08 W_DISP_MODE           0x26 R_MCU_APP_FW_VERSION  0x3D W_UPDATE_DP
0x15 R_GLASSID             0x16 R_DP7911_FW_VERSION   0x18 R_DSP_VERSION
0x19 W_CANCEL_ACTIVATION
```

Asynchronous events the glasses push, same framing:

```
0x6C02 P_START_HEARTBEAT   0x6C04 P_DISPLAY_TOGGLED   0x6C05 P_BUTTON_PRESSED
0x6C12 P_END_HEARTBEAT     0x6C09 P_ASYNC_TEXT_LOG
```

`P_DISPLAY_TOGGLED` and `P_BUTTON_PRESSED` are how a host learns the wearer pressed a
button — including the brightness-up long press that toggles 2D/3D. A tool that wants to
stay in step with the physical buttons should listen for these rather than poll.

The `0x3D`–`0x48` and `0x11xx` ranges are firmware update. **Leave them alone.**

Everything above `W_UPDATE_DP` can brick the device.

### Setting a mode the glasses are already in does nothing

**[verified, August 2026]** `W_DISP_MODE` is not idempotent in any useful sense. Writing the
mode the glasses are *currently* in is acked, and `R_DISP_MODE` reads back the expected
value, but no renegotiation happens on the DisplayPort side. The host keeps whatever timing
it learned when the link came up, so `3840x1080` never appears in the connector's mode list
and no DRM hotplug uevent is emitted.

Measured, glasses freshly plugged and reporting `0x01`:

```
write 0x04  -> acked, read back 0x04, 3840x1080 present within 2 s
```

Same command with the glasses already at `0x04`:

```
write 0x04  -> acked, read back 0x04, no mode change after 40 s
```

A forced sysfs re-detect (`echo detect > .../status`) does not help either, so this is not
the host caching EDID - the glasses simply do not re-drive the link.

**So a host that wants stereo must force a transition**: set a 2D mode, wait, then set the
side-by-side one. This matters more than it sounds, because the glasses keep their mode
across host reboots - after a crash they are still in SBS, and the naive "just set SBS" then
silently does nothing.

### Reply layout is not command layout

**[verified]** A reply to `R_DISP_MODE` (msgid `0x07`) has `length = 22`, not the 18 a
one-byte command uses, and the payload begins with a status byte:

```
fd 80f8726a 1600 6698baef1d6af420 0700 0000000000 00 04
                 ^len=22          ^msg ^reserved  ^  ^mode
                                                  status
```

The mode is at **offset 23**, with `0x00` at offset 22 meaning success. Reading offset 22 as
the value - the obvious choice given §6's command layout - reports `mode 0x00` for every
read, which looks like the device not answering rather than a layout mistake.

### Heartbeat

The driver sends `P_HEARTBEAT` periodically. Whether the glasses revert or sleep without
it was not tested — mode changes here held for minutes with no heartbeat at all. If a mode
proves not to stick, this is the first thing to try. **[inferred]**

---

## 7. The physical button

There is an MCU-adjacent flag governing whether the buttons may change display mode. In
XREAL's own SDK it appears as:

```
NRGlassesControlGetEnablePhysicalButtonSwitchDisplayModeFlag
NRGlassesControlSetEnablePhysicalButtonSwitchDisplayModeFlag
```

This matters practically: **the brightness-up long press stops toggling 2D/3D when host
software is managing the mode.** On the Beam Pro the button appeared dead; the cause was
almost certainly host software resetting the mode (§8) and/or this flag. The equivalent
raw MCU message id was not identified — it is not in the driver's public header, so it
would need finding by observation. **[inferred]**

---

## 8. Host software will fight you (Beam Pro specifically)

Relevant to Android; probably not to a Mac or Linux host, where nothing else wants these
interfaces. Recorded because it cost a lot of time.

- **Claiming interface 3 or 4 evicts XREAL's own service.** Its log shows
  `Device or resource busy`, then `Failed to reattach the driver to kernel`, then
  `[XREALPlugin] ForceKill`, and the AR launcher quits. It is one process or the other,
  never both. **[verified]**
- **The launcher restarts within seconds** — it has a USB attach receiver — and **resets
  the display mode** to 1920×1080. This is why a mode set from an app does not stick, and
  the best explanation for the button appearing dead. **[verified]**
- **Disabling the launcher does not help.** The mode then holds indefinitely, but the
  glasses display a **static placeholder image** instead of any app's output. The
  give-away was a system setting, `xreal_preview_png_path_3d`, pointing at
  `NebulaOS_3840.jpeg`. The logical display exists and an activity resumes on it; nothing
  is scanned out. **[verified]**

**Conclusion: on the Beam Pro, setting the mode is necessary and not sufficient.**
Compositing to the glasses belongs to XREAL's software. A host that drives the panel
itself — a Mac or a PC with the glasses as a plain DisplayPort monitor — has no such
problem: set 3840×1080 SBS, render side-by-side, done.

**This is why a Mac/Linux tool is the more promising project.** There, everything in this
document is sufficient on its own.

---

## 9. NRSDK, for the Android path only

If the target is Android and coexisting with XREAL's software rather than evicting it, the
supported route is their own SDK. From **XREAL Unity SDK 3.1.0**, the Android artefacts are
plain AARs and need no Unity:

- `Runtime/Plugins/Android/nr_api.aar` → `jni/arm64-v8a/libnr_api.so`, plus
  `libs/nrdisplay.jar`, `libs/nrcontroller.jar`, `libs/chameleon.jar`
- `nr_loader.aar`, `nr_common.aar`, `nractivitylife-release.aar`

### The dispatch table — signature verified

```c
long NRGetProcAddr(const char *name, void **out);   // returns 0 on success
```

**[verified]** — resolved on a Beam Pro from an ordinary app. Every name below returned
`ret = 0` with a valid pointer inside `libnr_api.so`; a deliberately fake name returned
`ret = 1, out = NULL`, so the table does not answer indiscriminately. `dlopen` of
`libnr_api.so` succeeds with no licence check at load time, and needs `libnr_libusb.so`
(from `nr_common.aar`) alongside it.

```c
void *lib = dlopen("libnr_api.so", RTLD_NOW);
long (*get)(const char *, void **) = dlsym(lib, "NRGetProcAddr");
void *fn = NULL;
if (get("NRGlassesControlSet2D3DMode", &fn) == 0) { /* fn is callable */ }
```

`libnr_api.so` exports only **36 symbols**; the real surface is ~910 entry points resolved
by name through that. It also exports `xrNegotiateLoaderRuntimeInterface`, so it doubles as
an **OpenXR runtime** — a documented, header-bearing alternative worth weighing against the
private ABI if a project needs full stereo rendering rather than just these two features.

The relevant ones:

```
NRAPICreate / NRAPIStart / NRAPIPause / NRAPIResume / NRAPIStop / NRAPIDestroy
NRAPIInitSetLicenseData            <- implies a licence gate
NRGlassesControlCreate / Start / Stop / Destroy
NRGlassesControlSet2D3DMode        <- the supported mode switch
NRGlassesControlGet2D3DMode
NRGlassesControlGetSupportedDisplayModeMask
NRGlassesControlSetNotifyDisplayChangeCallback
NRGlassesControlSetEnablePhysicalButtonSwitchDisplayModeFlag
NRGlassesControlSetKeyEventCallback
NRHeadTrackingCreate / Destroy / Recenter / DeepRecenter
NRHeadTrackingAcquireHeadPose
NRHeadPoseGetPose / GetVelocity / GetBias / GetHMDTimeNanos / GetTrackingReason

# Raw sensors - the same IMU §5 reads over HID, but without evicting anyone
NRImuCreate / NRImuStart / NRImuStop / NRImuPause / NRImuResume / NRImuDestroy
NRImuSetCaptureCallback            # push, rather than polling
NRImuDataGetGyroscope / NRImuDataGetAccelerometer / NRImuDataGetMagnetometer
NRImuDataGetHMDTimeNanos
NRImuSetFrequencyExtBase
NRHMDGetIMUGyroscopeBias / NRHMDGetIMUAccelerometerBias
NRImuCalibrationStart / Stop / QueryPitch / PhaseStart / PhaseQueryRate
NRGlassesControlSetIMUFrequencyDivider / GetImuInterruptCount
```

**These read the glasses' IMU, not the host's.** Three independent confirmations: the
naming (`HMD` = the head-mounted display), `NRGlassesControlSetIMUFrequencyDivider` being
a *glasses control*, and - decisively - XREAL's service logging
`Control IMU stream status ... Device or resource busy` at the exact moment this project
claimed **HID interface 3**. NRSDK is that interface's consumer. **[verified]**

So on Android there are two routes to the same sensor: take interface 3 directly (§5) and
evict XREAL's software, or ask NRSDK for it and coexist. The second is strictly better
where NRSDK is present. On a Mac or Linux host, where it is not, §5 is the only route -
and is sufficient.

`NRHeadPoseGetPose` is the better call for looking around a 360 video: already fused and
drift-corrected. `NRImuDataGetGyroscope` is there when raw rates are actually wanted.

Plain-Java, no JNI shim needed:

```java
com.xreal.sdk.display.GlassesDisplay          // in nr_api/libs/nrdisplay.jar
    GlassesDisplay(Activity, long)
    SurfaceView getSurfaceView()
    void startDisplay(long)  /  void stopDisplay(long)
    native int nativeRunOnGlassesDisplay(long, View)   // put a View on the glasses
com.xreal.sdk.display.GlassesListener
```

The `long` arguments are native session handles, so the native side must be initialised
first — the Java classes are not usable alone.

### Becoming a "glasses app" rather than a window in someone else's space

There is no separate platform here. XREAL's own video player — the one that switches 2D/3D
on the fly — is an ordinary Android app that links these AARs. What it gets from them:

- **`GlassesDisplayPlugEvent-2.4.2.aar`** ships `GlassesInitProvider`, a `ContentProvider`
  under `${applicationId}.glassesdisplayplugevent.provider`. A `ContentProvider` is created
  before `Application.onCreate`, so **merely depending on this AAR wires up the glasses
  connection at startup** with no code. It also has the USB attach/detach receiver (present
  but commented out in the manifest, registered dynamically instead).
- **`nractivitylife-release.aar`** provides the Nebula activity lifecycle —
  `ai.nreal.activitylife.NRFakeActivity`, seen running on the device.

**One caveat.** `nractivitylife` requests `android.permission.ACTIVITY_EMBEDDING`, which is
signature-level; their own manifest marks it `tools:ignore="ProtectedPermissions"`, so they
know it is not granted to everyone. A sideloaded third-party app will not get it. Whether
the parts we want — 2D/3D switching and the IMU — need it is **[inferred: probably not]**,
since those are glasses-control and sensor calls, not window management. Test before
depending on the activity-life layer. `SYSTEM_ALERT_WINDOW`, also requested, is
user-grantable.

**Costs.** No C headers ship, so signatures and struct layouts must be derived from the
binary. SDK 3.1.0's C# layer does not expose these (it moved to an XR plugin), so it is not
a source of signatures either. And `NRAPIInitSetLicenseData` suggests a licence gate whose
strictness is unknown.

### Head-locking the view, for 180/360 playback

A spherical video needs the opposite of what an AR launcher gives you. The launcher
**world-locks** the app's window — it stays put in the room while you turn — so head motion
moves the window. Panning a sphere needs the view **head-locked**, so head motion moves the
*content* instead. Two candidates, neither tested:

- **`NRHeadTrackingSetCoordinateMode`** — the cheap one to try first. NRSDK carries a
  notion of reference space, and this is the only call that names it.
- **The compositor API** — `NRRenderingBeginFrame`, `NRRenderingAcquireFrame`,
  `NRFrameSetRenderingPose`, `NRFrameSetBufferViewport`, `NRFrameGetViewportCount`,
  `NRFrameSetColorTextures`, `NRFrameCompose`, `NRFrameSubmit`. This is what makes an app
  *the* immersive app rather than a window inside someone else's space: it submits its own
  frames with its own pose, so head-locked versus world-locked is simply which pose is
  passed to `NRFrameSetRenderingPose`. Correspondingly larger — it is an XR render loop,
  and it displaces the launcher's compositing rather than sitting alongside it.

**[inferred]** — from naming only. If neither works, panning from the *host's* gyroscope
still gives a usable 180/360 viewer for a hand-held device; it is only wrong when the host
is pocketed and the head is what moves.

### Calling — signatures established on hardware

Two things must be right before any of it works, and both cost a crash to learn:

1. **Load `libnr_api.so` with `System.loadLibrary("nr_api")`, not `dlopen`.** It carries a
   `JNI_OnLoad`, which Android calls only for the loader, and that is where it receives the
   JavaVM. Reaching its functions without that aborts the process:
   `jni::InitializationException: JNI not initialized`. After the loader has it, `dlopen`
   in a shim returns the same handle and works.
2. **Ship the aars' Java classes, not just the `.so` files.** `NRAPICreate` calls back into
   Java and aborts with `jni::NameResolutionException: com/xreal/framework/net/binder/Connection`
   if they are absent. `nr_api.aar` and `nr_common.aar` together.

Then, all **[verified]** on an XREAL Air via a Beam Pro, each returning 0:

```c
long NRGetProcAddr(const char *name, void **out);        // 0 = ok, 1 = no such name
long NRAPICreate(jobject activity, void **out);          // Android object FIRST
long NRAPIStart(void *api);
long NRGlassesControlCreate(void **out);                 // NO api argument
long NRGlassesControlStart(void *glasses);
```

`NRAPICreate` taking a `jobject` first was found by passing an out pointer there and being
told `jobject is an invalid JNI transition frame reference: 0x7fe2d48660` — the stack
address that had just been handed over. `NRGlassesControlCreate` taking **one** argument
was found by the two-argument guess returning 0 while leaving the handle null: it was
writing a handle through the *api* pointer instead. Guess that one wrong and NRSDK's own
object is quietly corrupted, so test it in a fresh process.

Both create calls return real handles — `api = 0x73f90ee8b8`,
`glasses = 0xb400007bf05ba038` — and `NRGlassesControlStart` accepts the second.

### What a second app is actually allowed to do — settled

A battery of read-only getters, run against one handle, answers this outright. **[verified]**

With **Nebula running** — `NRGlassesControlStart` returns 0, and:

| Call | Result |
|---|---|
| `GetDisplayStereoMode` | **0, value 1** — the live display mode |
| `GetBrightness` | **0, value 7** |
| `GetBrightnessLevelNumber` | **0, value 8** |
| `GetTemperature` | **0, value 44** — degrees, real |
| `Get2D3DMode` | 22 |
| `Set2D3DMode` | 22 |
| `GetSupportedDisplayModeMask` | 22 |
| `GetDisplayDefaultStartMode` / `Set…` | 22 |
| `GetVersion`, `GetHWVersion`, `GetScreenStatus`, `GetActivatedState` | 22 |
| `GetEnablePhysicalButtonSwitchDisplayModeFlag` | 22 |
| `NRHeadTrackingCreate` — every handle shape, including via `NRPerceptionCreate` | 2 |
| `NRImuCreate` | 22 |

With **Nebula stopped** — `NRGlassesControlStart` returns **101**, and *everything* fails,
brightness and temperature included (2 or 24).

Two error codes are now readable. **2 = bad handle**: `GetBrightness` returns it for a null
or wrong handle and 0 for the right one. **22 = refused for this caller**, since it lands
on functions of exactly the same shape as ones that succeed — `Get2D3DMode(handle, int*)`
fails while `GetBrightness(handle, int*)` works. It is not a signature problem.

**The conclusion.** This process's NRSDK is a **client of a service Nebula hosts**. Without
Nebula there is nothing to talk to at all. With Nebula there is read-only access to state
it already holds, and every command and every device round-trip is refused. The same
exclusivity as the raw USB interface, arrived at politely.

For a second app on this device:

- **2D/3D switching: no.** `Set2D3DMode` is refused from a handle that demonstrably works
  for reads. The raw MCU route in §6 stays the only one that switches, and it evicts Nebula.
- **Head tracking and IMU: no.** They never create, in any handle shape.
- **Usable: brightness, brightness level count, temperature, current display stereo mode** —
  observation only, and only while Nebula runs.

None of that earns 48MB of aars. The one genuinely useful reading, the display mode, is
already free from `DisplayManager` via the display's shape.

This is not the SDK being obsolete — XREAL's own player switches 2D/3D on this very Nebula
build. It is the SDK working as intended for the process that owns the glasses, and only
for that one.

### The API XREAL's own apps actually use

Decompiling Nebula settles where the public SDK sits. **[verified]**

There is **no separate player app** on the device — no package, no APK. The video player is
inside `com.xreal.evapro.nebula` itself, which is why it can switch modes: it *is* the
process that owns the glasses. There is no third-party example to copy.

Nebula's APK carries libraries the public SDK does not:

```
lib/arm64-v8a/libnr_service.so        42MB   <- not in the SDK
lib/arm64-v8a/libnr_glasses_api.so     2MB   <- not in the SDK
lib/arm64-v8a/libnr_external_sensor.so 6MB   <- not in the SDK
lib/arm64-v8a/libnr_api.so            41MB   <- this one the SDK does ship
```

And a Java class the SDK does not: `com.xreal.glasses.servicesdk.NRServiceControl`, which
does `System.loadLibrary("nr_service")` and exposes exactly what we could not reach —

```java
native boolean nativeSet2D3DMode(int);
native boolean nativeSetDpStereoMode(int);
native boolean nativeSetDpInputModeEx(int, int);
native boolean nativeSetDisplayDefaultStartMode(int);
native boolean nativeSetEnablePhysicalButtonSwitchDisplayModeFlag(int);
int  get2D3D();     boolean set2D3D(int);     boolean enableLongPress(boolean);
```

So there are **two halves**, and only one of them ships publicly:

| | library | who has it | what it can do |
|---|---|---|---|
| **client** | `libnr_api.so` | the public Unity SDK | connect to a running service; read state |
| **service** | `libnr_service.so` | Nebula only | own the glasses; command them |

That is the whole explanation for error 22. Our app links the client half and connects to
the service Nebula hosts, which grants reads and refuses commands. Stop Nebula and there is
no service to connect to, which is why everything failed then.

`set2D3D` is additionally gated on `isStartSdk()` — the service must have been started
in-process (`nativeCreateService`, `nativeStartService`, `nativeGlassesInit`). Extracting
`libnr_service.so` from Nebula and starting a second one would put two services on one USB
device: the same collision as the raw MCU route in §6, with no advantage over it, and it
would mean redistributing XREAL's proprietary library.

Two incidental findings worth having:

- **Nebula's player is media3/ExoPlayer**, the same stack as this project, and the APK even
  bundles `SphericalGLSurfaceView`. Nothing exotic is going on in their player.
- `com.xreal.glassesdisplayplugevent.display.DisplayObserverHelper` declares
  `MR_DISPLAY_WIDTH = 3840`, `MR_DISPLAY_HEIGHT = 1080`. **XREAL detect 3D mode from the
  display's dimensions**, exactly as this project's `GlassesMode` does. That approach is
  not a workaround; it is what the vendor does.

---

## 10. Reference implementations

- **`gitlab.com/TheJackiMonster/nrealAirLinuxDriver`** — the protocol source of truth.
  `interface_lib/src/hid_ids.c` (ids and interface numbers), `device_imu.c` (IMU),
  `device_mcu.c` (MCU), `include/device_mcu.h` (message ids and display modes). MIT.
- **`github.com/wheaney/XRLinuxDriver`** — a service built on it; virtual display and
  head-tracking modes, Steam Deck packaging.
- **`github.com/wheaney/decky-XRGaming`** — the Decky plugin, for how this is packaged for
  end users.

Both are C with hidapi and port cleanly.

---

## 11. Open questions

1. ~~IMU field order~~ — **RESOLVED.** Multiplier and divisor precede their values. §5.
2. ~~Accel and magnetometer offsets~~ — **RESOLVED.** Full map in §5, all groups decoded
   from live data and sanity-checked against gravity and Earth's magnetic field.
3. **Interface 5** — still unidentified. Interface 6 is **RESOLVED**: usage page `0x000c`,
   usage `0x0001` — a standard HID Consumer Control endpoint, i.e. the buttons.
4. **Heartbeat** — is it needed to hold a mode, or to stop the glasses sleeping?
5. **The physical-button flag** — no raw MCU message id known; only the NRSDK name.
6. ~~macOS HID seize and Input Monitoring~~ — **RESOLVED.** Neither is required. §4.
7. **Air 2 Ultra** uses different interface numbers (IMU 2, MCU 0) and a 512-byte IMU
   payload. Nothing here was tested on it.
8. **Does the IMU length rule generalise?** `START_IMU_DATA` is verified at length 4
   (§5). The other IMU msgids (`0x14`–`0x1A`) were only ever sent at the old, wrong
   length, so they have never actually been exercised — every reply captured for them was
   the canned reject. Re-test the calibration messages with the corrected length before
   assuming they are unavailable.

---

## 12. Quick start for a Mac/Linux tool

The whole 2D/3D switch, in pseudocode:

```
dev = hid_open_path(interface 4 of vid 0x3318)
data      = [0x03]                      # or 0x01 for 2D
body      = le16(18) + le64(now_ms) + le16(0x0008) + [0,0,0,0,0] + data
packet    = [0xFD] + le32(crc32(body)) + body      # 23 bytes
hid_write(dev, packet)
# then read; the reply echoes msgid 0x0008 at offset 15
```

And the IMU:

```
dev    = hid_open_path(interface 3 of vid 0x3318)
body   = [0x04, 0x00, 0x19, 0x01]          # 0x04, not 0x03 — length counts itself
hid_write(dev, [0xAA] + le32(crc32(body)) + body)   # 9 bytes
loop: report = hid_read(dev, 64)
      if report[0:2] != [0x01, 0x02]: continue      # ack or reject, not data
      gyro_mul = i16le(report[12:14]); gyro_div = i32le(report[14:18])
      gx = i24le(report[18:21]) * gyro_mul / gyro_div      # deg/s
      gy = i24le(report[21:24]) * gyro_mul / gyro_div
      gz = i24le(report[24:27]) * gyro_mul / gyro_div
      acc_mul = i16le(report[27:29]); acc_div = i32le(report[29:33])
      ax = i24le(report[33:36]) * acc_mul / acc_div        # g
      ay = i24le(report[36:39]) * acc_mul / acc_div
      az = i24le(report[39:42]) * acc_mul / acc_div
      mag_mul = i16be(report[42:44]); mag_div = i32be(report[44:48])   # big-endian!
      mx = i16le(report[48:50] ^ 0x8000) * mag_mul / mag_div           # gauss
      my = i16le(report[50:52] ^ 0x8000) * mag_mul / mag_div
      mz = i16le(report[52:54] ^ 0x8000) * mag_mul / mag_div
```

Both are verified working, end to end, on macOS with nothing but IOKit — no hidapi, no
driver, no entitlement, no permission prompt. Sanity check before trusting a session:
`|accel|` must read ≈ 1.0 and `|mag|` ≈ 0.3 with the glasses still. The rest is a filter
and a UI.

A working macOS implementation of all of the above lives in `Tools/hidprobe.swift`:
`list`, `imu [sec]`, `sweep`, `getmode`, `setmode 2d|3d`.
