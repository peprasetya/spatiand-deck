#!/usr/bin/env bash
# One-shot bring-up: axis calibration, then the touchpad/button confirmation.
#
# Needs no typing — the Deck is an awkward machine to type on. Every step is either timed or
# triggered by a movement. Launch it in a Konsole window on the Deck itself, under `script`
# so there is a real PTY (see below).
#
# Two things this deliberately does NOT do, both learned the hard way:
#
#   * It does not run anything through `distrobox enter`. The container is a BUILD
#     environment only. Rust binaries built inside it run natively on SteamOS — same Arch
#     base, same glibc — and `distrobox enter` from a non-interactive script hangs.
#   * It does not redirect its own output through `tee`. Doing so replaces stdout with a
#     pipe, so Rust block-buffers instead of line-buffering and the prompts never appear
#     until the program exits, which looks exactly like a hang. Logging is the caller's job,
#     via `script`, which keeps a PTY.
set -uo pipefail

BIN="$HOME/spatiand/target/release/examples"

banner() {
    echo
    echo "════════════════════════════════════════════════════════════"
    echo "  $*"
    echo "════════════════════════════════════════════════════════════"
    echo
}

pause() {
    for ((i = $1; i > 0; i--)); do
        echo "  starting in $i..."
        sleep 1
    done
    echo
}

restart_steam() {
    banner "Restarting Steam"
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
        echo "  Steam is back up."
    else
        echo "  !! Steam did not restart — launch it from the taskbar."
    fi
}

if [[ ! -x "$BIN/axis_calibrate" ]]; then
    echo "!! $BIN/axis_calibrate is missing. Build it first:"
    echo "   distrobox enter --name spatiand -- bash -c 'cd ~/spatiand && cargo build --release --examples'"
    exit 1
fi

banner "Spatiand bring-up — part 1 of 2: head-tracking axes"
echo "  PUT THE GLASSES ON, then shake your head to begin."
echo "  Three movements follow, each with a countdown: turn left, look down, tilt left."
echo "  Move SLOWLY and hold each one until told to stop."
pause 5

"$BIN/axis_calibrate"

banner "Part 2 of 2: touchpads and buttons"
echo "  You can take the glasses off now."
echo "  Steam will be stopped for this, and restarted automatically afterwards."
pause 5

trap restart_steam EXIT

echo "  Stopping Steam..."
pkill -x steam
sleep 4
pgrep -x steam >/dev/null && echo "  (still running — results may read zero)" || echo "  stopped."

python3 "$HOME/spatiand/tools/deck_pads.py" --touch

banner "Done"
echo "  This window stays open — close it when you have read the results."
