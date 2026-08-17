#!/usr/bin/env python3
"""
Record what a touchpad actually reports through a swipe, a press and a lift.

Written because the scroll filter has now been guessed at twice and been wrong twice. Every
threshold in `spatiand-input::scroll` is a claim about how the hardware behaves across those
three moments, and none of them had ever been checked against the hardware doing it.

Writes a CSV of raw reports. `tools/analyse-pad-trace.py` reads it back, decimates it to the
frame rate the compositor actually samples at, and says what the filter would have done.

Both pads are recorded whatever you touch, so one run covers either hand.
"""
import fcntl, glob, os, select, struct, sys, time

VID_PID = "0003:000028DE:00001205"
HEADER = bytes([0x01, 0x00, 0x09, 0x40])

ID_CLEAR_DIGITAL_MAPPINGS = 0x81
ID_SET_SETTINGS_VALUES = 0x87
ID_LOAD_DEFAULT_SETTINGS = 0x8E
REG_LPAD_MODE = 0x07
REG_RPAD_MODE = 0x08
REG_GYRO_MODE = 0x30

BIT_LPAD_CLICK = 17
BIT_RPAD_CLICK = 18
BIT_LPAD_TOUCH = 19
BIT_RPAD_TOUCH = 20


def hidiocsfeature(size):
    return (3 << 30) | (size << 16) | (0x48 << 8) | 0x06


def send_feature(fd, payload):
    buf = bytearray(65)
    buf[1 : 1 + len(payload)] = payload
    fcntl.ioctl(fd, hidiocsfeature(len(buf)), bytes(buf))


def find_node():
    for path in glob.glob("/sys/class/hidraw/hidraw*"):
        try:
            if VID_PID in open(os.path.join(path, "device", "uevent")).read():
                return "/dev/" + os.path.basename(path)
        except OSError:
            continue
    return None


def take(fd):
    send_feature(fd, bytes([ID_CLEAR_DIGITAL_MAPPINGS, 0]))
    send_feature(fd, bytes([ID_LOAD_DEFAULT_SETTINGS, 0]))
    payload = bytearray([ID_SET_SETTINGS_VALUES, 9])
    for reg, value in ((REG_LPAD_MODE, 0x07), (REG_RPAD_MODE, 0x07), (REG_GYRO_MODE, 0x18)):
        payload += struct.pack("<BH", reg, value)
    send_feature(fd, bytes(payload))


def main():
    out_path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/pad-trace.csv"
    node = find_node()
    if not node:
        sys.exit("no Valve controller found")
    fd = os.open(node, os.O_RDWR | os.O_NONBLOCK)
    take(fd)
    time.sleep(0.3)

    print(f"reading {node}, writing {out_path}\n")
    print("Do these three things on the LEFT pad, pausing a beat between them:")
    print("  1. a slow steady swipe up, then lift off cleanly")
    print("  2. a quick flick, then lift off")
    print("  3. rest your thumb, press the pad down to click, release, then lift")
    print("\nRecording for 20 s starting now...")

    rows = []
    t0 = time.time()
    end = t0 + 20.0
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if not r:
            continue
        data = os.read(fd, 64)
        if len(data) < 64 or data[:4] != HEADER:
            continue
        seq = struct.unpack_from("<I", data, 4)[0]
        buttons = struct.unpack_from("<Q", data, 8)[0]
        lx, ly = struct.unpack_from("<hh", data, 16)
        rx, ry = struct.unpack_from("<hh", data, 20)
        lp = struct.unpack_from("<H", data, 56)[0]
        rp = struct.unpack_from("<H", data, 58)[0]
        rows.append(
            (
                round(time.time() - t0, 5),
                seq,
                lx, ly,
                int(bool(buttons & (1 << BIT_LPAD_TOUCH))),
                int(bool(buttons & (1 << BIT_LPAD_CLICK))),
                lp,
                rx, ry,
                int(bool(buttons & (1 << BIT_RPAD_TOUCH))),
                int(bool(buttons & (1 << BIT_RPAD_CLICK))),
                rp,
            )
        )
    os.close(fd)

    with open(out_path, "w") as f:
        f.write("t,seq,lx,ly,ltouch,lclick,lpressure,rx,ry,rtouch,rclick,rpressure\n")
        for row in rows:
            f.write(",".join(str(v) for v in row) + "\n")

    touched = sum(1 for r in rows if r[4] or r[9])
    clicked = sum(1 for r in rows if r[5] or r[10])
    print(f"\n{len(rows)} reports, {touched} with a thumb down, {clicked} with a click held")
    if touched < 100:
        print("WARNING: very little contact recorded — was a thumb on a pad?")
    if clicked == 0:
        print("WARNING: no click recorded — step 3 is the important one")
    print(f"wrote {out_path}")


if __name__ == "__main__":
    main()
