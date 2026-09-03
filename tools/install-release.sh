#!/usr/bin/env bash
# Install Spatiand from an unpacked release folder.
#
# Everything that can live under $HOME does, for one reason: SteamOS replaces /usr wholesale on
# every update. Only the session registration has to be on /usr, and that is the one thing this
# re-creates on demand afterwards — so an OS update costs you nothing but a click.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HOME/.local/share/spatiand"
BIN="$DEST/spatiand"

say() { printf '\n== %s ==\n' "$1"; }
die() { printf '\n!! %s\n' "$1" >&2; exit 1; }

[ -x "$HERE/spatiand" ] || die "no spatiand binary next to this script"

say "checking this machine"
uname_m="$(uname -m)"
[ "$uname_m" = "x86_64" ] || die "this build is for x86_64, and this machine is $uname_m"
if ! [ -e /dev/dri/card0 ]; then
    die "no graphics device at /dev/dri/card0"
fi
if ! command -v steamosctl >/dev/null && ! command -v steamos-session-select >/dev/null; then
    printf 'note: this does not look like SteamOS. Spatiand may still run, but the\n'
    printf '      "go to spatial mode" button will not know how to switch sessions.\n'
fi

say "installing to $DEST"
install -d "$DEST"
install -m 755 "$HERE/spatiand" "$BIN"
install -m 644 "$HERE/spatiand.svg" "$DEST/spatiand.svg"

say "registering the session (this needs your password once)"
# On /usr, so it needs root, and it is the only part that does. pkexec asks graphically, which
# matters when this was started by double-clicking rather than from a terminal.
LAUNCHER=/usr/local/bin/spatiand-session
SESSION=/usr/share/wayland-sessions/spatiand.desktop
script="$(cat <<EOF
set -e
if command -v steamos-readonly >/dev/null; then steamos-readonly disable || true; fi
cat > $LAUNCHER <<'LAUNCH'
#!/bin/sh
export SPATIAND_BACKEND=drm
export RUST_BACKTRACE=1
ENVFILE="\\\$HOME/.config/spatiand/session.env"
if [ -f "\\\$ENVFILE" ]; then set -a; . "\\\$ENVFILE"; set +a; fi
LOG="\\\$HOME/.local/share/spatiand-session.log"
mkdir -p "\\\$(dirname "\\\$LOG")"
[ -f "\\\$LOG" ] && mv -f "\\\$LOG" "\\\$LOG.1"
exec $BIN >"\\\$LOG" 2>&1
LAUNCH
chmod +x $LAUNCHER
mkdir -p /usr/share/wayland-sessions
# KDE is named alongside Spatiand on purpose: XDG_CURRENT_DESKTOP is what xdg-desktop-portal
# matches a backend against, and a name nothing recognises leaves Flatpak file choosers with
# no implementation to open. See tools/install-session.sh for the long version.
cat > $SESSION <<'ENTRY'
[Desktop Entry]
Name=Spatial Mode
Comment=Spatiand - 3D spatial desktop for XR glasses
Exec=$LAUNCHER
Type=Application
DesktopNames=Spatiand;KDE
X-Spatiand-Revision=2
ENTRY
if command -v steamos-readonly >/dev/null; then steamos-readonly enable || true; fi
EOF
)"
if command -v pkexec >/dev/null; then
    pkexec bash -c "$script" || die "could not register the session"
else
    sudo bash -c "$script" || die "could not register the session"
fi

say "adding a launcher you can click"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
APPS_DIR="$HOME/.local/share/applications"
DESKTOP_DIR="${XDG_DESKTOP_DIR:-$HOME/Desktop}"
install -d "$ICON_DIR" "$APPS_DIR" "$DESKTOP_DIR"
install -m 644 "$DEST/spatiand.svg" "$ICON_DIR/spatiand.svg"
entry="$(cat <<EOF
[Desktop Entry]
Type=Application
Name=Go To Spatiand
Comment=Put on the glasses and enter the spatial desktop
Exec=steamosctl switch-to-desktop-mode spatiand.desktop
Icon=$ICON_DIR/spatiand.svg
Terminal=false
Categories=System;
EOF
)"
printf '%s\n' "$entry" > "$APPS_DIR/spatiand-go.desktop"
printf '%s\n' "$entry" > "$DESKTOP_DIR/Go To Spatiand.desktop"
chmod +x "$DESKTOP_DIR/Go To Spatiand.desktop"

say "done"
cat <<'EOF'
Plug the glasses in first, then either:

  * double-click "Go To Spatiand" on the desktop, or
  * log out and pick "Spatial Mode" at the login screen.

To come back, hold any button on the sidecar's exit row, or from a terminal:
  steamosctl switch-to-desktop-mode plasma.desktop
EOF
