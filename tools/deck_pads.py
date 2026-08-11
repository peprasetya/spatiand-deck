#!/usr/bin/env python3
"""
Steam Deck controller: disable lizard mode, enable gyro, confirm the report layout.

Self-validating without anyone touching the device: the accelerometer is a known physical
quantity, so if |accel| reads ~1 g at the hypothesised offset the layout is right. Register
and command ids follow drivers/hid/hid-steam.c.
"""
import fcntl, glob, os, select, struct, sys, time
from math import sqrt

VID_PID = "0003:000028DE:00001205"

ID_CLEAR_DIGITAL_MAPPINGS = 0x81
ID_SET_SETTINGS_VALUES    = 0x87
ID_LOAD_DEFAULT_SETTINGS  = 0x8E

REG_RPAD_MARGIN         = 0x18
REG_LPAD_MODE           = 0x07
REG_RPAD_MODE           = 0x08
REG_GYRO_MODE           = 0x30
REG_LPAD_CLICK_PRESSURE = 0x34
REG_RPAD_CLICK_PRESSURE = 0x35

GYRO_ACCEL_AND_GYRO = 0x18


def hidiocsfeature(size):
    return (3 << 30) | (size << 16) | (0x48 << 8) | 0x06


def find_vendor_node():
    """The vendor interface is the one on .../input2 (HID group 0103)."""
    for n in sorted(glob.glob("/sys/class/hidraw/hidraw*")):
        ue = open(os.path.join(n, "device/uevent")).read()
        if VID_PID not in ue:
            continue
        for line in ue.splitlines():
            if line.startswith("HID_PHYS=") and line.endswith("/input2"):
                return "/dev/" + os.path.basename(n)
    return None


def send_feature(fd, payload):
    buf = bytearray(65)
    buf[0] = 0x00                      # report id: these are unnumbered reports
    buf[1:1 + len(payload)] = payload
    fcntl.ioctl(fd, hidiocsfeature(len(buf)), bytes(buf))


def write_registers(fd, pairs):
    body = bytearray()
    for reg, val in pairs:
        body += bytes([reg]) + struct.pack("<H", val)
    send_feature(fd, bytes([ID_SET_SETTINGS_VALUES, len(body)]) + body)


def set_lizard(fd, enabled):
    if enabled:
        # Hand the device back to its own keyboard/mouse emulation.
        send_feature(fd, bytes([ID_LOAD_DEFAULT_SETTINGS, 0]))
    else:
        write_registers(fd, [
            (REG_RPAD_MARGIN, 0),
            (REG_LPAD_MODE, 0x07),            # stop emulating a mouse
            (REG_RPAD_MODE, 0x07),
            (REG_LPAD_CLICK_PRESSURE, 0xFFFF),  # stop the pads acting as clicks
            (REG_RPAD_CLICK_PRESSURE, 0xFFFF),
        ])
        send_feature(fd, bytes([ID_CLEAR_DIGITAL_MAPPINGS, 0]))


def capture(fd, seconds):
    pkts, end = [], time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], max(0.0, end - time.time()))
        if r:
            pkts.append(os.read(fd, 128))
    return pkts


def decode(p):
    """SDL's ValveInReportDeck layout."""
    ver, typ, ln = struct.unpack_from("<HBB", p, 0)
    seq = struct.unpack_from("<I", p, 4)[0]
    btn = struct.unpack_from("<Q", p, 8)[0] & 0x00FFFFFFFFFFFFFF
    lpx, lpy, rpx, rpy = struct.unpack_from("<hhhh", p, 16)
    ax, ay, az = struct.unpack_from("<hhh", p, 24)
    gx, gy, gz = struct.unpack_from("<hhh", p, 30)
    trig_l, trig_r = struct.unpack_from("<HH", p, 44)
    return dict(ver=ver, typ=typ, ln=ln, seq=seq, btn=btn,
                lpad=(lpx, lpy), rpad=(rpx, rpy), accel=(ax, ay, az),
                gyro=(gx, gy, gz), trig=(trig_l, trig_r))


def main():
    path = find_vendor_node()
    print(f"== vendor node: {path} ==")
    if not path:
        sys.exit("controller not found")

    fd = os.open(path, os.O_RDWR)
    try:
        print("\n== before (as the kernel left it) ==")
        pkts = capture(fd, 1.5)
        if pkts:
            d = decode(pkts[-1])
            print(f"  accel={d['accel']}  gyro={d['gyro']}  lpad={d['lpad']}  rpad={d['rpad']}")

        print("\n== disabling lizard mode + enabling gyro ==")
        set_lizard(fd, False)
        write_registers(fd, [(REG_GYRO_MODE, GYRO_ACCEL_AND_GYRO)])
        time.sleep(0.3)

        pkts = capture(fd, 3.0)
        print(f"  {len(pkts)} reports, {len(pkts)/3.0:.0f} Hz")
        if not pkts:
            sys.exit("  no reports after reconfiguration")

        # Which byte offsets actually move now?
        n = min(len(p) for p in pkts)
        varying = [i for i in range(n) if len({p[i] for p in pkts}) > 1]
        print(f"  varying offsets: {varying}")

        d = decode(pkts[-1])
        print(f"\n  header  version={d['ver']:#06x} type={d['typ']:#04x} len={d['ln']}")
        print(f"  buttons {d['btn']:#016x}")
        print(f"  lpad    {d['lpad']}   rpad {d['rpad']}")
        print(f"  accel   {d['accel']}")
        print(f"  gyro    {d['gyro']}")
        print(f"  trig    {d['trig']}")

        # --- interactive: the pads and buttons need someone to touch them ---
        if "--touch" in sys.argv:
            print("\n== pad + button test ==")
            print("  Put BOTH thumbs on BOTH touchpads and hold some buttons down.")
            for n in range(6, 0, -1):
                print(f"    capturing in {n}...", flush=True)
                time.sleep(1)
            print("    CAPTURING — keep holding!", flush=True)
            touch = capture(fd, 3.0)
            # Pick the report with the most non-zero payload, so a moment of lifted
            # thumbs mid-capture does not decide the result.
            pkts = [max(touch, key=lambda q: sum(1 for b in q[8:24] if b))] if touch else pkts
            t = decode(pkts[-1])
            print(f"  lpad    {t['lpad']}   rpad {t['rpad']}")
            print(f"  buttons {t['btn']:#016x}")
            print(f"  trig    {t['trig']}")
            moved = t["lpad"] != (0, 0) or t["rpad"] != (0, 0)
            print(f"  PADS:    {'CONFIRMED at offsets 16..24' if moved else 'still zero — offsets are wrong'}")
            print(f"  BUTTONS: {'CONFIRMED at offset 8' if t['btn'] else 'still zero — offset is wrong'}")
            if not moved:
                print("  scanning for a pair that moved off zero:")
                base = decode(pkts[0])
                for off in range(8, 48, 2):
                    v = struct.unpack_from("<hh", pkts[-1], off)
                    if any(abs(x) > 500 for x in v):
                        print(f"    offset {off}: {v}  <-- candidate")

        mag = sqrt(sum(v * v for v in d["accel"]))
        # The Deck reports 1 g as 0x4000 = 16384.
        print(f"\n  |accel| = {mag:.0f} counts = {mag/16384:.3f} g   (expect ~1.00)")
        ok = 0.8 < mag / 16384 < 1.2
        print(f"  LAYOUT: {'CONFIRMED' if ok else 'NOT CONFIRMED'}")
        if not ok:
            print("  (accel offset 24 is a hypothesis; scan for the pair that reads 1 g)")
            for off in range(8, 56, 2):
                v = struct.unpack_from("<hhh", pkts[-1], off)
                m = sqrt(sum(x * x for x in v))
                if 0.8 < m / 16384 < 1.2:
                    print(f"    offset {off}: {v} -> {m/16384:.3f} g  <-- candidate")
    finally:
        print("\n== restoring lizard mode ==")
        try:
            set_lizard(fd, True)
        except Exception as e:
            print(f"  restore failed: {e}")
        os.close(fd)


main()
