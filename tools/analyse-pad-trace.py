#!/usr/bin/env python3
"""
Read a capture from `capture-pad-trace.py` and say what the pad really does.

The important step is the decimation. Reports arrive at ~250 Hz, but the compositor samples
the *latest* one once per rendered frame — so the deltas the filter sees are frame-to-frame,
not report-to-report, and any threshold reasoned about at report rate is wrong by a factor of
three or four. This replays at the frame rate to ask the question the filter actually faces.

For each contact it prints the speed profile: what the thumb was doing in the settled middle
of the gesture, against what the pad reported in the last frames before it let go. The gap
between those two numbers is the entire problem, and it is the only thing worth tuning against.
"""
import csv, sys
from math import hypot

FRAME_HZ = 72.0


def load(path):
    with open(path) as f:
        return [{k: float(v) for k, v in r.items()} for r in csv.DictReader(f)]


def decimate(rows, hz):
    """Keep the last report inside each frame window — exactly what the render loop sees."""
    if not rows:
        return []
    out, step = [], 1.0 / hz
    edge = rows[0]["t"] + step
    current = rows[0]
    for r in rows:
        if r["t"] <= edge:
            current = r
        else:
            out.append(current)
            while r["t"] > edge:
                edge += step
            current = r
    out.append(current)
    return out


def contacts(frames, pad):
    runs, run = [], []
    for f in frames:
        if f[pad + "touch"]:
            run.append(f)
        elif run:
            runs.append(run)
            run = []
    if run:
        runs.append(run)
    return runs


def speeds(run, pad):
    out = []
    prev = None
    for f in run:
        x, y = f[pad + "x"] / 32767.0, f[pad + "y"] / 32767.0
        if prev is not None:
            out.append(hypot(x - prev[0], y - prev[1]))
        prev = (x, y)
    return out


def pct(values, p):
    if not values:
        return 0.0
    s = sorted(values)
    return s[min(len(s) - 1, int(len(s) * p))]


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/pad-trace.csv"
    pad = sys.argv[2] if len(sys.argv) > 2 else "l"
    tail_n = 8
    frames = decimate(load(path), FRAME_HZ)
    print(f"{len(frames)} frames at {FRAME_HZ:.0f} Hz\n")

    for n, run in enumerate(contacts(frames, pad), 1):
        if len(run) < 6:
            continue
        v = speeds(run, pad)
        body = v[: -tail_n] if len(v) > tail_n else v
        tail = v[-tail_n:]
        clicked = [i for i, f in enumerate(run) if f[pad + "click"]]
        print(f"=== contact {n}: {len(run)} frames" + (f", click held frames {clicked[0]}-{clicked[-1]}" if clicked else ""))
        print(f"  settled speed  median {pct(body, 0.5):.4f}  p90 {pct(body, 0.9):.4f}  max {max(body):.4f}")
        print(f"  last {tail_n} frames  " + " ".join(f"{s:.4f}" for s in tail))
        print(f"  ratio of peak-at-lift to settled p90: {max(tail) / max(pct(body, 0.9), 1e-6):.1f}x")

        # How many trailing frames are faster than anything the settled gesture did. This is
        # the width of the contamination, and therefore how much has to be held back.
        limit = pct(body, 0.9)
        bad = 0
        for s in reversed(v):
            if s > limit * 1.5:
                bad += 1
            elif bad:
                break
        print(f"  trailing frames above 1.5x the settled p90: {bad}")

        if clicked:
            first, last = clicked[0], clicked[-1]
            approach = v[max(0, first - 6) : first]
            release = v[last : min(len(v), last + 8)]
            print(f"  speeds over the 6 frames INTO the click:  " + " ".join(f"{s:.4f}" for s in approach))
            print(f"  speeds over the 8 frames OUT of the click: " + " ".join(f"{s:.4f}" for s in release))
            press = [int(run[i][pad + "pressure"]) for i in range(max(0, first - 4), min(len(run), first + 4))]
            print(f"  pressure around the click edge: {press}")
        nonzero = sum(1 for f in run if f[pad + "pressure"] > 0)
        print(f"  frames with any pressure reported: {nonzero}/{len(run)}\n")


if __name__ == "__main__":
    main()
