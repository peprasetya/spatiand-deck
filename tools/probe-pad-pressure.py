#!/usr/bin/env python3
"""
Confirm where pad pressure lives in the Deck's input report.

`docs/steam-deck-controller.md` §5 lists offsets 56-59 as unidentified, and the kernel's
struct puts the two pad pressures exactly there. That is a hypothesis, not a fact, and the
scroll filter in `spatiand-input::scroll` leans on it — so this settles it.

Unlike `deck_pads.py` this one cannot self-validate: pressure has no known physical value the
way gravity does. It needs a thumb. So it watches every unexplained 16-bit field at once and
reports which ones move in step with the pad being touched, which is a much stronger answer
than reading the one offset we already suspect and finding a number in it.

Run it, follow the prompts, and read the verdict.
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
GYRO_ACCEL_AND_GYRO = 0x18

# Bit positions from crates/spatiand-input/src/layout.rs.
BIT_LPAD_TOUCH = 19
BIT_RPAD_TOUCH = 20

# Every 16-bit field whose meaning is not already nailed down, plus the two we expect to win.
CANDIDATES = list(range(48, 64, 2))


def hidiocsfeature(size):
    return (3 << 30) | (size << 16) | (0x48 << 8) | 0x06


def send_feature(fd, payload):
    buf = bytearray(65)
    buf[1 : 1 + len(payload)] = payload
    fcntl.ioctl(fd, hidiocsfeature(len(buf)), bytes(buf))


def find_node():
    for path in glob.glob("/sys/class/hidraw/hidraw*"):
        uevent = os.path.join(path, "device", "uevent")
        try:
            text = open(uevent).read()
        except OSError:
            continue
        if VID_PID in text and "input2" in text:
            return "/dev/" + os.path.basename(path)
    # Fall back to any node from this device: the interface suffix is not always spelled the
    # same, and picking the wrong one fails loudly on the header check below rather than
    # silently reporting nonsense.
    for path in glob.glob("/sys/class/hidraw/hidraw*"):
        try:
            if VID_PID in open(os.path.join(path, "device", "uevent")).read():
                return "/dev/" + os.path.basename(path)
        except OSError:
            continue
    return None


def take(fd):
    """Lizard mode off, gyro on — the same claim spatiand-input::takeover makes."""
    send_feature(fd, bytes([ID_CLEAR_DIGITAL_MAPPINGS, 0]))
    send_feature(fd, bytes([ID_LOAD_DEFAULT_SETTINGS, 0]))
    payload = bytearray([ID_SET_SETTINGS_VALUES, 9])
    for reg, value in (
        (REG_LPAD_MODE, 0x07),
        (REG_RPAD_MODE, 0x07),
        (REG_GYRO_MODE, GYRO_ACCEL_AND_GYRO),
    ):
        payload += struct.pack("<BH", reg, value)
    send_feature(fd, bytes(payload))


def sample(fd, seconds, want):
    """Collect (offset -> [values]), keeping only frames matching `want`.

    `want` is None for untouched, "left" or "right" for exactly that pad and no other — the
    exclusivity matters, because a frame with both thumbs down cannot tell the two fields
    apart and that is the whole question being asked.
    """
    seen = {off: [] for off in CANDIDATES}
    frames = 0
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if not r:
            continue
        data = os.read(fd, 64)
        if len(data) < 64 or data[:4] != HEADER:
            continue
        buttons = struct.unpack_from("<Q", data, 8)[0]
        left = bool(buttons & (1 << BIT_LPAD_TOUCH))
        right = bool(buttons & (1 << BIT_RPAD_TOUCH))
        state = None
        if left and not right:
            state = "left"
        elif right and not left:
            state = "right"
        elif left and right:
            state = "both"
        if state != want:
            continue
        frames += 1
        for off in CANDIDATES:
            seen[off].append(struct.unpack_from("<H", data, off)[0])
    return seen, frames


def moved(samples):
    """Did this field take a real spread of non-zero values?"""
    return bool(samples) and max(samples) > 0 and (max(samples) - min(samples)) > 50


def main():
    node = find_node()
    if not node:
        sys.exit("no Valve controller found")
    print(f"reading {node}")
    fd = os.open(node, os.O_RDWR | os.O_NONBLOCK)
    take(fd)
    time.sleep(0.3)

    print("\n>>> HANDS OFF the controller. Sampling 4 s...")
    idle, idle_frames = sample(fd, 4.0, None)
    print(f"    {idle_frames} untouched frames")

    print("\n>>> LEFT pad only. Press and slide, hard and soft, for 7 s...")
    left, left_frames = sample(fd, 7.0, "left")
    print(f"    {left_frames} left-only frames")

    print("\n>>> RIGHT pad only. Press and slide, hard and soft, for 7 s...")
    right, right_frames = sample(fd, 7.0, "right")
    print(f"    {right_frames} right-only frames")

    if left_frames < 20 or right_frames < 20:
        sys.exit("\nnot enough frames — each pad needs a thumb on it, one at a time")

    print("\noffset   untouched      left pad       right pad      verdict")
    found = {}
    for off in CANDIDATES:
        i, l, r = idle[off], left[off], right[off]

        def span(v):
            return f"{min(v):>5}..{max(v):<5}" if v else "     -     "

        # Pressure for one pad is zero when untouched, moves for that pad, and stays put for
        # the other. That last clause is what actually assigns the field to a side; without it
        # a shared or mislabelled field looks exactly like the right answer.
        verdict = ""
        if max(i) == 0 and moved(l) and not moved(r):
            verdict, found["left"] = "<-- LEFT pad pressure", off
        elif max(i) == 0 and moved(r) and not moved(l):
            verdict, found["right"] = "<-- RIGHT pad pressure", off
        elif max(i) == 0 and moved(l) and moved(r):
            verdict = "<-- moves for BOTH: shared, not per-pad"
        print(f"  {off:>3}   {span(i)}  {span(l)}  {span(r)}   {verdict}")

    print()
    if len(found) == 2:
        print(f"VERDICT: left pad pressure at {found['left']}, right at {found['right']}.")
        print("Expected left 56, right 58 (the kernel's struct order).")
    else:
        print(f"VERDICT: inconclusive — found {found}. Do not trust 56/58.")
    os.close(fd)


if __name__ == "__main__":
    main()
