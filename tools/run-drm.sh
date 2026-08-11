#!/usr/bin/env bash
# Run Spatiand as a real DRM session, with no desktop environment underneath.
#
# This stops the display manager, so the KDE desktop disappears for the duration. It ALWAYS
# comes back: the restore runs on normal exit, on Ctrl-C, on crash, and on timeout. Losing
# the desktop on a machine whose only screen is the thing being debugged is not a state to
# risk leaving behind.
#
#   tools/run-drm.sh [seconds]      default 180
#
# Seats: with SDDM stopped there is no logind session on seat0, so a compositor launched over
# SSH cannot take DRM master through logind. seatd provides the seat instead; `-g wheel` lets
# the desktop user talk to it, and `deck` is in wheel.
set -uo pipefail

DURATION="${1:-180}"
BIN="$HOME/spatiand/target/release/spatiand"
LOG=/tmp/spatiand-drm.log

restore() {
    echo
    echo "== restoring the desktop =="
    sudo pkill -x spatiand 2>/dev/null
    sudo pkill -x seatd 2>/dev/null
    sudo systemctl start sddm
    sleep 3
    if systemctl is-active --quiet sddm; then
        echo "   SDDM is back."
    else
        echo "   !! SDDM did not restart. Run: sudo systemctl start sddm"
    fi
}
trap restore EXIT INT TERM

if [[ ! -x "$BIN" ]]; then
    echo "!! $BIN missing. Build first:"
    echo "   distrobox enter --name spatiand -- bash -c 'cd ~/spatiand && cargo build --release -p spatiand'"
    exit 1
fi

echo "== stopping the desktop =="
sudo systemctl stop sddm
sleep 2

echo "== starting seatd =="
sudo seatd -g wheel >/tmp/seatd.log 2>&1 &
sleep 1
if [[ ! -S /run/seatd.sock ]]; then
    echo "!! seatd did not create its socket; see /tmp/seatd.log"
    exit 1
fi

echo "== running spatiand for ${DURATION}s =="
echo "   log: $LOG"
LIBSEAT_BACKEND=seatd \
SPATIAND_BACKEND=drm \
RUST_BACKTRACE=1 \
    timeout --foreground "$DURATION" "$BIN" 2>&1 | tee "$LOG"

echo "== spatiand exited (rc=$?) =="
