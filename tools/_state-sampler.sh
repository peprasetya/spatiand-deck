#!/usr/bin/env bash
# Sample DisplayPort connector state and glasses presence, logging only on change.
#
# A separate file on purpose. Earlier versions inlined this inside nested bash -c quoting
# passed through ssh, and it died after one sample without saying so - which is worse than
# not capturing at all, because the empty log looks like "nothing happened".
STATE_LOG="${1:-/tmp/spatiand-state.log}"
DURATION="${2:-900}"

last=""
end=$((SECONDS + DURATION))
while [ $SECONDS -lt $end ]; do
    status=$(cat /sys/class/drm/card0-DP-1/status 2>/dev/null)
    modes=$(tr '\n' ' ' < /sys/class/drm/card0-DP-1/modes 2>/dev/null | cut -c1-60)
    usb=$(lsusb 2>/dev/null | grep -c 3318)
    now="dp1=$status usb=$usb modes=\"$modes\""
    if [ "$now" != "$last" ]; then
        printf '[%s] %s\n' "$(date +%H:%M:%S.%2N)" "$now" >> "$STATE_LOG"
        last="$now"
    fi
    sleep 0.3
done
