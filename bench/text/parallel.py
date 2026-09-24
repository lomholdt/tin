#!/usr/bin/env python3
"""The benchmark's 250 queries run as plain SQL statements, so Postgres may
use parallel workers (PL/pgSQL's SELECT INTO never does, which is why
pg_bench.sql measures one core). Standard library only; needs psql.

    python3 parallel.py WORKERS [psql args...]    # e.g. 3 -h /tmp -p 5418

Runs every query twice (count(*), then best 10 by tin_score) and keeps the
second pass. Prints p50 / p90 per kind; writes parallel_<WORKERS>.tsv.
"""
import re
import statistics
import subprocess
import sys

workers = int(sys.argv[1])
psql = ["psql", "-X", "-q", *sys.argv[2:]]
queries = [l.rstrip("\n").split("\t")[:2] for l in open("queries.tsv")]

lines = ["SET client_min_messages = warning;", f"SET max_parallel_workers_per_gather = {workers};", "\\timing on"]
for _ in range(2):
    for kind, q in queries:
        lit = "'" + q.replace("'", "''") + "'"
        lines.append(f"SELECT count(*) FROM posts WHERE body ==> {lit};")
        lines.append(
            f"SELECT count(*) FROM (SELECT id FROM posts WHERE body ==> {lit} "
            f"ORDER BY tin_score('posts_body_tin'::regclass, body, {lit}) DESC LIMIT 10) x;"
        )
out = subprocess.run(psql, input="\n".join(lines), capture_output=True, text=True, check=True).stdout
times = [float(m) for m in re.findall(r"^Time: ([0-9.]+) ms", out, re.M)]
assert len(times) == 4 * len(queries), (len(times), out[-2000:])
second = times[2 * len(queries):]

by = {}
with open(f"parallel_{workers}.tsv", "w") as f:
    for i, (kind, q) in enumerate(queries):
        for j, mode in enumerate(["count", "top10"]):
            ms = second[2 * i + j]
            by.setdefault((mode, kind), []).append(ms)
            f.write(f"{mode}\t{kind}\t{i + 1}\t{ms}\n")


def pct(v, p):
    v = sorted(v)
    x = (len(v) - 1) * p
    lo = int(x)
    hi = min(lo + 1, len(v) - 1)
    return v[lo] + (v[hi] - v[lo]) * (x - lo)


print(f"workers {workers}")
for (mode, kind), v in sorted(by.items()):
    print(f"{mode:6} {kind:8} p50 {pct(v, 0.5):8.1f} ms  p90 {pct(v, 0.9):8.1f} ms  max {max(v):8.1f} ms")
