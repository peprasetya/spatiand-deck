#!/usr/bin/env python3
"""
Set the XREAL display mode and leave it there.

Unlike xr_spike.py this does not restore mono on the way out - it exists to leave the glasses
in a known state while something else observes the consequences.

  xr_setmode.py 2d | sbs | sbs60 | read
"""
import glob, os, select, struct, sys, time, zlib

VID = "0003:00003318:00000424"
MODES = {
    "2d": 0x05,      # 1920x1080 @72
    "sbs": 0x04,     # 3840x1080 @72
    "sbs60": 0x03,   # 3840x1080 @60
}
MSG_W_DISP_MODE = 0x0008
MSG_R_DISP_MODE = 0x0007


def find_hidraw(suffix):
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


def mcu_packet(msgid, data):
    body = struct.pack("<H", 17 + len(data))
    body += struct.pack("<Q", int(time.time() * 1000))
    body += struct.pack("<H", msgid) + b"\x00" * 5 + bytes(data)
    return bytes([0xFD]) + struct.pack("<I", zlib.crc32(body) & 0xFFFFFFFF) + body


def command(fd, msgid, data, timeout=1.5):
    os.write(fd, b"\x00" + mcu_packet(msgid, data))
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], max(0.0, end - time.time()))
        if not r:
            break
        p = os.read(fd, 64)
        if len(p) < 17:
            continue
        echoed = struct.unpack_from("<H", p, 15)[0]
        if echoed == msgid:
            return p
        print(f"    (async push {echoed:#06x})", flush=True)
    return None


# Payload starts after head(1) + crc(4) + len(2) + timestamp(8) + msgid(2) + reserved(5).
MCU_DATA_OFFSET = 22
# ...but a READ REPLY is not laid out like a command. Observed for R_DISP_MODE (msgid 0x07):
#
#   fd 80f8726a 1600 6698baef1d6af420 0700 0000000000 00 04 ...
#                    ^len=22          ^msg ^reserved  ^  ^mode
#
# The body length is 22 rather than the 18 a one-byte command uses, and byte 22 is 0x00 with
# the actual value at 23 - so 22 reads as a status/result code and the payload follows it.
# Taking 22 as the value silently reports "mode 0x00" for every read, which looks like the
# glasses not answering rather than a layout mistake.
MCU_REPLY_VALUE_OFFSET = 23
MODE_NAMES = {
    0x1: "1920x1080@60 2D",
    0x3: "3840x1080@60 SBS",
    0x4: "3840x1080@72 SBS",
    0x5: "1920x1080@72 2D",
    0x8: "1920x1080@60 SBS",
    0x9: "3840x1080@90 SBS",
    0xA: "1920x1080@90 2D",
    0xB: "1920x1080@120 2D",
}


def describe(reply):
    if not reply or len(reply) <= MCU_REPLY_VALUE_OFFSET:
        return "no reply"
    status = reply[MCU_DATA_OFFSET]
    raw = reply[MCU_REPLY_VALUE_OFFSET] if len(reply) > MCU_REPLY_VALUE_OFFSET else 0
    out = f"mode {raw:#04x} = {MODE_NAMES.get(raw, 'unknown')} (status {status:#04x})"
    if os.environ.get("XR_DUMP"):
        # Read replies may not carry their payload at the same offset as commands. Dump the
        # whole frame and let the bytes settle it rather than trusting the assumption.
        body_len = struct.unpack_from("<H", reply, 5)[0]
        out += (
            f"\n      raw len={len(reply)} body_len={body_len}"
            f"\n      {reply[:32].hex(' ')}"
            f"\n      {reply[32:64].hex(' ')}"
            f"\n      offsets 15..30: {' '.join(f'{i}:{reply[i]:02x}' for i in range(15, min(31, len(reply))))}"
        )
    return out


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "read"
    mcu = find_hidraw("/input4")
    if not mcu:
        sys.exit("glasses MCU not found")
    fd = os.open(mcu, os.O_RDWR)
    try:
        if what == "read":
            print(f"  {describe(command(fd, MSG_R_DISP_MODE, []))}")
            return
        if what not in MODES:
            sys.exit(f"unknown mode {what!r}; expected one of {', '.join(MODES)} or read")
        value = MODES[what]
        print(f"setting display mode {what} ({value:#04x})", flush=True)
        print(f"  before: {describe(command(fd, MSG_R_DISP_MODE, []))}", flush=True)
        reply = command(fd, MSG_W_DISP_MODE, [value])
        print("  write acked" if reply else "  write NOT acked", flush=True)
        time.sleep(1.5)
        # Reading the mode back is what separates "the write was ignored" from "the glasses
        # believe they switched but are not driving a wider signal". The write ack alone says
        # nothing: it is returned even when the mode does not change.
        print(f"  after:  {describe(command(fd, MSG_R_DISP_MODE, []))}", flush=True)
    finally:
        os.close(fd)


main()
