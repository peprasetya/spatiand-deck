#!/usr/bin/env bash
# Capture the full USB conversation with the glasses, plus connector state, across a plug-in
# and a mode switch.
#
# The point is to learn the exact sequence that makes 3840x1080 appear, so Spatiand can
# reproduce it deliberately instead of depending on the glasses happening to be freshly
# plugged. What we know so far: the MCU acks a side-by-side command and genuinely changes its
# internal mode (R_DISP_MODE reads back 0x04), but the DisplayPort side keeps advertising the
# 2D timing - so something about link-up, not the command, is what publishes the wider mode.
#
#   capture-usb.sh start    - begin capturing; plug the glasses in after this
#   capture-usb.sh switch   - send the SBS command while still capturing
#   capture-usb.sh report   - summarise what was captured
#   capture-usb.sh stop
set -uo pipefail

# Logs live under $HOME, never /tmp: SteamOS clears /tmp on reboot, and a reboot in the
# middle of an investigation is exactly when the capture matters most.
mkdir -p "${SPATIAND_LOGS:-$HOME/spatiand-logs}"

USBMON="${SPATIAND_LOGS:-$HOME/spatiand-logs}/usbmon.log"
STATE="${SPATIAND_LOGS:-$HOME/spatiand-logs}/state.log"
# Cache credentials once, up front, then use plain sudo.
#
# `echo pass | sudo -S cmd &` does not work: backgrounding detaches stdin, so sudo has no
# password to read and the command never starts - silently, which is how the usbmon capture
# came back empty while claiming to be running. sudo -v caches for this shell instead.
sudo_ready() {
    echo "${SUDO_PASS:-}" | sudo -S -v 2>/dev/null || {
        echo "!! sudo credentials rejected; set SUDO_PASS" >&2
        exit 1
    }
}
S() { sudo "$@"; }

state_line() {
    printf '[%s] dp1=%-12s usb=%s modes="%s"\n' \
        "$(date +%H:%M:%S.%3N)" \
        "$(cat /sys/class/drm/card0-DP-1/status 2>/dev/null)" \
        "$(lsusb 2>/dev/null | grep -c 3318)" \
        "$(tr '\n' ' ' < /sys/class/drm/card0-DP-1/modes 2>/dev/null | cut -c1-46)"
}

case "${1:-report}" in
start)
    sudo_ready
    # Stop AND reset-failed: a transient unit that exited leaves its name claimed, and
    # systemd-run then refuses to start a new one with the same name. That refusal is what
    # made the capture silently not run.
    echo "${SUDO_PASS:-}" | sudo -S systemctl stop spatiand-usbmon >/dev/null 2>&1
    echo "${SUDO_PASS:-}" | sudo -S systemctl reset-failed spatiand-usbmon >/dev/null 2>&1
    pkill -f _state-sampler >/dev/null 2>&1
    # Remove as root: dd runs as root under systemd, so a previous capture leaves a
    # root-owned file the desktop user cannot truncate. Failing to clear it silently was
    # enough to stop the whole capture starting.
    echo "${SUDO_PASS:-}" | sudo -S rm -f "$USBMON" >/dev/null 2>&1
    rm -f "$STATE"; : > "$STATE"
    S modprobe usbmon

    # Bus 0 is "all buses". Noisier - the Steam Controller alone polls at ~250 Hz - but the
    # glasses may enumerate on either port, and guessing the bus wrong loses the whole plug.
    #
    # dd rather than `cat > file`: the redirection has to happen as root, and wrapping it in
    # sh -c through sudo through ssh is exactly the nested-quoting shape that has failed
    # silently twice already. dd takes the destination as an argument, so nothing needs
    # quoting.
    # systemd-run rather than backgrounding sudo.
    #
    # Over SSH there is no tty, so `sudo -v` does not cache and every sudo needs the password
    # on stdin - which backgrounding takes away. The result was a capture that reported
    # itself running and recorded nothing. systemd-run hands the job to systemd as root and
    # returns immediately, with no stdin to lose.
    echo "${SUDO_PASS:-}" | sudo -S systemd-run --unit=spatiand-usbmon --collect \
        dd if=/sys/kernel/debug/usb/usbmon/0u of="$USBMON" bs=1 >/dev/null 2>&1

    setsid nohup bash "$(dirname "$0")/_state-sampler.sh" "$STATE" 900 >/dev/null 2>&1 &

    sleep 3
    # Verify rather than assume. An empty capture that is reported as running wastes whoever
    # is holding the glasses, and that has already happened once.
    ok=true
    systemctl is-active --quiet spatiand-usbmon || { echo "!! usbmon capture is NOT running"; ok=false; }
    pgrep -f _state-sampler >/dev/null || { echo "!! state sampler is NOT running"; ok=false; }
    [ -s "$STATE" ] || { echo "!! state log is empty"; ok=false; }
    if [ "$ok" = true ]; then
        echo "capturing (verified)"
    else
        echo "capture did NOT start cleanly - do not plug anything yet"
    fi
    before=$(wc -l < "$USBMON" 2>/dev/null || echo 0)
    sleep 2
    after=$(wc -l < "$USBMON" 2>/dev/null || echo 0)
    echo "  usbmon lines: $before -> $after $([ "$after" -gt "$before" ] && echo '(growing)' || echo '(NOT GROWING)')"
    state_line
    ;;

switch)
    echo "--- before ---"; state_line
    python3 "$(dirname "$0")/xr_setmode.py" read
    echo "--- switching to SBS ---"
    python3 "$(dirname "$0")/xr_setmode.py" sbs
    for i in 1 2 3 4 5 6; do sleep 2; printf 't+%2ds ' "$((i*2))"; state_line; done
    grep -q 3840 /sys/class/drm/card0-DP-1/modes 2>/dev/null \
        && echo ">>> 3840 AVAILABLE <<<" || echo ">>> no 3840 <<<"
    ;;

report)
    echo "== state timeline =="; cat "$STATE" 2>/dev/null || echo "  (none)"
    echo
    echo "== usbmon size =="; wc -l "$USBMON" 2>/dev/null || echo "  (none)"
    echo
    echo "== glasses traffic (vendor device lines) =="
    # usbmon lines are: tag time event addr status len data. Filter to the glasses' bus/dev
    # once we know it; for now show enumeration-shaped control transfers.
    grep -E "^[0-9a-f]+ [0-9]+ [CS] Ci" "$USBMON" 2>/dev/null | head -20 || echo "  (none)"
    ;;

stop)
    sudo_ready
    S pkill -f "cat /sys/kernel/debug/usb/usbmon" >/dev/null 2>&1
    pkill -f spatiand-state-sampler >/dev/null 2>&1
    echo "stopped; logs kept at $USBMON and $STATE"
    ;;
esac
