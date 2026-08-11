#!/usr/bin/env bash
# Observe what the kernel reports when the glasses change display mode or are replugged.
#
# The open question: when the glasses renegotiate into side-by-side, does the kernel emit a
# DRM hotplug uevent? If it does, Spatiand should listen for it rather than polling
# get_connector, which is what it does now and which does not see the new mode. If it does
# not, polling is not the problem and something else is.
#
#   watch-hotplug.sh switch   - flip to SBS and watch
#   watch-hotplug.sh replug   - watch for 40s while you unplug and replug by hand
set -uo pipefail

MODE="${1:-switch}"
S() { echo "${SUDO_PASS:-}" | sudo -S "$@" 2>/dev/null; }
UDEV_LOG=/tmp/udev-monitor.log
: > "$UDEV_LOG"

connector_state() {
    printf '    status=%s modes="%s"\n' \
        "$(cat /sys/class/drm/card0-DP-1/status 2>/dev/null)" \
        "$(tr '\n' ' ' < /sys/class/drm/card0-DP-1/modes 2>/dev/null | cut -c1-70)"
}

echo "== starting udev monitor =="
S udevadm monitor --kernel --udev --property --subsystem-match=drm --subsystem-match=usb \
    > "$UDEV_LOG" 2>&1 &
MON_PID=$!
cleanup() { S pkill -f "udevadm monitor" >/dev/null 2>&1; }
trap cleanup EXIT INT TERM
sleep 2

echo "== before =="
connector_state

case "$MODE" in
  switch)
    echo
    echo "== switching to SBS =="
    python3 "$(dirname "$0")/xr_setmode.py" sbs
    ;;
  replug)
    echo
    echo "== UNPLUG AND REPLUG THE GLASSES NOW - watching for 40s =="
    ;;
  *) echo "unknown mode $MODE"; exit 1 ;;
esac

DURATION=$([ "$MODE" = replug ] && echo 40 || echo 16)
for i in $(seq 1 $((DURATION / 2))); do
    sleep 2
    printf '  t+%2ds' "$((i * 2))"
    connector_state
done

sleep 1
cleanup
sleep 1

echo
echo "== DRM uevents seen =="
grep -A 12 "subsystem=drm\|SUBSYSTEM=drm" "$UDEV_LOG" 2>/dev/null \
    | grep -E "^(KERNEL|UDEV|ACTION|DEVNAME|HOTPLUG|CONNECTOR|SEQNUM|PROPERTY)" | head -40 \
    || echo "  (none)"

echo
echo "== event summary =="
grep -cE "^(KERNEL|UDEV)\[" "$UDEV_LOG" 2>/dev/null | xargs -I{} echo "  {} total events"
grep -oE "add|remove|change|bind|unbind" "$UDEV_LOG" 2>/dev/null | sort | uniq -c | sed 's/^/  /'
echo
echo "  full log: $UDEV_LOG"
