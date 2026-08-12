#!/usr/bin/env python3
"""Measure which XREAL sensor axis is pitch, which is roll, and with what sign.

Written because guessing failed. Two candidate axis maps have been in use on this hardware
and both were reported wrong; there are four that are valid rotations, and cycling through
them by feel did not settle it either. This measures it directly.

The method deliberately does **not** reuse the in-app calibration. That flow integrates gyro
rate over a timed window and takes the dominant axis, which is exactly the measurement under
suspicion. This uses **gravity** instead — an absolute reference the gyro cannot provide:

  * Hold the glasses level: the accelerometer points along the sensor axis that is head-UP.
  * Nod them forward (as if looking down): gravity swings toward the axis that is
    head-FORWARD.
  * Tilt them sideways (left ear down): gravity swings toward the axis that is head-LEFT.

Those three are enough to name every axis with no integration and no timing. The gyro is
recorded alongside so its axes can be cross-checked against the same motion.

Run it with the glasses plugged in and OFF your head, resting on the desk, so the poses are
unambiguous. Nothing here changes any stored configuration.
"""

import os
import select
import struct
import sys
import time
from math import sqrt

VID, PID = 0x3318, 0x0424
IMU_INTERFACE = 3
AXES = "XYZ"


def crc(b):
    c = 0xFFFFFFFF
    for x in b:
        c ^= x
        for _ in range(8):
            c = (0xEDB88320 ^ (c >> 1)) if c & 1 else c >> 1
    return c ^ 0xFFFFFFFF


def i24(p, o):
    v = p[o] | (p[o + 1] << 8) | (p[o + 2] << 16)
    return v - 0x1000000 if v & 0x800000 else v


def find_imu():
    for name in sorted(os.listdir("/sys/class/hidraw")):
        try:
            text = open(f"/sys/class/hidraw/{name}/device/uevent").read()
        except OSError:
            continue
        if f"{VID:08X}:{PID:08X}".lower() not in text.lower():
            continue
        for line in text.splitlines():
            if line.startswith("HID_PHYS=") and line.rstrip().endswith(f"/input{IMU_INTERFACE}"):
                return f"/dev/{name}"
    return None


def write_report(fd, payload):
    os.write(fd, b"\x00" + payload)


def read_sample(fd, timeout=0.5):
    r, _, _ = select.select([fd], [], [], timeout)
    if not r:
        return None
    p = os.read(fd, 64)
    if len(p) < 54 or p[0] != 0x01 or p[1] != 0x02:
        return None
    gm, gd = struct.unpack_from("<h", p, 12)[0], struct.unpack_from("<i", p, 14)[0]
    am, ad = struct.unpack_from("<h", p, 27)[0], struct.unpack_from("<i", p, 29)[0]
    if not (gd and ad):
        return None
    g = [i24(p, 18 + 3 * i) * gm / gd for i in range(3)]
    a = [i24(p, 33 + 3 * i) * am / ad for i in range(3)]
    if all(v == 0 for v in a):
        return None
    return g, a


def average(fd, seconds, label):
    """Mean accel and mean gyro over a window, with a visible countdown."""
    gsum = [0.0, 0.0, 0.0]
    asum = [0.0, 0.0, 0.0]
    n = 0
    end = time.time() + seconds
    last = None
    while time.time() < end:
        left = int(end - time.time()) + 1
        if left != last:
            print(f"\r    {label}: {left:2d}s ", end="", flush=True)
            last = left
        s = read_sample(fd)
        if not s:
            continue
        g, a = s
        gsum = [x + y for x, y in zip(gsum, g)]
        asum = [x + y for x, y in zip(asum, a)]
        n += 1
    print("\r" + " " * 40 + "\r", end="")
    if n == 0:
        return None
    return [v / n for v in gsum], [v / n for v in asum], n


def dominant(v):
    best = max(range(3), key=lambda i: abs(v[i]))
    return best, v[best]


def describe(v):
    return "  ".join(f"{AXES[i]}{v[i]:+7.3f}" for i in range(3))


def main():
    node = find_imu()
    if not node:
        print("XREAL IMU not found. Are the glasses plugged in?")
        return 1
    print(f"IMU: {node}\n")

    fd = os.open(node, os.O_RDWR)
    body = bytes([0x04, 0x00, 0x19, 0x01])
    write_report(fd, bytes([0xAA]) + struct.pack("<I", crc(body)) + body)
    time.sleep(0.3)

    poses = [
        ("LEVEL", "Rest the glasses FLAT on the desk, lenses forward, and leave them alone."),
        ("NOSE DOWN", "Tip the FRONT of the glasses down about 45 degrees and hold still."),
        ("LEFT EAR DOWN", "Roll the glasses left about 45 degrees, left temple down, hold still."),
    ]
    # Timed rather than prompted. Whoever is moving the glasses usually cannot see this
    # output -- it runs over ssh -- so the schedule is fixed and printed in advance instead.
    hold = 10.0
    settle = 6.0
    print("  Schedule (each pose: get into it, then hold still):")
    for i, (name, instruction) in enumerate(poses):
        print(f"    t={i * hold:4.0f}s  {name:14s} {instruction}")
    print(f"\n  Measuring the last {hold - settle:.0f}s of each {hold:.0f}s window.\n")

    results = {}
    for name, instruction in poses:
        print(f"  {name}: {instruction}")
        # Discard the move itself; only the held pose is a measurement.
        average(fd, settle, "get into position")
        out = average(fd, hold - settle, "measuring")
        if not out:
            print("    no samples; is something else holding the device?")
            os.close(fd)
            return 1
        g, a, n = out
        results[name] = a
        print(f"    accel {describe(a)}   ({n} samples)\n")

    os.close(fd)

    level = results["LEVEL"]
    up_axis, up_value = dominant(level)
    print("=" * 62)
    print(f"  head UP      = sensor {AXES[up_axis]}  (sign {'+' if up_value > 0 else '-'})")

    # The axis that moved most between level and nose-down is the forward axis; likewise
    # between level and ear-down for the left axis. Differences, not absolutes, because the
    # resting orientation is not exactly level and gravity leaks into every axis a little.
    for pose, label in [("NOSE DOWN", "FORWARD"), ("LEFT EAR DOWN", "LEFT")]:
        delta = [results[pose][i] - level[i] for i in range(3)]
        # Ignore the up axis: gravity leaves it in both poses, and it will often show the
        # largest change without saying anything about which way the head turned.
        delta[up_axis] = 0.0
        axis, value = dominant(delta)
        print(f"  head {label:8s} = sensor {AXES[axis]}  (sign {'+' if value > 0 else '-'})"
              f"   [delta {describe(delta)}]")

    print("=" * 62)
    print()
    print("  Spatiand's canonical frame is +X forward, +Y left, +Z up, and AxisMap names")
    print("  the sensor axis for each of yaw (about up), pitch (about left) and roll")
    print("  (about forward). So:")
    print("     yaw_axis   = the UP axis")
    print("     pitch_axis = the LEFT axis")
    print("     roll_axis  = the FORWARD axis")
    print()
    print("  Signs still need the gyro's sense, but the axis assignment above is absolute:")
    print("  it comes from gravity, not from integrating a movement.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
