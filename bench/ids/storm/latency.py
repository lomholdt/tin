#!/usr/bin/env python3
"""p50 / p99 / max latency per pgbench script from --log files.

    python3 latency.py LOG_PREFIX [FROM_EPOCH TO_EPOCH] [script names...]
"""
import glob
import sys


def pct(v, p):
    return v[min(len(v) - 1, int(round((len(v) - 1) * p)))]


prefix, rest = sys.argv[1], sys.argv[2:]
window = None
if len(rest) >= 2 and rest[0].isdigit() and rest[1].isdigit():
    window, rest = (int(rest[0]), int(rest[1])), rest[2:]
names = rest
by_script = {}
for path in glob.glob(prefix + ".*"):
    for line in open(path):
        f = line.split()
        if len(f) < 6:
            continue  # cut off when the client was stopped
        if window and not window[0] <= int(f[4]) <= window[1]:
            continue
        by_script.setdefault(int(f[3]), []).append(int(f[2]) / 1000)
for s, v in sorted(by_script.items()):
    v.sort()
    name = names[s] if s < len(names) else f"script {s}"
    print(f"{name}: n={len(v)} p50={pct(v, .5):.2f} ms p99={pct(v, .99):.2f} ms max={v[-1]:.1f} ms")
