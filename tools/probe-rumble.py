#!/usr/bin/env python3
"""Test the Deck's force feedback through the kernel's own interface.

The raw-HID haptic probe fires eight guesses at the device and hopes one of them is the right
feature report. This does not guess: `hid-steam` registers the Steam Deck as a force-feedback
device (`/proc/bus/input/devices` shows `B: FF=` non-zero on it), which means the kernel
already knows the right command and exposes the standard `EVIOCSFF` interface for it.

That is worth preferring for more than reliability. It is the same API on any gamepad, so
feedback keeps working on hardware that is not a Steam Deck — which the raw-HID path could
never do.

The catch is that rumble drives the body motors, not the actuators under the trackpads. If a
click needs to feel like a click *in the pad*, this is not it, and the HID route is the only
way. Firing both is how to tell.
"""

import array
import fcntl
import os
import struct
import sys
import time

# linux/input.h
EVIOCSFF = 0x40304580  # _IOC(_IOC_WRITE, 'E', 0x80, sizeof(struct ff_effect)) == 48 bytes
EV_FF = 0x15
FF_RUMBLE = 0x50


def find_device():
    """The event node of the first force-feedback capable device named like a Deck."""
    name = None
    handlers = None
    with open("/proc/bus/input/devices") as f:
        for line in f:
            line = line.strip()
            if line.startswith("N: Name="):
                name = line.split("=", 1)[1].strip('"')
            elif line.startswith("H: Handlers="):
                handlers = line.split("=", 1)[1].split()
            elif line.startswith("B: FF=") and handlers:
                bits = line.split("=", 1)[1].split()
                if any(b.strip("0") for b in bits):
                    for h in handlers:
                        if h.startswith("event"):
                            return f"/dev/input/{h}", name
    return None, None


def upload(fd, strong, weak, duration_ms):
    """Upload a rumble effect and return its id.

    struct ff_effect is 48 bytes: type, id, direction, trigger(4), replay(4), then a union.
    The rumble union is two u16 magnitudes at the start of that union, which begins at
    offset 16.
    """
    effect = bytearray(48)
    struct.pack_into("<HhH", effect, 0, FF_RUMBLE, -1, 0)  # type, id=-1 (new), direction
    struct.pack_into("<HH", effect, 8, 0, 0)               # trigger: button, interval
    struct.pack_into("<HH", effect, 12, duration_ms, 0)    # replay: length, delay
    struct.pack_into("<HH", effect, 16, strong, weak)      # rumble: strong, weak
    buf = array.array("B", effect)
    fcntl.ioctl(fd, EVIOCSFF, buf, True)
    return struct.unpack_from("<h", buf, 2)[0]


def play(fd, effect_id):
    # An EV_FF event with the effect id as code and value 1 starts it.
    now = time.time()
    event = struct.pack(
        "<qqHHi", int(now), int((now % 1) * 1e6), EV_FF, effect_id & 0xFFFF, 1
    )
    os.write(fd, event)


def main():
    path, name = find_device()
    if not path:
        print("No force-feedback device found.")
        return 1
    print(f"device: {path}  ({name})\n")

    try:
        fd = os.open(path, os.O_RDWR)
    except PermissionError:
        print(f"No permission for {path}. This needs the seat ACL or a udev rule.")
        return 1

    try:
        tests = [
            ("strong motor, 300 ms", 0xFFFF, 0x0000, 300),
            ("weak motor, 300 ms", 0x0000, 0xFFFF, 300),
            ("both motors, 600 ms", 0xFFFF, 0xFFFF, 600),
            ("short tap, both, 60 ms", 0xC000, 0xC000, 60),
        ]
        print(f"Firing {len(tests)} effects, 3 seconds apart. Hold the deck.\n")
        for i, (label, strong, weak, ms) in enumerate(tests, 1):
            print(f"  {i}. {label}")
            try:
                effect_id = upload(fd, strong, weak, ms)
                play(fd, effect_id)
            except OSError as e:
                print(f"     rejected: {e}")
            time.sleep(3.0)
        print("\nDone. Which numbers did you feel?")
    finally:
        os.close(fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
