#!/usr/bin/env bash
# Who owns the GPU, and is the connector actually healthy?
#
# Two questions that turned out to matter more than expected:
#
#   1. Does anything still hold DRM master after the display manager stops? SteamOS ships
#      /usr/bin/gamescope-wayland-teardown-workaround and its own sddm config calls it a
#      "janky workaround for wayland sessions not stopping in sddm" - so a surviving kwin is
#      a documented failure mode, not a wild guess. A compositor that still holds the device
#      stops Spatiand owning the connector properly.
#
#   2. Is EDID readable? A mode list of exactly "800x600 640x480" is the kernel's synthesized
#      fallback, used when hotplug is asserted but EDID could not be read at all. That is a
#      different failure from the glasses being in 2D, where they advertise a full list. Zero
#      bytes in the edid file confirms it.
#
# Run it whenever the display is behaving oddly - it is read-only.
set -uo pipefail
S() { echo "${SUDO_PASS:-}" | sudo -S "$@" 2>/dev/null; }

echo "=== compositors / display manager still alive? ==="
found=false
for p in kwin_wayland kwin_x11 plasmashell gamescope gamescope-wl sddm sddm-helper Xwayland spatiand weston; do
    pids=$(pgrep -x "$p" 2>/dev/null | tr '\n' ' ')
    [ -n "$pids" ] && { printf '  %-16s pids: %s\n' "$p" "$pids"; found=true; }
done
$found || echo "  (nothing - the GPU should be free)"

echo
echo "=== who has /dev/dri/card0 open ==="
S fuser -v /dev/dri/card0 2>&1 | grep -v "^$" || echo "  (nobody)"

echo
echo "=== drm clients (the one with master owns modesetting) ==="
S cat /sys/kernel/debug/dri/0/clients 2>/dev/null || echo "  (debugfs not readable)"

echo
echo "=== connector state ==="
for c in /sys/class/drm/card0-*/; do
    name=$(basename "$c")
    [ -e "$c/status" ] || continue
    status=$(cat "$c/status" 2>/dev/null)
    edid_bytes=$(wc -c < "$c/edid" 2>/dev/null || echo 0)
    modes=$(tr '\n' ' ' < "$c/modes" 2>/dev/null | cut -c1-56)
    printf '  %-20s %-13s edid=%4s bytes  dpms=%-4s modes: %s\n' \
        "$name" "$status" "$edid_bytes" "$(cat "$c/dpms" 2>/dev/null || echo '?')" "$modes"
done

echo
echo "=== verdict on DP-1 ==="
dp_status=$(cat /sys/class/drm/card0-DP-1/status 2>/dev/null)
dp_edid=$(wc -c < /sys/class/drm/card0-DP-1/edid 2>/dev/null || echo 0)
dp_modes=$(tr '\n' ' ' < /sys/class/drm/card0-DP-1/modes 2>/dev/null)
if [ "$dp_status" != "connected" ]; then
    echo "  not connected - nothing to say"
elif [ "$dp_edid" -eq 0 ]; then
    echo "  CONNECTED BUT NO EDID. The kernel is falling back to generic modes; the glasses"
    echo "  are not answering on AUX. This is the unexplained failure - capture dmesg now."
elif echo "$dp_modes" | grep -q 3840; then
    echo "  healthy, side-by-side available"
else
    echo "  healthy, 2D only (${dp_edid} bytes of EDID) - glasses are in a mono mode"
fi

echo
echo "=== recent DP / EDID / AUX kernel messages ==="
S dmesg 2>/dev/null | grep -iE "DP-1|edid|link training|aux|dpcd|amdgpu.*connector" | tail -15 \
    || echo "  (none)"
