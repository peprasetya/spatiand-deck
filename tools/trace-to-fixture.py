#!/usr/bin/env python3
"""
Turn a captured pad trace into a Rust test fixture.

The scroll filter's thresholds are claims about how the hardware behaves, and the only honest
way to hold them is against a recording of the hardware behaving. This emits the decimated
frames of chosen contacts as a Rust array so `spatiand-input::scroll` can replay them.

Usage: trace-to-fixture.py pad-trace.csv l 2,3,4 > fixture.rs
"""
import csv, sys
from math import hypot

FRAME_HZ = 72.0


def decimate(rows, hz):
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


def main():
    path, pad, want = sys.argv[1], sys.argv[2], {int(n) for n in sys.argv[3].split(",")}
    with open(path) as f:
        rows = [{k: float(v) for k, v in r.items()} for r in csv.DictReader(f)]
    frames = decimate(rows, FRAME_HZ)

    runs, run = [], []
    for f in frames:
        if f[pad + "touch"]:
            run.append(f)
        elif run:
            runs.append(run)
            run = []
    if run:
        runs.append(run)

    for n, run in enumerate(runs, 1):
        if n not in want:
            continue
        # Emitted whole and in order. An earlier version of this script thinned long dwells to
        # keep the file short, which spliced a discontinuity into the middle of the recording —
        # a jump the hardware never made, arriving as a scroll the filter then had to be
        # blamed for. A recording that has been edited is not a recording.
        keep = run
        print(f"/// Contact {n} from the capture: {len(run)} frames, verbatim.")
        print(f"const CONTACT_{n}: &[(f32, f32, bool, u16)] = &[")
        for f in keep:
            print(
                f"    ({f[pad + 'x'] / 32767.0:.5f}, {f[pad + 'y'] / 32767.0:.5f}, "
                f"{'true' if f[pad + 'click'] else 'false'}, {int(f[pad + 'pressure'])}),"
            )
        print("];")
        print()


if __name__ == "__main__":
    main()
