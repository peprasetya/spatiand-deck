#!/usr/bin/env python3
"""Copy in every application and paste in every other, across the session and a host.

    tools/clipboard-matrix.py --deck USER@SESSION-MACHINE --host USER@HOST-MACHINE [--only a,b]

Everything goes through the real compositors: keys and clicks are sent to a session's control
socket (see crates/spatiand/src/control.rs), so a window on the host receives them over the
link exactly as it would from the wearer, and every paste is a real application pasting. What
arrived is read back independently of the clipboard -- from a file the application saved, from
a terminal that writes each line it is given to a file, or from a page's title -- so a paste
that failed cannot pass by copying the same thing back.

The session must be running with its control socket (SPATIAND_BACKEND=headless is enough, and
needs no headset), connected to the host. Applications are started with their own throwaway
profiles and state under ~/.cache/cliptest, and closed at the end.
"""

import argparse
import random
import re
import shlex
import string
import subprocess
import sys
import threading
import time
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
PAGE = (
    "<title>cliptest</title><textarea id=t autofocus spellcheck=false "
    "style='width:95vw;height:85vh;font-size:32px' "
    "oninput=\"document.title='T:'+this.value\"></textarea>"
)
DATA_URL = "data:text/html," + urllib.parse.quote(PAGE, safe="=:;',/'")

# Every line a terminal is given goes to a file, and then fills the screen, so a double click
# anywhere selects exactly one copy of it: terminals have no select-all key worth trusting.
TERM_SH = r"""#!/bin/bash
out="$1"; : > "$out"
printf '\033]0;cliptest-term\007'
clear
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$out"
  clear
  for i in $(seq 1 150); do printf '%s\n' "$line"; done
done
"""


class Conversation:
    """Lines to and from one control socket, over ssh."""

    def __init__(self, ssh, socket, bridge):
        self.ssh = ssh
        self.proc = subprocess.Popen(
            ["ssh", "-T", ssh, "python3", "-u", bridge, socket],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.lines = []
        self.cond = threading.Condition()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            with self.cond:
                self.lines.append(line.rstrip("\n"))
                self.cond.notify_all()

    def ask(self, line, ends=("done", "failed", "end", "text", "launched", "closed"), timeout=10):
        with self.cond:
            self.lines.clear()
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()
        got = []
        deadline = time.time() + timeout
        with self.cond:
            while True:
                while self.lines:
                    l = self.lines.pop(0)
                    got.append(l)
                    if l.split(" ", 1)[0] in ends:
                        return got
                left = deadline - time.time()
                if left <= 0:
                    got.append("failed no answer in time")
                    return got
                self.cond.wait(left)


def sh(ssh, command, check=True):
    r = subprocess.run(["ssh", ssh, command], capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"{ssh}: {command}\n{r.stderr}")
    return r.stdout


TRACE = False


def spawn(ssh, unit, env, argv):
    """Start something detached, as a transient user unit that is cleaned up when it exits.

    With --trace, the application's Wayland conversation goes to /tmp/<unit>-wl.log."""
    if TRACE:
        env = {**env, "WAYLAND_DEBUG": "client"}
        argv = ["bash", "-c", f"exec {' '.join(shlex.quote(a) for a in argv)} 2>/tmp/{unit}-wl.log"]
    sets = " ".join(f"--setenv={k}={shlex.quote(v)}" for k, v in env.items())
    sh(ssh, f"systemctl --user stop {unit} 2>/dev/null; systemctl --user reset-failed {unit} 2>/dev/null; "
            f"systemd-run --user --quiet --unit={unit} --collect {sets} {' '.join(shlex.quote(a) for a in argv)}")


def windows(conv):
    """id -> (app, size, flags, title) from a session's or host's `list`."""
    out = {}
    for line in conv.ask("list"):
        parts = line.split(" ", 5)
        if parts[0] != "window":
            continue
        # The session's form has a WIDTHxHEIGHT field; a host title can contain an "x" too.
        if len(parts) >= 6 and re.fullmatch(r"\d+x\d+", parts[3]):
            out[int(parts[1])] = (parts[2], parts[3], parts[4], parts[5])
        else:  # the host's: window <id> <app> <title>
            head = line.split(" ", 3)
            out[int(head[1])] = (head[2], "", "", head[3] if len(head) > 3 else "")
    return out


def wait_new(conv, before, match, timeout=40):
    """The id of a window that was not in `before` and matches."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        now = windows(conv)
        for wid, info in now.items():
            if wid not in before and match(info):
                return wid, info
        time.sleep(1)
    raise RuntimeError(f"no new window turned up: {now}")


@dataclass
class App:
    name: str
    side: str  # "deck" or "host"
    kind: str  # "editor", "terminal", "browser"
    wid: int = -1  # the session's number for its window
    hid: int = -1  # the host's number, for a window on the host
    size: tuple = (800, 600)
    readback: str = ""  # file path for editors and terminals
    notes: list = field(default_factory=list)


class Rig:
    def __init__(self, deck, host):
        self.deck, self.host = deck, host
        # Each machine's own home and runtime directory, asked rather than assumed.
        self.dhome, self.drun = self._where(deck)
        self.hhome, self.hrun = self._where(host)
        self.dwork, self.hwork = f"{self.dhome}/.cache/cliptest", f"{self.hhome}/.cache/cliptest"
        self._put(deck, self.dwork)
        self._put(host, self.hwork)
        self.session = Conversation(deck, f"{self.drun}/spatiand/control", f"{self.dwork}/bridge.py")
        self.hostctl = Conversation(host, f"{self.hrun}/spatiand-host/control", f"{self.hwork}/bridge.py")
        self.log = []
        self.focused = None

    @staticmethod
    def _where(ssh):
        home, uid = subprocess.run(["ssh", ssh, "echo $HOME; id -u"], capture_output=True, text=True,
                                   check=True).stdout.split()
        return home, f"/run/user/{uid}"

    def _put(self, ssh, dest):
        subprocess.run(["ssh", ssh, f"mkdir -p {dest}"], check=True)
        subprocess.run(["scp", "-q", str(HERE / "ctl-bridge.py"), f"{ssh}:{dest}/bridge.py"], check=True)
        subprocess.run(["ssh", ssh, f"cat > {dest}/term.sh && chmod +x {dest}/term.sh"], input=TERM_SH, text=True, check=True)

    def say(self, msg):
        print(msg, flush=True)
        self.log.append(msg)

    # --- starting the applications -------------------------------------------------------

    def start(self, app):
        deck_env = {"XDG_RUNTIME_DIR": self.drun, "WAYLAND_DISPLAY": "wayland-1", "DISPLAY": ":1"}
        host_env = {"XDG_RUNTIME_DIR": self.hrun, "WAYLAND_DISPLAY": "wayland-1", "DISPLAY": ":1"}
        before_s = windows(self.session)
        before_h = windows(self.hostctl)
        unit = f"cliptest-{app.name}"
        if app.name == "deck-kwrite":
            app.readback = f"{self.dwork}/kwrite.txt"
            sh(self.deck, f": > {app.readback}")
            spawn(self.deck, unit, {**deck_env, "QT_QPA_PLATFORM": "wayland"}, ["kwrite", app.readback])
        elif app.name == "deck-konsole":
            app.readback = f"{self.dwork}/konsole.txt"
            spawn(self.deck, unit, {**deck_env, "QT_QPA_PLATFORM": "wayland"},
                  ["konsole", "--separate", "--nofork", "--hide-menubar", "--hide-tabbar", "-e",
                   f"{self.dwork}/term.sh", app.readback])
        elif app.name == "deck-chrome":
            spawn(self.deck, unit, deck_env,
                  ["flatpak", "run", "--command=/app/bin/chrome", "com.google.Chrome",
                   "--ozone-platform=wayland", "--no-first-run", "--no-default-browser-check",
                   f"--user-data-dir={self.dhome}/.var/app/com.google.Chrome/cliptest-profile",
                   "--new-window", DATA_URL])
        elif app.name == "host-chrome":
            spawn(self.host, unit, host_env,
                  ["google-chrome", "--ozone-platform=wayland", "--no-first-run", "--no-default-browser-check",
                   f"--user-data-dir={self.hwork}/chrome-profile", "--new-window", DATA_URL])
        elif app.name == "deck-chrome-x11":
            # The X11 side of the session's clipboard, which Wine games and anything else
            # under Xwayland go through.
            spawn(self.deck, unit, deck_env,
                  ["flatpak", "run", "--command=/app/bin/chrome", "com.google.Chrome",
                   "--ozone-platform=x11", "--no-first-run", "--no-default-browser-check",
                   f"--user-data-dir={self.dhome}/.var/app/com.google.Chrome/cliptest-profile-x11",
                   "--new-window", DATA_URL])
        elif app.name == "host-chrome-x11":
            # The host's X11 bridge -- the path SpatiWorld and Firestorm take -- with an
            # application that can be typed into without anything being said in public.
            spawn(self.host, unit, host_env,
                  ["google-chrome", "--ozone-platform=x11", "--no-first-run", "--no-default-browser-check",
                   f"--user-data-dir={self.hwork}/chrome-profile-x11", "--new-window", DATA_URL])
        elif app.name == "host-qterminal":
            app.readback = f"{self.hwork}/qterminal.txt"
            spawn(self.host, unit, {**host_env, "QT_QPA_PLATFORM": "wayland"},
                  ["qterminal", "-e", f"{self.hwork}/term.sh", app.readback])
        else:
            raise ValueError(app.name)

        if app.side == "host":
            app.hid, _ = wait_new(self.hostctl, before_h, lambda i: True)
            wid, info = wait_new(self.session, before_s, lambda i: i[0].startswith("remote."))
        else:
            wid, info = wait_new(self.session, before_s, lambda i: not i[0].startswith("remote."))
        app.wid = wid
        w, h = (int(v) for v in info[1].split("x")) if info[1] else (800, 600)
        app.size = (w, h)
        self.say(f"  {app.name}: session window {wid} ({info[0]}, {info[1]}), host window {app.hid}")
        time.sleep(4 if "chrome" in app.name else 2)

    def stop(self, app):
        ssh = self.deck if app.side == "deck" else self.host
        sh(ssh, f"systemctl --user stop cliptest-{app.name} 2>/dev/null", check=False)
        # Flatpak runs Chrome outside the unit it was started from, so stopping the unit
        # leaves it open; its test profile is what tells it apart from anybody's real one.
        if "chrome" in app.name:
            # Bracketed, so the pattern does not match the shell that is running it.
            sh(ssh, "pkill -f '[c]liptest-profile|[c]liptest/chrome-profile' 2>/dev/null", check=False)

    # --- what each kind of application does ------------------------------------------------

    def focus(self, app):
        """Give an application the keyboard and let it notice, as a person would.

        Keys sent in the same instant as the focus reach a Qt window before it has made itself
        active, and its shortcuts -- a terminal's Ctrl+Shift+V -- are ignored, while plain typing
        goes through. Nobody pastes within a millisecond of choosing a window."""
        if self.focused == app.wid:
            return
        r = self.session.ask(f"focus {app.wid}")
        if r[-1] != "done":
            raise RuntimeError(f"focusing {app.name}: {r}")
        self.focused = app.wid
        time.sleep(0.5)

    def keys(self, app, *chords):
        self.focus(app)
        r = self.session.ask(f"key {app.wid} {' '.join(chords)}")
        if r[-1] != "done":
            raise RuntimeError(f"keys to {app.name}: {r}")

    def type(self, app, text):
        self.focus(app)
        r = self.session.ask(f"type {app.wid} {text}")
        if r[-1] != "done":
            raise RuntimeError(f"typing in {app.name}: {r}")

    def click_in(self, app, count=1, x=None):
        self.focus(app)
        w, h = app.size if app.size[0] > 0 else (800, 600)
        x = w // 2 if x is None else x
        # One click at a time, a hand's interval apart. Three in one instant carry one
        # timestamp, and a toolkit counts that as one click, not three: nothing is selected.
        for _ in range(count):
            r = self.session.ask(f"click {app.wid} {x} {h // 2} 1")
            if r[-1] != "done":
                raise RuntimeError(f"clicking in {app.name}: {r}")
            time.sleep(0.08)

    def copy(self, app, token):
        if app.kind == "browser":
            # Into the page, as a person would: a new window's keyboard may be in its address
            # bar, and a copy made there is a copy of the wrong field.
            self.click_in(app)
        if app.kind in ("editor", "browser"):
            self.keys(app, "ctrl+a")
            self.type(app, token)
            time.sleep(0.3)
            self.keys(app, "ctrl+a", "ctrl+c")
        elif app.kind == "terminal":
            self.type(app, token)
            self.keys(app, "return")
            time.sleep(0.8)
            # A double click takes the token as one word -- it is letters and digits only, so
            # every terminal agrees where it ends. A triple click would take the line and its
            # newline, and a terminal asked to paste more than one line may stop to ask first:
            # qterminal opens a window of its own when ConfirmMultilinePaste is on, and every
            # key after that goes to it.
            # Near the left edge, where every line starts with the token: the middle of the
            # window is past its end, and a double click on nothing selects a newline.
            self.click_in(app, 2, x=40)
            time.sleep(0.3)
            self.keys(app, "ctrl+shift+c")

    def paste(self, app):
        if app.kind == "editor":
            self.keys(app, "ctrl+a", "ctrl+v")
            time.sleep(0.4)
            self.keys(app, "ctrl+s")
        elif app.kind == "browser":
            self.click_in(app)
            self.keys(app, "ctrl+a", "ctrl+v")
        elif app.kind == "terminal":
            self.keys(app, "ctrl+shift+v")
            time.sleep(0.4)
            self.keys(app, "return")

    def pasted(self, app):
        """What the application now holds, read without touching the clipboard."""
        if app.kind == "browser":
            conv = self.hostctl if app.side == "host" else self.session
            wid = app.hid if app.side == "host" else app.wid
            title = windows(conv).get(wid, ("", "", "", ""))[3]
            # "T:clip-... - Google Chrome", the page's title and the browser's name after it
            if title.startswith("T:"):
                return title[2:].rsplit(" - ", 1)[0]
            return f"<title {title!r}>"
        ssh = self.deck if app.side == "deck" else self.host
        text = sh(ssh, f"cat {app.readback} 2>/dev/null", check=False)
        lines = [l for l in text.splitlines() if l.strip()]
        return lines[-1].strip() if lines else "<empty>"

    def clipboard(self, side):
        conv = self.session if side == "deck" else self.hostctl
        r = conv.ask("clipboard")
        held = next((l[5:] for l in r if l.startswith("held ")), "?")
        text = next((urllib.parse.unquote(l[5:]) for l in r if l.startswith("text ")), None)
        return held, text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--deck", required=True, help="ssh destination of the machine running the session")
    ap.add_argument("--host", required=True, help="ssh destination of the machine running spatiand-host")
    ap.add_argument("--only", help="comma-separated application names")
    ap.add_argument("--keep", action="store_true", help="leave the applications open")
    ap.add_argument("--trace", action="store_true", help="log each application's Wayland traffic")
    args = ap.parse_args()
    global TRACE
    TRACE = args.trace

    apps = [
        App("deck-kwrite", "deck", "editor"),
        App("deck-konsole", "deck", "terminal"),
        App("deck-chrome", "deck", "browser"),
        App("host-chrome", "host", "browser"),
        App("host-qterminal", "host", "terminal"),
        App("deck-chrome-x11", "deck", "browser"),
        App("host-chrome-x11", "host", "browser"),
    ]
    if args.only:
        wanted = args.only.split(",")
        apps = [a for a in apps if a.name in wanted]

    rig = Rig(args.deck, args.host)
    rig.say("starting applications")
    for app in apps:
        rig.start(app)

    results = {}
    run = "".join(random.choice(string.ascii_lowercase) for _ in range(3))
    try:
        for src in apps:
            # Letters and digits only: one word to any terminal's double click.
            token = f"clip{run}{src.name.replace('-', '')}{random.randint(100, 999)}"
            rig.say(f"\ncopy in {src.name}: {token}")
            rig.copy(src, token)
            time.sleep(1.0)
            for side in ("deck", "host"):
                held, text = rig.clipboard(side)
                ok = "ok " if (text or "").strip() == token else "BAD"
                rig.say(f"  {ok} {side} clipboard holds {text!r} (held by {held})")
            for dst in apps:
                if dst is src:
                    continue
                rig.paste(dst)
                time.sleep(1.2)
                # A terminal's line comes with its newline, and pastes as one.
                got = rig.pasted(dst).strip()
                ok = got == token
                results[(src.name, dst.name)] = ok
                rig.say(f"  {'ok ' if ok else 'BAD'} -> {dst.name}: {got!r}")
    finally:
        if not args.keep:
            for app in apps:
                rig.stop(app)

    names = [a.name for a in apps]
    rig.say("\nfrom \\ to        " + " ".join(f"{n[:14]:>14}" for n in names))
    for s in names:
        row = " ".join(
            f"{'-' if s == d else ('ok' if results.get((s, d)) else 'FAIL'):>14}" for d in names
        )
        rig.say(f"{s[:16]:<16} {row}")
    failed = [k for k, v in results.items() if not v]
    rig.say(f"\n{len(results) - len(failed)}/{len(results)} pastes arrived")
    Path("/tmp/clipboard-matrix.log").write_text("\n".join(rig.log) + "\n")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
