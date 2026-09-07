#!/usr/bin/env bash
# Register Spatiand as a SteamOS session ("spatial mode"), alongside desktop and game mode.
#
# Run this ONCE with sudo, and again after any SteamOS update — `/usr` is read-only and gets
# replaced wholesale by the atomic updater, so the session entry does not survive. Everything
# that matters (the binary, your calibration, your settings) lives under /home and does
# survive; only this registration is lost.
#
#   sudo tools/install-session.sh
#
# Afterwards, no root is needed again. Switch into spatial mode with:
#
#   steamosctl switch-to-desktop-mode spatiand.desktop
#
# and back with:
#
#   steamosctl switch-to-game-mode        # or switch-to-desktop-mode plasma.desktop
#
# Why a session rather than launching it by hand: SDDM starts it on seat0, which is what
# gives the compositor DRM master through logind. Launched from an SSH shell there is no seat
# and the compositor cannot take the display without running a separate seatd as root.
set -euo pipefail

# Bumped whenever the *content* of the session entry or the launcher below changes.
#
# Written into the entry and read back by go-to-spatiand.sh, which used to re-register only
# when the entry was missing -- so an entry that was present and out of date was left alone
# forever. That is not hypothetical: DesktopNames gained KDE, without which no Flatpak file
# chooser opens, and every machine that already had a session entry would have carried on
# without it.
ENTRY_REVISION=3

if [[ $EUID -ne 0 ]]; then
    echo "!! Run this with sudo:  sudo $0" >&2
    exit 1
fi

# pkexec does not set SUDO_USER, it sets PKEXEC_UID. go-to-spatiand.sh launches this via
# pkexec so the password prompt is graphical, so both paths have to work - otherwise the
# re-registration installs itself for root and the launcher points at /root.
REAL_USER="${SPATIAND_USER:-${SUDO_USER:-}}"
if [[ -z "$REAL_USER" && -n "${PKEXEC_UID:-}" ]]; then
    REAL_USER=$(getent passwd "$PKEXEC_UID" | cut -d: -f1)
fi
REAL_USER="${REAL_USER:-deck}"
echo "== installing for user: $REAL_USER =="
HOME_DIR=$(getent passwd "$REAL_USER" | cut -d: -f6)
BIN="$HOME_DIR/spatiand/target/release/spatiand"
LAUNCHER=/usr/local/bin/spatiand-session
DESKTOP=/usr/share/wayland-sessions/spatiand.desktop

if [[ ! -x "$BIN" ]]; then
    echo "!! $BIN not found or not executable." >&2
    echo "   Build it first (as $REAL_USER):" >&2
    echo "   distrobox enter --name spatiand -- bash -c 'cd ~/spatiand && cargo build --release -p spatiand'" >&2
    exit 1
fi

readonly_was_disabled=false
if command -v steamos-readonly >/dev/null && [[ "$(steamos-readonly status 2>/dev/null)" == "enabled" ]]; then
    echo "== unlocking the read-only root =="
    steamos-readonly disable
    readonly_was_disabled=true
fi
relock() {
    if [[ "$readonly_was_disabled" == true ]]; then
        echo "== relocking the read-only root =="
        steamos-readonly enable || echo "   !! could not relock; run: sudo steamos-readonly enable"
    fi
}
trap relock EXIT

echo "== installing launcher: $LAUNCHER =="
install -d /usr/local/bin
cat > "$LAUNCHER" <<EOF
#!/usr/bin/env bash
# Launched by SDDM.
#
# SPATIAND_BACKEND must be set explicitly. The default is the nested winit backend, which
# needs an existing compositor to be a client of — in a bare session there is none, and it
# fails with "Failed to initialize an event loop", which reads like a Spatiand bug rather
# than a wrong-backend one.
export SPATIAND_BACKEND=drm
export RUST_BACKTRACE=1
# Anything set in ~/.config/spatiand/session.env is exported into the session.
#
# There is no terminal to set a variable in front of, and reinstalling the session entry to
# turn on one log line is absurd. This is how a diagnosis gets made on a machine whose only
# screen is the thing being diagnosed -- write RUST_LOG in a file, restart, read the log.
ENVFILE="\$HOME/.config/spatiand/session.env"
if [ -f "\$ENVFILE" ]; then
    set -a
    . "\$ENVFILE"
    set +a
fi
# Log where it can be read after the fact: a session that fails at startup leaves no terminal
# to have shown the error in.
#
# The previous log is kept, because truncating it here destroys the only copy of exactly the
# thing worth reading. A session that dies is followed within seconds by SDDM starting
# another, and that next session opened this file with > and wiped the crash that caused it.
# One crash report arrived with nothing behind it for precisely that reason.
LOG="\$HOME/.local/share/spatiand-session.log"
mkdir -p "\$(dirname "\$LOG")"
[ -f "\$LOG" ] && mv -f "\$LOG" "\$LOG.1"
# Not exec'd, because there is one thing to do after it stops.
"$BIN" >"\$LOG" 2>&1
status=\$?

# Stop telling the rest of the machine that this compositor is here.
#
# Spatiand withdraws these itself on the way out, and this is the case it cannot cover: a
# crash, or a SIGKILL, runs no code of ours at all. The systemd user manager outlives the
# session, so a WAYLAND_DISPLAY left pointing at a socket that has gone is inherited by
# whatever starts next -- and game mode is the one that notices, because gamescope reads it,
# decides it should run nested inside us, and exits 1. Only a full power cycle cleared it.
#
# Harmless when Spatiand has already done it: unsetting a variable that is not set succeeds.
systemctl --user unset-environment WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE 2>/dev/null

exit \$status
EOF
chmod +x "$LAUNCHER"

echo "== installing session entry: $DESKTOP =="
install -d /usr/share/wayland-sessions
# Two names in DesktopNames, and the second one is insurance.
#
# XDG_CURRENT_DESKTOP is one of the things xdg-desktop-portal matches a backend against, and a
# name nothing has ever heard of matches nothing. That matters because a Flatpak application's
# file chooser is a portal call rather than a window the application opens for itself, so no
# backend means no dialog and no error either.
#
# Stated carefully, because it was first written as the whole explanation and is not: this
# machine's /usr/share/xdg-desktop-portal/portals.conf also says `default=kde`, and portal
# 1.20 honours that when UseIn does not match -- so the backend would have been found anyway.
# The name is worth setting because it is what the specification asks for and because not
# every machine has that default; the thing that was actually broken is next door, in the
# compositor: a portal service starting on demand had no WAYLAND_DISPLAY of ours to draw on.
# See `publish_session_environment`.
cat > "$DESKTOP" <<EOF
[Desktop Entry]
Name=Spatial Mode
Comment=Spatiand — 3D spatial desktop for XR glasses
Exec=$LAUNCHER
Type=Application
DesktopNames=Spatiand;KDE
X-Spatiand-Revision=$ENTRY_REVISION
EOF

echo
echo "Installed. To enter spatial mode (no root needed):"
echo "    steamosctl switch-to-desktop-mode spatiand.desktop"
echo
echo "To come back:"
echo "    steamosctl switch-to-game-mode"
echo "    steamosctl switch-to-desktop-mode plasma.desktop"
echo
echo "If a session fails to start, SDDM returns you to the login screen and the reason is in"
echo "    ~/.local/share/spatiand-session.log       (this session)"
echo "    ~/.local/share/spatiand-session.log.1     (the one before -- where a crash will be)"
