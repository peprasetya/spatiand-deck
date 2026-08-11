#!/usr/bin/env bash
# Record connector and USB events to a file, detached, so a replug can happen whenever.
#
# The earlier attempt watched for a fixed window and caught nothing, because prompts printed
# here do not reach whoever is holding the glasses until the command finishes. This runs in
# the background instead: start it, replug at leisure, then read the log.
#
#   record-hotplug.sh start [seconds]   default 600
#   record-hotplug.sh report
#   record-hotplug.sh stop
set -uo pipefail

# Logs live under $HOME, never /tmp: SteamOS clears /tmp on reboot, and a reboot in the
# middle of an investigation is exactly when the capture matters most.
mkdir -p "${SPATIAND_LOGS:-$HOME/spatiand-logs}"

LOG="${SPATIAND_LOGS:-$HOME/spatiand-logs}/hotplug.log"
UDEV="${SPATIAND_LOGS:-$HOME/spatiand-logs}/udev.log"
S() { echo "${SUDO_PASS:-}" | sudo -S "$@" 2>/dev/null; }

case "${1:-report}" in
start)
    DURATION="${2:-600}"
    S pkill -f "udevadm monitor" >/dev/null 2>&1
    pkill -f "spatiand-hotplug-sampler" >/dev/null 2>&1
    : > "$LOG"; : > "$UDEV"

    S sh -c "udevadm monitor --kernel --udev --property > $UDEV 2>&1" &
    setsid bash -c '
        exec -a spatiand-hotplug-sampler bash -c "
            end=\$((SECONDS + '"$DURATION"'))
            last=
            while [ \$SECONDS -lt \$end ]; do
                st=\$(cat /sys/class/drm/card0-DP-1/status 2>/dev/null)
                md=\$(tr \"\\n\" \" \" < /sys/class/drm/card0-DP-1/modes 2>/dev/null | cut -c1-40)
                usb=\$(lsusb 2>/dev/null | grep -c 3318)
                now=\"\$st|\$md|usb=\$usb\"
                if [ \"\$now\" != \"\$last\" ]; then
                    echo \"[\$(date +%H:%M:%S)] \$now\" >> '"$LOG"'
                    last=\$now
                fi
                sleep 0.5
            done
        "' >/dev/null 2>&1 &
    sleep 1
    echo "recording for ${DURATION}s"
    echo "  state changes: $LOG"
    echo "  udev events:   $UDEV"
    echo
    echo "Unplug and replug the glasses whenever you are ready, then run: $0 report"
    ;;

report)
    echo "== connector / usb state changes =="
    cat "$LOG" 2>/dev/null || echo "  (nothing recorded)"
    echo
    echo "== drm uevents =="
    grep -B 2 -A 6 "SUBSYSTEM=drm" "$UDEV" 2>/dev/null | grep -E "^(KERNEL|UDEV)\[|ACTION=|DEVNAME=|HOTPLUG|CONNECTOR" | head -30 \
        || echo "  (none)"
    echo
    echo "== usb events for the glasses =="
    grep -B 2 -A 4 "3318\|idVendor=3318" "$UDEV" 2>/dev/null | grep -E "^(KERNEL|UDEV)\[|ACTION=|PRODUCT=" | head -20 \
        || echo "  (none)"
    echo
    echo "== totals =="
    printf '  udev lines: %s\n' "$(wc -l < "$UDEV" 2>/dev/null || echo 0)"
    grep -oE "^(KERNEL|UDEV)\[[0-9.]+\] (add|remove|change|bind|unbind)" "$UDEV" 2>/dev/null \
        | awk '{print $2}' | sort | uniq -c | sed 's/^/  /' || true
    ;;

stop)
    S pkill -f "udevadm monitor" >/dev/null 2>&1
    pkill -f "spatiand-hotplug-sampler" >/dev/null 2>&1
    echo "stopped"
    ;;
esac
