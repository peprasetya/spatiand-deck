#!/usr/bin/env bash
# Does the room come back when the application holding it is killed?
#
# It did not, for one release. `sky_owner` was released by the polite `destroy` request and by
# nothing else, so a single SIGKILL left the claim pointing at a surface that no longer
# existed and every later request for `equirect_180` / `equirect_360` — from the same
# application restarted, or from any other — was refused for the life of the compositor.
#
# It reached us as "VR180 plays in the window, and switching to All around you does nothing",
# which is why this is a script and not a paragraph: the symptom is three steps away from the
# cause, and the only cheap way to know it has not come back is to kill something and look.
#
# Not a `cargo test`. The fault lives in the lifetime of a Wayland object across two client
# processes, and there is no way to reach that from a unit test — `Spatiand` needs a display,
# a client needs a socket, and SIGKILL needs a process.
#
#   tools/sky-handover.sh                        # against the built binary
#   tools/sky-handover.sh /tmp/spatiand-before   # against another one, to see it fail
set -uo pipefail

REPO="${SPATIAND_REPO:-$HOME/spatiand}"
BIN="${1:-$REPO/target/release/spatiand}"
PROBE="${SPATIAND_PROBE:-$HOME/probes/stereo-probe}"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[[ -x "$BIN" ]] || { echo "no compositor at $BIN" >&2; exit 1; }
if [[ ! -x "$PROBE" ]]; then
    cat >&2 <<EOF
no probe at $PROBE. Build it inside the container:

  mkdir -p ~/probes && cd ~/probes
  X=$REPO/crates/spatiand-proto/protocol/spatiand-xr-v1.xml
  wayland-scanner client-header "\$X" spatiand-xr-v1-client-protocol.h
  wayland-scanner private-code  "\$X" spatiand-xr-v1-protocol.c
  P=\$(pkg-config --variable=pkgdatadir wayland-protocols)
  wayland-scanner client-header "\$P/stable/xdg-shell/xdg-shell.xml" xdg-shell-client-protocol.h
  wayland-scanner private-code  "\$P/stable/xdg-shell/xdg-shell.xml" xdg-shell-protocol.c
  wayland-scanner client-header "\$P/stable/viewporter/viewporter.xml" viewporter-client-protocol.h
  wayland-scanner private-code  "\$P/stable/viewporter/viewporter.xml" viewporter-protocol.c
  cc -o stereo-probe $REPO/tools/stereo-probe.c xdg-shell-protocol.c \\
     spatiand-xr-v1-protocol.c viewporter-protocol.c -I. \$(pkg-config --cflags --libs wayland-client)
EOF
    exit 1
fi

# Two clients in one compositor lifetime. The first takes the room and is killed without
# ceremony -- SIGKILL, so no `destroy` request is ever sent and the compositor's only notice
# is the connection going away. The second asks for exactly the same thing.
cat > "$WORK/twice.sh" <<EOF
#!/bin/bash
export SPATIAND_LAYER=equirect360
"$PROBE" 2>"$WORK/first.txt" &
sleep 4
pkill -9 -x stereo-probe
sleep 1
"$PROBE" 2>"$WORK/second.txt" &
sleep 10
EOF
chmod +x "$WORK/twice.sh"

echo "== killing an application that is the room, then asking for it again =="
SPATIAND_BACKEND=snapshot \
SPATIAND_SNAPSHOT="$WORK/shot.png" \
SPATIAND_SNAPSHOT_SIZE=640x400 \
SPATIAND_CLIENT="$WORK/twice.sh" \
SPATIAND_CLIENT_WAIT=20 \
RUST_LOG=info "$BIN" 2>&1 |
    grep -iE "layer_refused|environment is free|still claimed" | sed 's/^/  /'

echo
if grep -q "refused" "$WORK/second.txt" 2>/dev/null; then
    echo "FAIL: the room was never given back."
    echo "      $(grep refused "$WORK/second.txt")"
    echo
    echo "  Whoever claimed it is gone and cannot ask for 'window' again, so nothing short of"
    echo "  restarting the session can free it. See \`release\` and \`release_dead_sky\` in"
    echo "  crates/spatiand/src/xr.rs — both destroy paths have to run the same body."
    exit 1
fi
if ! grep -q "asked for layer" "$WORK/second.txt" 2>/dev/null; then
    echo "INCONCLUSIVE: the second client never got as far as asking for a layer."
    echo "              Check that it connected at all:"
    sed 's/^/                /' "$WORK/second.txt" 2>/dev/null
    exit 2
fi
echo "PASS: the second application was given the room."
