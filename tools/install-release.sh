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
# Two traps live in this one construct, both paid for.
#
# ESCAPING IS ONE LEVEL, NOT TWO. The heredoc below is unquoted (<<EOF), so its body is
# expanded once when $script is built: a \$ here becomes a $ there. The inner heredoc that
# writes the launcher IS quoted (<<'LAUNCH'), so nothing is expanded again. Anything written
# \\\$ therefore reached the installed launcher as a literal \$, which sh reads as an escaped
# dollar -- so LOG was set to the eight characters "$HOME/..." and the session log went to a
# file named that, in whatever directory SDDM happened to start in.
#
# NO APOSTROPHES anywhere inside this substitution, comments included. Bash scans the
# body of $( ) for quotes before it ever gets to the heredoc, so a lone ' in a word like
# "doesn't" makes it hunt for a closing quote to the end of the file and report an unmatched
# parenthesis on this line. Cost twenty minutes once; the rule is cheaper than the diagnosis.
script="$(cat <<EOF
set -e
if command -v steamos-readonly >/dev/null; then steamos-readonly disable || true; fi
cat > $LAUNCHER <<'LAUNCH'
#!/bin/sh
export SPATIAND_BACKEND=drm
export RUST_BACKTRACE=1
ENVFILE="\$HOME/.config/spatiand/session.env"
if [ -f "\$ENVFILE" ]; then set -a; . "\$ENVFILE"; set +a; fi
LOG="\$HOME/.local/share/spatiand-session.log"
mkdir -p "\$(dirname "\$LOG")"
[ -f "\$LOG" ] && mv -f "\$LOG" "\$LOG.1"
# Deliberately not exec: the session environment has to be taken back afterwards. Spatiand does that
# itself on a normal exit; this is the crash case, where no code of ours runs at all. A
# WAYLAND_DISPLAY left in the systemd user manager -- which outlives the session -- points
# whatever starts next at a socket that is gone, and game mode is what notices: gamescope
# reads it, runs itself nested inside a compositor that has exited, and fails to start.
$BIN >"\$LOG" 2>&1
status=\$?
systemctl --user unset-environment WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE 2>/dev/null
exit \$status
LAUNCH
chmod +x $LAUNCHER
mkdir -p /usr/share/wayland-sessions
# KDE is named alongside Spatiand so xdg-desktop-portal has a backend name it recognises.
# Insurance rather than the whole fix -- see tools/install-session.sh for what was actually
# broken.
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
