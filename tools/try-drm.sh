#!/usr/bin/env bash
# Run Spatiand on the real display WITHOUT touching the session configuration.
#
# Switching the SDDM session to test a compositor is a bad trade: every failed attempt leaves
# the machine mid-transition, and steamos-manager has proven fragile when driven that way.
# This stops the display manager, runs the compositor under seatd for a bounded time, and
# always puts the desktop back - on success, failure, crash, timeout or Ctrl-C.
set -uo pipefail

DURATION="${1:-120}"
BIN="$HOME/spatiand/target/release/spatiand"
LOG=/tmp/spatiand-drm.log
S() { echo "$SUDO_PASS" | sudo -S "$@" 2>/dev/null; }

restore() {
    echo
    echo "== restoring the desktop =="
    S pkill -x spatiand
    S pkill -x seatd
    S systemctl start sddm
    for _ in $(seq 1 20); do
        sleep 2
        pgrep -x plasmashell >/dev/null && { echo "   desktop is back."; return; }
    done
    echo "   !! desktop did not come back; run: sudo systemctl restart sddm"
}
trap restore EXIT INT TERM

[[ -x "$BIN" ]] || { echo "!! $BIN missing"; exit 1; }

echo "== stopping the display manager =="
S systemctl stop sddm
sleep 3

echo "== starting seatd =="
# With SDDM stopped there is no logind session on seat0, so a process launched over SSH
# cannot take DRM master through logind. seatd provides the seat instead; deck is in wheel.
S seatd -g wheel >/tmp/seatd.log 2>&1 &
sleep 2
[[ -S /run/seatd.sock ]] || { echo "!! seatd socket missing; see /tmp/seatd.log"; cat /tmp/seatd.log; exit 1; }
echo "   seatd up."

echo "== running spatiand for ${DURATION}s =="
LIBSEAT_BACKEND=seatd SPATIAND_BACKEND=drm RUST_BACKTRACE=1 \
    timeout --foreground "$DURATION" "$BIN" >"$LOG" 2>&1
rc=$?
echo "== spatiand exited (rc=$rc) =="
echo "--- log ---"
tail -40 "$LOG"
