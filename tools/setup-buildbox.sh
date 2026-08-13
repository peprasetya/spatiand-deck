#!/usr/bin/env bash
# Create the build container.
#
# It must be built against **SteamOS's own packages**, not vanilla Arch. A plain Arch
# container ships a newer glibc (2.44 at time of writing) than SteamOS runs (2.41), and the
# resulting binary references symbol versions the host does not have:
#
#   /usr/lib/libm.so.6: version `GLIBC_2.43' not found
#
# Small test binaries can get away with it — they happen not to touch the newer symbols — so
# this failure appears late and looks like a packaging problem rather than a toolchain one.
#
# Pointing pacman at Valve's mirror with the versioned repo names gives byte-identical
# libraries to the host, so anything that builds here runs there.
set -euo pipefail

BOX="${1:-holo}"
# Matches /etc/pacman.conf on the Deck. The suffix pins the OS branch; read it off the host
# rather than hardcoding, so this keeps working across SteamOS releases.
SUFFIX=$(grep -oE '^\[(core)-[0-9.x]+\]' /etc/pacman.conf | head -1 | sed 's/^\[core-//;s/\]$//')
SUFFIX="${SUFFIX:-3.8.1x}"
echo "== SteamOS repo suffix: $SUFFIX =="

if distrobox list 2>/dev/null | grep -q "^[0-9a-f]* *| *$BOX "; then
    echo "== removing existing '$BOX' =="
    distrobox rm -f "$BOX" >/dev/null 2>&1 || true
fi

echo "== creating '$BOX' =="
distrobox create --name "$BOX" --image docker.io/library/archlinux:latest --yes >/dev/null

cat > /tmp/buildbox-inner.sh <<INNER
set -euo pipefail
sudo tee /etc/pacman.d/mirrorlist >/dev/null <<'EOF'
Server = https://steamdeck-packages.steamos.cloud/archlinux-mirror/\$repo/os/\$arch
EOF

# SigLevel=Never: Valve signs these with keys a vanilla Arch container has no reason to
# trust, and importing their keyring here would be more moving parts than the guarantee is
# worth for a local build box.
sudo tee /etc/pacman.conf >/dev/null <<'EOF'
[options]
Architecture = auto
SigLevel    = Never
LocalFileSigLevel = Optional
EOF
for repo in jupiter holo core extra multilib; do
    printf '[%s-%s]\nInclude = /etc/pacman.d/mirrorlist\n' "\$repo" "$SUFFIX" | sudo tee -a /etc/pacman.conf >/dev/null
done

echo "== syncing against SteamOS packages (this downgrades to match the host) =="
sudo pacman -Syyuu --noconfirm >/dev/null

echo "== installing the toolchain =="
sudo pacman -S --noconfirm --needed \\
    rust base-devel pkgconf git \\
    wayland wayland-protocols libinput libdisplay-info seatd \\
    mesa libglvnd libxkbcommon systemd-libs >/dev/null

echo "== versions =="
rustc --version
pacman -Q glibc wayland libinput mesa
INNER

distrobox enter --name "$BOX" -- bash /tmp/buildbox-inner.sh

echo
echo "== glibc =="
# The binary is built in the container and run on the host, so the host's glibc has to be at
# least the container's. Arch tracks glibc closely and SteamOS does not, so a box built from
# `archlinux:latest` drifts ahead within a few months and then produces binaries that die on
# launch with `version GLIBC_2.xx not found` — which looks like a broken build, not an old OS.
#
# Checked rather than printed. This was a printed warning and it was read exactly as often as
# printed warnings are.
host_glibc=$(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')
box_glibc=$(distrobox enter --name "$BOX" -- bash -c "ldd --version | head -1" 2>/dev/null |
    grep -oE '[0-9]+\.[0-9]+$')
echo "  host      $host_glibc"
echo "  container $box_glibc"
if [ -n "$host_glibc" ] && [ -n "$box_glibc" ] &&
    [ "$(printf '%s\n%s\n' "$host_glibc" "$box_glibc" | sort -V | tail -1)" != "$host_glibc" ]; then
    echo
    echo "  !! The container is AHEAD of the host. Binaries built here will not run on SteamOS."
    echo "     Either rebuild the box from an older archlinux image, or build in a box whose"
    echo "     glibc is <= $host_glibc."
fi
echo
echo "Build with:"
echo "  distrobox enter --name $BOX -- bash -c 'cd ~/spatiand && cargo build --release'"
