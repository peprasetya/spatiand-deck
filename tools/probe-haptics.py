#!/usr/bin/env python3
"""Find the feature report that makes the Deck's trackpads buzz.

The pads feel dead inside Spatiand and alive on the desktop. That is not a bug in reading
them -- the click bit arrives fine -- it is that haptics are *driven*: something has to send
a command for every buzz, and on the desktop that something is Steam. Nothing in Spatiand
sends one yet.

`hid-steam.c` carries two different mechanisms and the Deck does not use the same one as the
original Steam Controller, so rather than guess, this fires each candidate in turn with a gap
between them and asks which ones were felt.

Numbered output; the numbers are what matter. Run it, feel the pads, report the numbers.
"""

import fcntl
import os
import sys
import time

VID, PID = 0x28DE, 0x1205


def ioc(direction, letter, nr, size):
    return (direction << 30) | (size << 16) | (ord(letter) << 8) | nr


def send_feature(fd, payload):
    buf = bytearray(65)
    buf[1 : 1 + len(payload)] = payload
    fcntl.ioctl(fd, ioc(3, "H", 0x06, len(buf)), buf)


def find_vendor_node():
    for name in sorted(os.listdir("/sys/class/hidraw")):
        try:
            text = open(f"/sys/class/hidraw/{name}/device/uevent").read()
        except OSError:
            continue
        if f"{VID:08X}:{PID:08X}".lower() not in text.lower():
            continue
        for line in text.splitlines():
            if line.startswith("HID_PHYS=") and line.rstrip().endswith("/input2"):
                return f"/dev/{name}"
    return None


def le16(v):
    return [v & 0xFF, (v >> 8) & 0xFF]


def candidates():
    """Each entry is (label, payload). Order is the order they fire in."""
    out = []

    # --- 0x8f: ID_TRIGGER_HAPTIC_PULSE, the Steam Controller mechanism ---
    # [cmd, len, pad, duration, interval, count, gain] -- pad 0 is right, 1 is left on the
    # original controller. Long duration and high repeat count so it is unmistakable.
    for pad, side in ((0, "pad byte 0"), (1, "pad byte 1")):
        out.append(
            (
                f"HAPTIC_PULSE 0x8f, {side}",
                bytes([0x8F, 8, pad] + le16(6000) + le16(6000) + le16(80) + [0]),
            )
        )

    # --- 0xEA: ID_TRIGGER_HAPTIC_COMMAND, the newer per-pad interface ---
    # [cmd, len, side, intensity, gain, priority(2)] -- shapes vary between kernel versions,
    # so send a plausible fixed-length payload and see.
    for side, name in ((0, "side 0"), (1, "side 1"), (2, "side 2")):
        out.append(
            (
                f"HAPTIC_COMMAND 0xea, {name}",
                bytes([0xEA, 0x0D, side, 0x00] + le16(0x0800) + le16(0x0400) + [0x00] * 7),
            )
        )

    # --- 0xEB: ID_TRIGGER_RUMBLE_CMD, what the Deck uses for force feedback ---
    # [cmd, len, unknown, left_speed, right_speed, left_gain, right_gain]
    out.append(
        (
            "RUMBLE 0xeb, both motors",
            bytes([0xEB, 9, 0x00] + le16(0xFFFF) + le16(0xFFFF) + [0x02, 0x02]),
        )
    )
    out.append(
        (
            "RUMBLE 0xeb, left only",
            bytes([0xEB, 9, 0x00] + le16(0xFFFF) + le16(0x0000) + [0x02, 0x00]),
        )
    )
    out.append(
        (
            "RUMBLE 0xeb, right only",
            bytes([0xEB, 9, 0x00] + le16(0x0000) + le16(0xFFFF) + [0x00, 0x02]),
        )
    )
    return out


def main():
    if os.system("pgrep -x steam >/dev/null") == 0:
        print("Steam is running and owns the controller; its haptics would mask these.")
        print("Stop Steam and run again.")
        return 1

    node = find_vendor_node()
    if not node:
        print("Deck controller not found.")
        return 1
    print(f"controller: {node}\n")

    fd = os.open(node, os.O_RDWR)
    try:
        # Same takeover Spatiand performs, so this tests haptics under the conditions that
        # actually apply -- lizard mode off is exactly when the pads went quiet.
        payload = bytearray([0x87, 12])
        for reg, value in ((0x07, 0x07), (0x08, 0x07), (0x18, 0x00), (0x30, 0x18)):
            payload += bytes([reg, value & 0xFF, (value >> 8) & 0xFF])
        send_feature(fd, payload)
        send_feature(fd, bytes([0x81, 0x00]))
        print("lizard mode off (as Spatiand leaves it)\n")
        time.sleep(0.5)

        tests = candidates()
        print(f"Firing {len(tests)} candidates, 3 seconds apart. Hold both pads.\n")
        for i, (label, payload) in enumerate(tests, 1):
            print(f"  {i}. {label}")
            try:
                # Three bursts, so a single dropped report does not read as a dead command.
                for _ in range(3):
                    send_feature(fd, payload)
                    time.sleep(0.12)
            except OSError as e:
                print(f"     rejected by the device: {e}")
            time.sleep(3.0)

        print("\nDone. Which numbers did you feel, and in which pad?")
    finally:
        try:
            send_feature(fd, bytes([0x8E, 0x00]))  # ID_LOAD_DEFAULT_SETTINGS
        except OSError:
            pass
        os.close(fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
