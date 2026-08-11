# Steam Deck controller — low-level notes

What Spatiand needs from the Deck's own controls, and what was established on hardware.

Tagged like `xreal-air.md`:

- **[verified]** — observed on this Steam Deck (SteamOS 3.8.x, kernel 6.16.12-valve24.5,
  August 2026).
- **[kernel]** — read out of `drivers/hid/hid-steam.c` / SDL's `SDL_hidapi_steamdeck.c`.
- **[inferred]** — reasoning from the above.

---

## 1. Why hidraw and not evdev

The kernel's `hid-steam` driver exposes the Deck as an evdev gamepad with every button, and
the touchpads "presented as a hat and a button each". That is not enough. It **disables the
touchpads' pointer input**, and evdev carries no absolute pad coordinates, no pad pressure
and no gyro. A 3D pointer needs all three.

The raw 64-byte vendor reports carry everything. So `spatiand-input` opens hidraw directly.
**[kernel]**

---

## 2. Device identity

| | |
|---|---|
| VID:PID | `28DE:1205` |
| hidraw nodes | three, all `hid-steam` |
| **vendor interface** | the one whose `HID_PHYS` ends **`/input2`** (HID group `0103`) — `/dev/hidraw3` on this machine |

Match on the `/input2` suffix, not the node number: `hidraw3` is not stable across boots or
across which USB devices enumerate first. **[verified]**

Access is granted to the desktop user by ACL — `crw-rw----+ root root` with
`user:deck:rw-`. **No udev rule and no root are required.** **[verified]**

---

## 3. Steam owns the device while it runs

`fuser /dev/hidraw3` shows `steam` holding it. Reading concurrently is possible, but
worthless: with Steam running, **every payload field reads zero** — buttons, pads, accel and
gyro alike — because Steam has configured the controller for its own use and turned gyro
reporting off. Only the sequence counter moves. **[verified]**

Stopping Steam and reconfiguring the device ourselves produces full reports immediately.

**Consequence for the design:** spatial mode must own the controller exclusively, and must
not run concurrently with Game Mode. This is the origin of the input-arbitration design —
it is forced by the hardware, not a preference.

Restarting Steam from a plain SSH shell fails silently; it needs the session environment.
Lift it from a running session process:

```bash
eval "$(tr '\0' '\n' < /proc/$(pgrep -x plasmashell | head -1)/environ \
        | grep -E '^(XDG_RUNTIME_DIR|WAYLAND_DISPLAY|DISPLAY|DBUS_SESSION_BUS_ADDRESS)=' \
        | sed 's/^/export /;s/=/="/;s/$/"/')"
setsid steam -silent >/dev/null 2>&1 </dev/null &
```

**[verified]**

---

## 4. Taking the device

Feature reports, via `HIDIOCSFEATURE`, with a leading `0x00` report-id byte. Commands and
registers from `hid-steam.c`. **[kernel]**

```
ID_CLEAR_DIGITAL_MAPPINGS = 0x81   ID_SET_SETTINGS_VALUES = 0x87
ID_LOAD_DEFAULT_SETTINGS  = 0x8E

REG_LPAD_MODE           = 0x07     REG_RPAD_MODE           = 0x08
REG_RPAD_MARGIN         = 0x18     REG_GYRO_MODE           = 0x30
REG_LPAD_CLICK_PRESSURE = 0x34     REG_RPAD_CLICK_PRESSURE = 0x35
```

Disable lizard mode (its keyboard/mouse emulation) by writing `LPAD_MODE = RPAD_MODE = 0x07`,
zeroing `RPAD_MARGIN`, setting both click pressures to `0xFFFF`, then sending
`ID_CLEAR_DIGITAL_MAPPINGS`. Enable the IMU with `REG_GYRO_MODE = 0x18` (accel **and** gyro).
`ID_LOAD_DEFAULT_SETTINGS` hands the device back. **[verified — all of it took effect]**

`SET_SETTINGS_VALUES` payload is `[0x87, byte_count, reg, lo, hi, reg, lo, hi, …]`.

---

## 5. Input report layout

64 bytes, **~250–270 Hz**. Header `01 00 09 40`. **[verified]**

| Offset | Size | Field | Status |
|---|---|---|---|
| 0 | 2 | version, `0x0001` | **[verified]** |
| 2 | 1 | type, `0x09` | **[verified]** |
| 3 | 1 | length, `0x40` | **[verified]** |
| 4 | 4 | sequence, u32 | **[verified]** — the only field that moves when Steam owns the device |
| 8 | 8 | buttons, bitfield | **[kernel]** — not yet exercised |
| 16 | 4 | left pad X, Y — 2 × i16 | **[kernel]** — read `(0,0)` untouched, still to confirm |
| 20 | 4 | right pad X, Y — 2 × i16 | **[kernel]** — as above |
| 24 | 6 | accelerometer X, Y, Z — 3 × i16 | **[verified]** |
| 30 | 6 | gyroscope X, Y, Z — 3 × i16 | **[verified]** |
| 44 | 4 | triggers L, R — 2 × u16 | **[kernel]** |

**Accelerometer scale is `0x4000` = 16384 counts per g.** Resting on a desk the raw vector
read `(-716, 298, 16664)`, magnitude `16682` = **1.018 g**. That is what confirms the layout:
gravity is a known quantity, so an offset error would destroy it. **[verified]**

Byte offsets seen changing on an untouched, stationary controller were `4–5` (sequence),
`24–35` (accel + gyro noise), `48`, `50`, `52`, `54–55` and `60–63`. The last two groups are
unidentified — plausibly a fused orientation quaternion and battery/pressure — and are not
needed yet. **[inferred]**

---

## 6. Still open

1. **Pad coordinates and pressure** — offsets 16–23 are the kernel's layout but were never
   exercised, because confirming them requires someone to touch the pads. The 3D pointer
   depends on this, so it is the next thing to nail down.
2. **Button bitfield** — same reason.
3. Offsets 48–55 and 60–63.
4. **Haptics.** The pads can buzz, which the pointer wants for edge and click feedback.
