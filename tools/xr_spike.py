#!/usr/bin/env python3
"""
Spatiand M0 spike — validate XREAL Air SBS mode + IMU on the Steam Deck.

Protocol per HoloFrame/xrealAir.md:
  MCU (iface 4): FD | crc32(body) LE | body
                 body = len16 | ts64 | msgid16 | 5x00 | data
                 len counts ITSELF onward  -> 18 for a 1-byte payload
  IMU (iface 3): AA | crc32(body) LE | body
                 body = len16 | msgid8 | data8   -> len = 4 (counts itself)
"""
import glob, os, select, struct, subprocess, sys, time, zlib
from math import sqrt

VID = "0003:00003318:00000424"
MODE_2D_72  = 0x05   # 1920x1080 @72
MODE_SBS_72 = 0x04   # 3840x1080 @72  <- preferred
MODE_SBS_60 = 0x03   # 3840x1080 @60  <- fallback


def find_hidraw(suffix):
    """Locate the hidraw node for a given USB interface (…/inputN)."""
    for node in sorted(glob.glob("/sys/class/hidraw/hidraw*")):
        try:
            ue = open(os.path.join(node, "device/uevent")).read()
        except OSError:
            continue
        if VID not in ue:
            continue
        for line in ue.splitlines():
            if line.startswith("HID_PHYS=") and line.endswith(suffix):
                return "/dev/" + os.path.basename(node)
    return None


def crc(body):
    return zlib.crc32(bytes(body)) & 0xFFFFFFFF


def selfcheck():
    """Verify our framing reproduces the captures documented in xrealAir.md."""
    body = bytes([0x04, 0x00, 0x19, 0x01])
    pkt = bytes([0xAA]) + struct.pack("<I", crc(body)) + body
    want = bytes.fromhex("aac5d12142040019 01".replace(" ", ""))
    ok = pkt == want
    print(f"  CRC self-check vs documented capture: {'PASS' if ok else 'FAIL'}  {pkt.hex()}")
    return ok


def mcu_packet(msgid, data):
    body = struct.pack("<H", 17 + len(data))          # 2+8+2+5+len = 18 for 1 byte
    body += struct.pack("<Q", int(time.time() * 1000))
    body += struct.pack("<H", msgid) + b"\x00" * 5 + bytes(data)
    return bytes([0xFD]) + struct.pack("<I", crc(body)) + body


def write_report(fd, pkt):
    """hidraw wants a leading report-id byte; these devices use unnumbered reports."""
    try:
        os.write(fd, b"\x00" + pkt)
        return "0x00-prefixed"
    except OSError:
        os.write(fd, pkt)
        return "raw"


def read_reply(fd, want_msgid=0x0008, timeout=1.5):
    """Drain async pushes (0x6Cxx) until the ack echoing our msgid turns up."""
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], max(0.0, end - time.time()))
        if not r:
            break
        p = os.read(fd, 64)
        if len(p) < 17:
            continue
        mid = struct.unpack_from("<H", p, 15)[0]
        if mid == want_msgid:
            return p, mid
        print(f"     (async push msgid {mid:#06x}, ignoring)")
    return None, None


def dp_modes():
    try:
        return open("/sys/class/drm/card0-DP-1/modes").read().split()
    except OSError:
        return []


def set_mode(fd, mode, label):
    pkt = mcu_packet(0x0008, [mode])
    how = write_report(fd, pkt)
    print(f"  -> W_DISP_MODE {mode:#04x} ({label}) [{how}]  {pkt.hex()}")
    reply, echo = read_reply(fd)
    print(f"  <- ack msgid {echo:#06x}" if reply else "  <- NO ACK (msgid 0x0008 never echoed)")
    for _ in range(16):                       # let the link retrain + EDID re-read
        time.sleep(0.5)
        if any(m.startswith("3840x1080") for m in dp_modes()):
            break
    modes = dp_modes()
    print(f"  DP-1 modes now: {' '.join(modes[:6])}{' …' if len(modes) > 6 else ''}")
    return any(m.startswith("3840x1080") for m in modes)


def i24(b, o):
    v = b[o] | b[o + 1] << 8 | b[o + 2] << 16
    return v - (1 << 24) if v & 0x800000 else v


def imu_test(path, seconds=3.0):
    fd = os.open(path, os.O_RDWR)
    body = bytes([0x04, 0x00, 0x19, 0x01])
    write_report(fd, bytes([0xAA]) + struct.pack("<I", crc(body)) + body)
    n = 0
    acc_m = mag_m = 0.0
    gsum = [0.0, 0.0, 0.0]
    t0 = time.time()
    while time.time() - t0 < seconds:
        r, _, _ = select.select([fd], [], [], 0.5)
        if not r:
            continue
        p = os.read(fd, 64)
        if len(p) < 54 or p[0] != 0x01 or p[1] != 0x02:
            continue
        gm, gd = struct.unpack_from("<h", p, 12)[0], struct.unpack_from("<i", p, 14)[0]
        am, ad = struct.unpack_from("<h", p, 27)[0], struct.unpack_from("<i", p, 29)[0]
        mm, md = struct.unpack_from(">h", p, 42)[0], struct.unpack_from(">i", p, 44)[0]
        if not (gd and ad and md):
            continue                                   # first packets arrive zeroed
        g = [i24(p, 18 + 3 * i) * gm / gd for i in range(3)]
        a = [i24(p, 33 + 3 * i) * am / ad for i in range(3)]
        # offset binary -> two's complement: XOR then reinterpret those 16 bits as signed
        m = []
        for i in range(3):
            v = struct.unpack_from("<H", p, 48 + 2 * i)[0] ^ 0x8000
            m.append((v - 0x10000 if v & 0x8000 else v) * mm / md)
        if n == 0:
            print(f"  scalers: gyro {gm}/{gd}  accel {am}/{ad}  mag {mm}/{md}")
        if all(v == 0 for v in a):
            continue
        n += 1
        acc_m += sqrt(sum(v * v for v in a))
        mag_m += sqrt(sum(v * v for v in m))
        gsum = [s + v for s, v in zip(gsum, g)]
    body = bytes([0x04, 0x00, 0x19, 0x00])
    write_report(fd, bytes([0xAA]) + struct.pack("<I", crc(body)) + body)
    os.close(fd)
    if not n:
        print("  no IMU data packets received")
        return False
    dt = time.time() - t0
    print(f"  samples {n} in {dt:.1f}s = {n/dt:.0f} Hz   (expect ~1067)")
    print(f"  |accel| {acc_m/n:.3f} g   (expect ~1.00)")
    print(f"  |mag|   {mag_m/n:.3f} G   (expect ~0.30)")
    print(f"  gyro bias  {gsum[0]/n:+.2f} {gsum[1]/n:+.2f} {gsum[2]/n:+.2f} deg/s")
    return 0.9 < acc_m / n < 1.1


def main():
    print("== framing ==")
    selfcheck()

    imu, mcu = find_hidraw("/input3"), find_hidraw("/input4")
    print(f"\n== devices ==\n  IMU {imu}\n  MCU {mcu}")
    if not (imu and mcu):
        sys.exit("glasses not found")

    print(f"\n== DP-1 before ==\n  {' '.join(dp_modes()[:5])}")

    fd = os.open(mcu, os.O_RDWR)
    got = False
    try:
        print("\n== switch to SBS 3840x1080@72 ==")
        got = set_mode(fd, MODE_SBS_72, "3840x1080@72")
        if not got:
            print("\n== @72 did not appear; trying SBS 3840x1080@60 ==")
            got = set_mode(fd, MODE_SBS_60, "3840x1080@60")
        print(f"\n  RESULT: 3840x1080 {'AVAILABLE' if got else 'NOT AVAILABLE'}")
        if got:
            print("  holding SBS for 5s so you can look through the glasses…")
            time.sleep(5)
    finally:
        print("\n== restoring 2D 1920x1080@72 ==")
        set_mode(fd, MODE_2D_72, "1920x1080@72")
        os.close(fd)

    print("\n== IMU ==")
    imu_ok = imu_test(imu)

    print(f"\n=== M0 GATE: SBS {'PASS' if got else 'FAIL'} | IMU {'PASS' if imu_ok else 'FAIL'} ===")


main()
