#!/usr/bin/env bash
# Confirm the Steam Deck touchpad/button report offsets.
#
# Steam must not be running: while it is, it configures the controller for its own use and
# every payload field reads zero (see docs/steam-deck-controller.md §3). This stops Steam,
# runs the probe, and always puts Steam back — including on Ctrl-C.
set -uo pipefail

restart_steam() {
    echo
    echo "== restarting Steam =="
    # A plain shell has none of the session environment Steam needs; borrow it from a
    # process that is already inside the session.
    local pid
    pid=$(pgrep -x plasmashell | head -1)
    if [[ -n "$pid" ]]; then
        eval "$(tr '\0' '\n' < "/proc/$pid/environ" \
                | grep -E '^(XDG_RUNTIME_DIR|WAYLAND_DISPLAY|DISPLAY|DBUS_SESSION_BUS_ADDRESS|XAUTHORITY)=' \
                | sed 's/^/export /;s/=/="/;s/$/"/')"
    fi
    setsid steam -silent >/tmp/steam-restart.log 2>&1 </dev/null &
    sleep 8
    if pgrep -x steam >/dev/null; then
        echo "   Steam is back up."
    else
        echo "   !! Steam did not restart. Launch it from the taskbar, or see /tmp/steam-restart.log"
    fi
}
trap restart_steam EXIT

echo "== stopping Steam =="
pkill -x steam
sleep 4
pgrep -x steam >/dev/null && echo "   (still running — the probe may read zeros)" || echo "   stopped."
echo

python3 "$(dirname "$0")/deck_pads.py" --touch
