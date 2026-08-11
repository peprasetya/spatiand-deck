#!/usr/bin/env python3
"""
Spatiand M0b — Steam Deck controller probe. READ-ONLY: no feature reports, no mode
changes, nothing that could disturb a running session.

Answers: which hidraw node carries the 64-byte vendor reports, whether they flow, and
whether the documented Deck layout decodes plausibly.
"""
import glob, os, select, struct, subprocess, sys, time
from math import sqrt

VID_PID = "0003:000028DE:00001205"


def nodes():
    out = []
    for n in sorted(glob.glob("/sys/class/hidraw/hidraw*"), key=lambda p: int(p.split("hidraw")[-1])):
        ue = open(os.path.join(n, "device/uevent")).read()
        if VID_PID in ue:
            phys = grp = "?"
            for line in ue.splitlines():
                if line.startswith("HID_PHYS="):
                    phys = line.split("=", 1)[1]
                if line.startswith("HID_ID="):
                    grp = line.split("=", 1)[1]
            out.append(("/dev/" + os.path.basename(n), phys, grp))
    return out


def who_holds(path):
    try:
        r = subprocess.run(["fuser", "-v", path], capture_output=True, text=True, timeout=5)
        return (r.stdout + r.stderr).strip() or "(nobody)"
    except Exception:
        return "(fuser unavailable)"


def capture(path, seconds):
    """Collect reports; return list of bytes."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    except OSError as e:
        print(f"  cannot open {path}: {e}")
        return []
    pkts = []
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], max(0.0, end - time.time()))
        if not r:
            continue
        try:
            pkts.append(os.read(fd, 128))
        except BlockingIOError:
            pass
    os.close(fd)
    return pkts


def varying(pkts):
    """Byte offsets that are not constant across the capture."""
    if len(pkts) < 2:
        return []
    n = min(len(p) for p in pkts)
    return [i for i in range(n) if len({p[i] for p in pkts}) > 1]


def decode(p):
    """Hypothesis: SDL's ValveInReportDeck layout. Print what it implies."""
    if len(p) < 64:
        return
    ver, typ, ln = struct.unpack_from("<HBB", p, 0)
    print(f"    header: version={ver:#06x} type={typ:#04x} len={ln} "
          f"{'(matches documented 0x0001/0x09/0x40)' if (ver, typ, ln) == (1, 9, 64) else '(UNEXPECTED)'}")
    seq = struct.unpack_from("<I", p, 4)[0]
    btn = struct.unpack_from("<Q", p, 8)[0] & 0xFFFFFFFFFFFFFF
    lpx, lpy, rpx, rpy = struct.unpack_from("<hhhh", p, 16)
    ax, ay, az = struct.unpack_from("<hhh", p, 24)
    gx, gy, gz = struct.unpack_from("<hhh", p, 30)
    print(f"    seq={seq}  buttons={btn:#016x}")
    print(f"    left_pad=({lpx:+6d},{lpy:+6d})  right_pad=({rpx:+6d},{rpy:+6d})")
    print(f"    accel=({ax:+6d},{ay:+6d},{az:+6d})  |a|={sqrt(ax*ax+ay*ay+az*az):.0f} "
          f"(expect ~16384 = 1 g if the offset is right)")
    print(f"    gyro =({gx:+6d},{gy:+6d},{gz:+6d})")


def main():
    print("== hid-steam nodes ==")
    found = nodes()
    for path, phys, grp in found:
        print(f"  {path}  phys={phys}  id={grp}")
        print(f"      held by: {who_holds(path)}")
    if not found:
        sys.exit("no Steam Deck controller found")

    print("\n== lizard-mode / emulation state ==")
    devs = open("/proc/bus/input/devices").read()
    print("  virtual pad present:", "Microsoft X-Box 360 pad" in devs,
          "(present => Steam is running and owns the controller)")

    print("\n== capture: 4s per node ==")
    print("  >>> slide a finger on BOTH touchpads and press some buttons now <<<\n")
    for path, phys, _ in found:
        pkts = capture(path, 4.0)
        if not pkts:
            print(f"  {path} ({phys}): no reports\n")
            continue
        sizes = sorted({len(p) for p in pkts})
        v = varying(pkts)
        print(f"  {path} ({phys}): {len(pkts)} reports, sizes={sizes}, "
              f"{len(pkts)/4.0:.0f} Hz")
        print(f"    first: {pkts[0][:32].hex(' ')}")
        print(f"    varying byte offsets: {v[:40]}{' …' if len(v) > 40 else ''}")
        if 64 in sizes:
            decode(next(p for p in pkts if len(p) >= 64))
        print()


main()
