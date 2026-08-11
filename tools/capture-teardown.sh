#!/usr/bin/env bash
# Capture USB, kernel and connector state ACROSS a display-manager teardown and a replug.
#
# Everything captured so far came from a healthy plug with KDE running - i.e. the case that
# already works. The interesting window is the one we have no data for:
#
#     sddm stops  ->  link state changes  ->  glasses replugged  ->  ???
#
# That is where DP-1 has twice come back asserting hotplug with no readable EDID (the
# 800x600 + 640x480 fallback list), and we have never seen the wire during it.
#
# Every collector runs as a systemd SYSTEM unit, on purpose. A capture started from the
# desktop session dies with the session - which is exactly the moment being investigated -
# and it dies silently, leaving an empty log that reads as "nothing happened". Logs go under
# $HOME because SteamOS clears /tmp on reboot, which has already destroyed one capture.
#
#   capture-teardown.sh arm          start the collectors (do this with KDE up)
#   capture-teardown.sh teardown     stop sddm and free the GPU, still capturing
#   capture-teardown.sh mark "text"  annotate the timeline (e.g. "replugged")
#   capture-teardown.sh switch       send the SBS command
#   capture-teardown.sh report       what happened, in order
#   capture-teardown.sh restore      bring the desktop back and stop capturing
set -uo pipefail

LOGS="${SPATIAND_LOGS:-$HOME/spatiand-logs}"
mkdir -p "$LOGS"
USBMON="$LOGS/teardown-usbmon.log"
KMSG="$LOGS/teardown-dmesg.log"
STATE="$LOGS/teardown-state.log"
S() { echo "${SUDO_PASS:-}" | sudo -S "$@" 2>/dev/null; }

unit_restart() { # name, then command
    local unit="$1"; shift
    S systemctl stop "$unit" >/dev/null 2>&1
    S systemctl reset-failed "$unit" >/dev/null 2>&1
    S systemd-run --unit="$unit" --collect "$@" >/dev/null 2>&1
    systemctl is-active --quiet "$unit"
}

note() { printf '[%s] === %s ===\n' "$(date +%H:%M:%S.%2N)" "$1" >> "$STATE"; }

case "${1:-report}" in
arm)
    S modprobe usbmon
    S rm -f "$USBMON" "$KMSG"
    rm -f "$STATE"; : > "$STATE"

    ok=true
    unit_restart spatiand-usbmon dd if=/sys/kernel/debug/usb/usbmon/0u of="$USBMON" bs=1 \
        || { echo "!! usbmon unit failed"; ok=false; }
    # Kernel messages are where DP link training and AUX failures show up, and they are the
    # only direct evidence for why EDID becomes unreadable.
    unit_restart spatiand-dmesg dmesg --follow --notime -w \
        || { echo "!! dmesg unit failed"; ok=false; }
    S sh -c "journalctl -f -k > $KMSG 2>&1 &" >/dev/null 2>&1
    unit_restart spatiand-state bash "$(cd "$(dirname "$0")" && pwd)/_state-sampler.sh" "$STATE" 1800 \
        || { echo "!! state sampler unit failed"; ok=false; }

    sleep 2
    a=$(wc -l < "$USBMON" 2>/dev/null || echo 0); sleep 2
    b=$(wc -l < "$USBMON" 2>/dev/null || echo 0)
    [ "$b" -gt "$a" ] || { echo "!! usbmon is not growing ($a -> $b)"; ok=false; }

    note "armed"
    if [ "$ok" = true ]; then
        echo "ARMED and verified (usbmon $a -> $b lines)"
        echo "  logs: $LOGS"
        echo "  next: capture-teardown.sh teardown"
    else
        echo "NOT armed cleanly - do not proceed"
    fi
    ;;

teardown)
    note "stopping sddm"
    S systemctl stop sddm
    sleep 4
    for p in plasmashell kwin_wayland kwin_x11 gamescope gamescope-wl Xwayland; do
        pgrep -x "$p" >/dev/null && { note "killing surviving $p"; S pkill -x "$p"; }
    done
    sleep 2
    if S fuser /dev/dri/card0 >/dev/null 2>&1; then
        note "GPU STILL HELD after teardown"
    else
        note "GPU free"
    fi
    note "teardown complete - replug the glasses now"
    echo "teardown done; capture still running."
    echo "  replug the glasses, then: capture-teardown.sh mark replugged"
    ;;

mark)  note "${2:-mark}"; echo "marked: ${2:-mark}" ;;

switch)
    note "sending SBS"
    python3 "$(dirname "$0")/xr_setmode.py" read || true
    python3 "$(dirname "$0")/xr_setmode.py" sbs || true
    sleep 4
    note "after SBS"
    ;;

report)
    echo "== timeline (state changes and marks, in order) =="
    cat "$STATE" 2>/dev/null || echo "  (none)"
    echo
    echo "== kernel: DP / EDID / AUX =="
    grep -iE "DP-1|edid|link training|aux|dpcd" "$KMSG" 2>/dev/null | tail -25 || echo "  (none)"
    echo
    echo "== usbmon =="
    wc -l "$USBMON" 2>/dev/null || echo "  (none)"
    ;;

restore)
    for u in spatiand-usbmon spatiand-dmesg spatiand-state; do
        S systemctl stop "$u" >/dev/null 2>&1
        S systemctl reset-failed "$u" >/dev/null 2>&1
    done
    S pkill -f "journalctl -f -k" >/dev/null 2>&1
    S systemctl start sddm
    for _ in $(seq 1 20); do
        sleep 2
        pgrep -x plasmashell >/dev/null && { echo "desktop is back; capture stopped"; exit 0; }
    done
    echo "!! desktop did not return; run: sudo systemctl restart sddm"
    ;;
esac
