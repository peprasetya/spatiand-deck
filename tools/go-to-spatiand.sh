#!/usr/bin/env bash
# Enter spatial mode, from the desktop, with one click.
#
# The interesting part is self-healing. SteamOS is atomic: an update replaces /usr wholesale,
# which silently removes both the session entry and its launcher. Everything else survives -
# the binary, the calibration, the udev rules - so the only thing lost is registration, and
# the only symptom is that spatial mode quietly stops being offered.
#
# Rather than making that the user's problem, this checks on every launch and re-registers
# when needed. Re-registering needs root, so pkexec is used for a graphical password prompt
# instead of failing with a terminal error nobody will see.
set -uo pipefail

REPO="${SPATIAND_REPO:-$HOME/spatiand}"
SESSION=/usr/share/wayland-sessions/spatiand.desktop
LAUNCHER=/usr/local/bin/spatiand-session
BIN="$REPO/target/release/spatiand"

fail() {
    if command -v kdialog >/dev/null; then
        kdialog --title "Spatiand" --error "$1"
    else
        echo "$1" >&2
    fi
    exit 1
}

[[ -x "$BIN" ]] || fail "Spatiand is not built yet.\n\nExpected: $BIN\n\nBuild it with:\ndistrobox enter --name holo -- bash -c 'cd ~/spatiand && cargo build --release'"

# A binary that cannot resolve its libraries dies before it prints anything, and since this
# switches to a session with no desktop underneath, "dies before printing anything" looks
# exactly like a black screen that never comes back. The usual cause is a build container
# whose glibc has drifted ahead of the host's: it compiles and links perfectly and then fails
# at the first instruction.
#
# `ldd` resolves the same way the loader does, so this catches it before the screen goes away.
missing=$(ldd "$BIN" 2>&1 | grep -E 'not found' | head -4)
if [[ -n "$missing" ]]; then
    fail "Spatiand cannot run on this system.\n\n$missing\n\nThis usually means it was built \
against newer libraries than SteamOS has. Rebuild it in a container whose glibc is no newer \
than this machine's ($(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')):\n\n\
distrobox enter --name holo -- bash -c 'cd ~/spatiand && cargo build --release'"
fi

if [[ ! -f "$SESSION" || ! -x "$LAUNCHER" ]]; then
    if command -v kdialog >/dev/null; then
        kdialog --title "Spatiand" --msgbox \
"Spatial mode needs to be re-registered.

A SteamOS update replaces the system partition, which removes the session
entry. Your settings, calibration and the app itself are untouched.

You will be asked for your password."
    fi
    pkexec env SPATIAND_USER="$USER" bash "$REPO/tools/install-session.sh" \
        || fail "Could not register spatial mode.\n\nRun this in a terminal to see why:\nsudo $REPO/tools/install-session.sh"
    [[ -f "$SESSION" ]] || fail "Registration reported success but $SESSION is still missing."
fi

# steamosctl owns the SDDM autologin drop-in; writing that file by hand does not work,
# because SteamOS regenerates it and silently ignores hand-edits.
steamosctl set-default-desktop-session spatiand.desktop >/dev/null 2>&1
steamosctl switch-to-desktop-mode spatiand.desktop >/dev/null 2>&1 \
    || fail "Could not switch sessions.\n\nsteamosctl refused. Try again, or reboot."
