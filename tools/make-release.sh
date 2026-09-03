#!/usr/bin/env bash
# Build a release folder somebody can download, unpack and click.
#
# The shape is deliberate. What arrives is a folder with a binary, an installer you double
# click, and a text file — because the audience for this is somebody with a Steam Deck and a
# pair of glasses, not somebody with a Rust toolchain. Anything that requires reading build
# instructions before seeing the thing work has lost most of the people it was for.
#
# Run this on the Deck, inside the build container. It writes to ./dist.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
# The version is the date, because that is what it honestly is: a snapshot of a project that
# has no releases behind it. A semantic version would promise a stability nobody has earned.
VERSION="${SPATIAND_VERSION:-$(date +%Y-%m-%d)}"
NAME="spatiand-$VERSION"
OUT="$REPO/dist/$NAME"

echo "== building =="
cd "$REPO"
# `cargo` is not on the container's PATH when the shell is not a login one, and the failure
# reads as "no Rust installed" rather than "wrong PATH".
CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
[ -x "$CARGO" ] || { echo "no cargo found; set CARGO=/path/to/cargo" >&2; exit 1; }
"$CARGO" build --release

echo "== packaging $NAME =="
rm -rf "$OUT"
install -d "$OUT"
install -m 755 target/release/spatiand "$OUT/spatiand"
install -m 644 assets/icons/spatiand.svg "$OUT/spatiand.svg"
install -m 755 tools/install-release.sh "$OUT/install.sh"

# The clickable half. `Terminal=true` on purpose: the install says what it is doing and what it
# could not do, and a graphical launcher that fails silently is worse than no launcher.
cat > "$OUT/Install Spatiand.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Install Spatiand
Comment=Install the spatial desktop and add it to the login session list
Exec=bash -c 'cd "\$(dirname "%k")" && ./install.sh; echo; read -rp "Press enter to close."'
Icon=$OUT/spatiand.svg
Terminal=true
Categories=System;
EOF
chmod +x "$OUT/Install Spatiand.desktop"

cp "$REPO/docs/install.md" "$OUT/README.txt"

echo "== tarball =="
cd "$REPO/dist"
tar czf "$NAME.tar.gz" "$NAME"
echo
echo "wrote dist/$NAME.tar.gz"
ls -la "$NAME.tar.gz"
