#!/usr/bin/env bash
# Put a "Go To Spatiand" launcher on the desktop and in the application menu.
#
# Needs no root: everything lands under $HOME, which also means it survives SteamOS updates.
# Only the session registration itself lives on /usr, and go-to-spatiand.sh re-creates that
# on demand when an update has removed it.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
APPS_DIR="$HOME/.local/share/applications"
DESKTOP_DIR="${XDG_DESKTOP_DIR:-$HOME/Desktop}"
ENTRY_NAME=spatiand-go.desktop

install -d "$ICON_DIR" "$APPS_DIR" "$DESKTOP_DIR"
install -m 644 "$REPO/assets/icons/spatiand.svg" "$ICON_DIR/spatiand.svg"

# Icon by absolute path rather than by theme name: a themed icon needs the cache rebuilt and
# the theme to actually be hicolor, and neither is worth depending on for one launcher.
write_entry() {
    cat > "$1" <<EOF
[Desktop Entry]
Type=Application
Name=Go To Spatiand
GenericName=Spatial Desktop
Comment=Switch to spatial mode - a 3D desktop on XR glasses
Exec=$REPO/tools/go-to-spatiand.sh
Icon=$ICON_DIR/spatiand.svg
Terminal=false
Categories=System;
Keywords=XR;AR;VR;spatial;glasses;XREAL;
StartupNotify=false
EOF
    chmod +x "$1"
}

write_entry "$APPS_DIR/$ENTRY_NAME"
write_entry "$DESKTOP_DIR/$ENTRY_NAME"

# KDE will not run a desktop file it does not trust, and shows it as plain text instead.
# Marking it executable is not enough on Plasma 6; the metadata flag is what silences the
# "untrusted" prompt.
if command -v kwriteconfig6 >/dev/null; then
    gio set "$DESKTOP_DIR/$ENTRY_NAME" metadata::trusted true 2>/dev/null || true
fi

command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" 2>/dev/null || true

echo "Installed:"
echo "  desktop icon : $DESKTOP_DIR/$ENTRY_NAME"
echo "  menu entry   : $APPS_DIR/$ENTRY_NAME"
echo "  icon         : $ICON_DIR/spatiand.svg"
echo
echo "If Plasma shows it as an untrusted file, right-click it once and allow launching."
