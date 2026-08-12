#!/usr/bin/env python3
"""Discover the Steam Deck's button bit map by asking someone to press each one.

`docs/steam-deck-controller.md` §6 lists the button bitfield and the pad offsets as the two
things read out of `hid-steam.c` but never exercised here. Everything Spatiand's shell does —
D-pad navigation, A to select, B to go back, STEAM for the HUD, the kebab for the launcher —
depends on them, so guessing is not good enough.

This walks through the controls one at a time, watches which bit goes high, and prints a table
ready to paste into `crates/spatiand-input/src/layout.rs`.

Two things have to be true first, and the script checks both:

  * **Steam must not be running.** It configures the controller for itself and turns gyro
    reporting off, after which every payload field reads zero while the sequence counter keeps
    advancing. That looks exactly like a wrong offset.
  * **Lizard mode must be off**, or the pads act as a mouse and report nothing useful.

Usage:  sudo systemctl --user stop app-steam@autostart.service   # or just kill it
        python3 probe-controller.py
"""

import fcntl
import os
import struct
import sys
import time

VID, PID = 0x28DE, 0x1205
HEADER = bytes([0x01, 0x00, 0x09, 0x40])

# --- feature report plumbing (docs/steam-deck-controller.md §4) ---
ID_CLEAR_DIGITAL_MAPPINGS = 0x81
ID_SET_SETTINGS_VALUES = 0x87
ID_LOAD_DEFAULT_SETTINGS = 0x8E

REG_LPAD_MODE = 0x07
REG_RPAD_MODE = 0x08
REG_RPAD_MARGIN = 0x18
REG_GYRO_MODE = 0x30

# The controls to walk through, in an order that keeps your thumbs in one place for a while
# rather than jumping across the device between prompts.
SEQUENCE = [
    ("A", "A"),
    ("B", "B"),
    ("X", "X"),
    ("Y", "Y"),
    ("Up", "D-pad UP"),
    ("Down", "D-pad DOWN"),
    ("Left", "D-pad LEFT"),
    ("Right", "D-pad RIGHT"),
    ("L1", "L1 (left bumper)"),
    ("R1", "R1 (right bumper)"),
    ("L2", "L2 (left trigger, pull FULLY)"),
    ("R2", "R2 (right trigger, pull FULLY)"),
    ("View", "VIEW button (the two-squares one, left of the left stick)"),
    ("Menu", "MENU button (the three-lines one, right of the right stick)"),
    ("Steam", "STEAM button"),
    ("Quick", "the ... button (quick access, below STEAM)"),
    ("L4", "L4 (upper-left back paddle)"),
    ("R4", "R4 (upper-right back paddle)"),
    ("L5", "L5 (lower-left back paddle)"),
    ("R5", "R5 (lower-right back paddle)"),
    ("LPadTouch", "REST a finger on the LEFT touchpad (do not click)"),
    ("RPadTouch", "REST a finger on the RIGHT touchpad (do not click)"),
    ("LPadClick", "CLICK the LEFT touchpad"),
    ("RPadClick", "CLICK the RIGHT touchpad"),
    ("LStickClick", "press the LEFT stick IN"),
    ("RStickClick", "press the RIGHT stick IN"),
]

PER_CONTROL_TIMEOUT = 12.0


def ioc(direction, letter, nr, size):
    return (direction << 30) | (size << 16) | (ord(letter) << 8) | nr


def send_feature(fd, payload):
    """HIDIOCSFEATURE with a leading report-id byte, padded to the 64-byte report size."""
    buf = bytearray(65)
    buf[0] = 0x00
    buf[1 : 1 + len(payload)] = payload
    fcntl.ioctl(fd, ioc(3, "H", 0x06, len(buf)), buf)


def set_registers(fd, pairs):
    payload = bytearray([ID_SET_SETTINGS_VALUES, len(pairs) * 3])
    for reg, value in pairs:
        payload += bytes([reg, value & 0xFF, (value >> 8) & 0xFF])
    send_feature(fd, payload)


def find_vendor_node():
    """The vendor interface is the one whose HID_PHYS ends /input2 — node numbers move."""
    for name in sorted(os.listdir("/sys/class/hidraw")):
        try:
            with open(f"/sys/class/hidraw/{name}/device/uevent") as f:
                text = f.read()
        except OSError:
            continue
        if f"{VID:08X}:{PID:08X}".lower() not in text.lower():
            continue
        for line in text.splitlines():
            if line.startswith("HID_PHYS=") and line.rstrip().endswith("/input2"):
                return f"/dev/{name}"
    return None


def buttons_of(report):
    return int.from_bytes(report[8:16], "little")


def bit_names(mask):
    return [i for i in range(64) if mask & (1 << i)]


def record(fd, seconds, resting):
    """Log every button-bit transition with a timestamp.

    Preferred over the guided mode whenever the person pressing the buttons cannot see this
    script's output — over SSH, for instance. They work through a printed list at their own
    pace and the order of first appearance is what identifies each bit.
    """
    print(f"recording for {seconds:.0f}s — press each control ONCE, in the listed order,")
    print("leaving about two seconds between them\n")
    os.set_blocking(fd, False)
    previous = resting
    started = time.time()
    order = []
    while time.time() - started < seconds:
        try:
            data = os.read(fd, 64)
        except BlockingIOError:
            time.sleep(0.002)
            continue
        if len(data) < 64 or data[:4] != HEADER:
            continue
        now = buttons_of(data) & ~resting
        if now == previous:
            continue
        for bit in bit_names(now & ~previous):
            t = time.time() - started
            first = "" if bit in order else "   <- first time"
            if bit not in order:
                order.append(bit)
            print(f"  t={t:6.2f}  DOWN  bit {bit:2d}  (byte {8 + bit // 8}, bit {bit % 8}){first}")
        for bit in bit_names(previous & ~now):
            t = time.time() - started
            print(f"  t={t:6.2f}  up    bit {bit:2d}")
        previous = now
    print(f"\norder of first appearance: {order}")
    return order


def main():
    if os.system("pgrep -x steam >/dev/null") == 0:
        print("Steam is running. It owns the controller and zeroes every payload field,")
        print("so this probe would read nothing. Stop Steam and run again.")
        return 1

    node = find_vendor_node()
    if not node:
        print("Could not find the Deck's vendor interface (28DE:1205 .../input2).")
        return 1
    print(f"controller: {node}")

    fd = os.open(node, os.O_RDWR)
    try:
        # Take the device: pads as absolute, no click-pressure threshold, no mouse emulation,
        # IMU on. Without this the pads behave as a mouse and report nothing usable.
        set_registers(
            fd,
            [
                (REG_LPAD_MODE, 0x07),
                (REG_RPAD_MODE, 0x07),
                (REG_RPAD_MARGIN, 0x00),
                (REG_GYRO_MODE, 0x18),
            ],
        )
        send_feature(fd, bytes([ID_CLEAR_DIGITAL_MAPPINGS, 0x00]))
        print("lizard mode off, gyro on\n")

        os.set_blocking(fd, False)

        # Settle, and learn the resting state so a stuck bit is not mistaken for a press.
        deadline = time.time() + 1.0
        resting = 0
        seen_any = False
        while time.time() < deadline:
            try:
                data = os.read(fd, 64)
            except BlockingIOError:
                time.sleep(0.002)
                continue
            if len(data) >= 64 and data[:4] == HEADER:
                resting = buttons_of(data)
                seen_any = True
        if not seen_any:
            print("No input reports arrived. Is something else holding the device?")
            return 1
        if resting:
            print(f"note: bits {bit_names(resting)} are high at rest and will be ignored\n")

        if "--record" in sys.argv:
            seconds = 90.0
            for i, a in enumerate(sys.argv):
                if a == "--record" and i + 1 < len(sys.argv):
                    try:
                        seconds = float(sys.argv[i + 1])
                    except ValueError:
                        pass
            record(fd, seconds, resting)
            return 0

        wanted = SEQUENCE
        for i, a in enumerate(sys.argv):
            if a == "--only" and i + 1 < len(sys.argv):
                keep = {k.strip() for k in sys.argv[i + 1].split(",")}
                wanted = [(k, p) for k, p in SEQUENCE if k in keep]

        found = {}
        skipped = []
        print("=" * 62)
        print("  Spatiand controller probe")
        print("  Press the control named below. SPACE-bar-free: it reads the pad")
        print("  directly. Nothing you press here reaches Steam or the desktop.")
        print("=" * 62)
        print()

        for index, (key, prompt) in enumerate(wanted, 1):
            print(f"  [{index:2d}/{len(wanted)}]  PRESS:  {prompt}")
            print("           ", end="", flush=True)
            got = None
            deadline = time.time() + PER_CONTROL_TIMEOUT
            last_tick = 0
            while time.time() < deadline:
                remaining = deadline - time.time()
                tick = int(remaining)
                if tick != last_tick:
                    # A countdown, so a control that is never going to fire looks like a
                    # timeout rather than like the tool having hung.
                    print(f"\r           waiting… {tick:2d}s ", end="", flush=True)
                    last_tick = tick
                try:
                    data = os.read(fd, 64)
                except BlockingIOError:
                    time.sleep(0.002)
                    continue
                if len(data) < 64 or data[:4] != HEADER:
                    continue
                new = buttons_of(data) & ~resting
                # Ignore bits already claimed, so holding a paddle down while reaching for the
                # next control does not shadow it.
                for taken in found.values():
                    new &= ~(1 << taken)
                if new:
                    bits = bit_names(new)
                    got = bits[0]
                    if len(bits) > 1:
                        print(f"\r           (several at once: {bits}) ", end="")
                    break
            if got is None:
                skipped.append(key)
                print("\r           -> timed out, skipped" + " " * 20)
                continue
            found[key] = got
            print(f"\r           -> OK   bit {got}  (byte {8 + got // 8}, bit {got % 8})" + " " * 10)
            # Wait for release so the next prompt starts clean.
            release_by = time.time() + 3.0
            while time.time() < release_by:
                try:
                    data = os.read(fd, 64)
                except BlockingIOError:
                    time.sleep(0.002)
                    continue
                if len(data) >= 64 and data[:4] == HEADER and not (buttons_of(data) & (1 << got)):
                    break

        # Pads, while we are here: §6 lists offsets 16-23 as unexercised too.
        print()
        print("  Last one: SWEEP a finger all around the RIGHT touchpad for 4 seconds.")
        print("           ", end="", flush=True)
        lo = [32767, 32767]
        hi = [-32768, -32768]
        deadline = time.time() + 4.0
        while time.time() < deadline:
            try:
                data = os.read(fd, 64)
            except BlockingIOError:
                time.sleep(0.002)
                continue
            if len(data) < 64 or data[:4] != HEADER:
                continue
            x, y = struct.unpack_from("<hh", data, 20)
            lo[0], hi[0] = min(lo[0], x), max(hi[0], x)
            lo[1], hi[1] = min(lo[1], y), max(hi[1], y)
        pad_line = f"right pad at offsets 20/22: x {lo[0]}..{hi[0]}  y {lo[1]}..{hi[1]}"
        if hi[0] - lo[0] < 1000 and hi[1] - lo[1] < 1000:
            pad_line += "  -> barely moved, NOT the right pad"
        else:
            pad_line += "  -> confirmed"
        print(f"\r           {pad_line}")

        report = ["--- paste into crates/spatiand-input/src/layout.rs ---"]
        for key, _ in wanted:
            if key in found:
                report.append(f"    (Control::{key}, {found[key]}, Confidence::Verified),")
            else:
                report.append(f"    // Control::{key} — not observed")
        report.append(pad_line)
        text = "\n".join(report)

        out = os.path.expanduser("~/spatiand-controller-map.txt")
        with open(out, "w") as f:
            f.write(text + "\n")

        print()
        print(text)
        print()
        if skipped:
            print(f"  not captured: {', '.join(skipped)}")
        print(f"  saved to {out}")
        print()
        input("  done — press Enter to close this window ")
    finally:
        # Hand the controller back, or it stays in our configuration after we exit and Steam
        # finds it behaving oddly.
        try:
            send_feature(fd, bytes([ID_LOAD_DEFAULT_SETTINGS, 0x00]))
        except OSError:
            pass
        os.close(fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
