#!/usr/bin/env python3
"""Hand the Steam Deck's controller back to its own firmware.

Run this if a button stops responding — the left bumper was the one that found it — and a
reboot does not bring it back.

Claiming the controller means clearing its *digital button mappings* in firmware, and handing
it back means `ID_LOAD_DEFAULT_SETTINGS`. Spatiand does that in `Drop`, which covers a normal
exit and a signal, but cannot cover a `SIGKILL` or a hard power loss. When it is missed, the
mapping stays cleared inside the controller's own microcontroller — which keeps power across a
soft reboot, and in practice across a shutdown too, since the Deck does not fully de-power it.
So the symptom outlives every obvious remedy and looks exactly like a failed button.

This sends the one command that undoes it, and nothing else.

    sudo python3 tools/restore-controller.py
"""
import fcntl
import glob
import os
import sys

FEATURE_BUF_LEN = 65
ID_LOAD_DEFAULT_SETTINGS = 0x8E

VENDOR = 0x28DE
PRODUCT = 0x1205


def hidiocsfeature(length: int) -> int:
    """`HIDIOCSFEATURE(len)` — `_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, len)`."""
    return (3 << 30) | (length << 16) | (ord("H") << 8) | 0x06


def controller_nodes():
    """Every hidraw node belonging to the Deck's built-in controller.

    Matched on the parsed HID_ID fields rather than on a substring of the uevent: the kernel
    writes them zero-padded as `0003:000028DE:00001205`, so the obvious search for
    "28DE:1205" finds nothing at all and the script reports no controller on a machine that
    plainly has one.
    """
    for sysfs in sorted(glob.glob("/sys/class/hidraw/hidraw*")):
        try:
            uevent = open(os.path.join(sysfs, "device/uevent")).read()
        except OSError:
            continue
        fields = dict(
            line.split("=", 1) for line in uevent.splitlines() if "=" in line
        )
        hid_id = fields.get("HID_ID", "")
        parts = hid_id.split(":")
        if len(parts) != 3:
            continue
        try:
            vendor, product = int(parts[1], 16), int(parts[2], 16)
        except ValueError:
            continue
        if (vendor, product) != (VENDOR, PRODUCT):
            continue
        yield "/dev/" + os.path.basename(sysfs), fields.get("HID_PHYS", "")


def restore(node: str) -> bool:
    buf = bytearray(FEATURE_BUF_LEN)
    # Report id 0: these are unnumbered reports, so the payload starts at byte 1.
    buf[1] = ID_LOAD_DEFAULT_SETTINGS
    try:
        fd = os.open(node, os.O_RDWR)
    except OSError as e:
        print(f"  {node}: cannot open — {e}")
        return False
    try:
        fcntl.ioctl(fd, hidiocsfeature(FEATURE_BUF_LEN), buf, True)
        print(f"  {node}: default settings restored")
        return True
    except OSError as e:
        # Only the vendor interface answers this. The keyboard and mouse interfaces time out,
        # which is expected and not a problem worth reporting as a failure.
        print(f"  {node}: not the vendor interface ({e.strerror})")
        return False
    finally:
        os.close(fd)


nodes = list(controller_nodes())
if not nodes:
    print("No Steam Deck controller found.")
    sys.exit(1)

print(f"Found {len(nodes)} controller interface(s):")
if not any(restore(node) for node, _ in nodes):
    print("\nNothing accepted the command. Are you root?")
    sys.exit(1)
print("\nDone. Try the button again.")
