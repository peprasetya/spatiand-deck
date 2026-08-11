#!/usr/bin/env bash
# Grant persistent access to the XR glasses and the Steam Deck controller.
#
# By default these hidraw nodes are readable only through a logind ACL, which exists solely
# for the *active session on seat0*. That is fine when Spatiand is started by SDDM, and not
# fine in two cases that matter:
#
#   * running the compositor over SSH for development, where there is no seat0 session at
#     all - the glasses simply cannot be opened, and the symptom is Spatiand quietly falling
#     back to the built-in panel rather than any error about permissions;
#   * anything that needs the devices before or between sessions.
#
# OWNER as well as GROUP, deliberately. Group permissions alone were not enough: logind
# leaves an ACL on these nodes whose mask clamps the effective group bits once the seat0
# session goes away, so `deck` still got EACCES despite being in wheel. An ACL mask does not
# apply to the file owner, so OWNER is what actually survives. Still not MODE=0666 as the
# reverse-engineering notes suggest - there is no reason for every process on the machine to
# be able to send MCU commands to the glasses.
#
# SPATIAND_USER can override the owner on a system where the desktop user is not `deck`.
#
# /etc survives SteamOS updates (unlike /usr), so unlike the session entry this only needs
# installing once.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "!! Run with sudo:  sudo $0" >&2
    exit 1
fi

RULES=/etc/udev/rules.d/99-spatiand.rules
OWNER="${SPATIAND_USER:-${SUDO_USER:-deck}}"
echo "== granting access to user: $OWNER =="

cat > "$RULES" <<EOF
# XREAL / Nreal glasses - IMU (interface 3) and MCU (interface 4).
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="3318", OWNER="$OWNER", GROUP="wheel", MODE="0660"

# Valve Steam Deck / Steam Controller - the vendor interface carrying absolute touchpad
# coordinates, pressure and the IMU, none of which evdev exposes.
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="28de", OWNER="$OWNER", GROUP="wheel", MODE="0660"
EOF

echo "== installed $RULES =="
cat "$RULES"

udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw
echo
echo "== current permissions =="
sleep 1
for n in /dev/hidraw*; do
    printf '%s  ' "$(ls -l "$n" | awk '{print $1, $3, $4, $NF}')"
    echo
done
