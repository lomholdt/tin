#!/usr/bin/env python3
"""Score search results: latency, whether the intended row was found, and
whether the returned rows actually match the query (stdlib only).

    python3 bench/ids/evaluate.py DATA_DIR results.tsv [more.tsv ...] > report.md

Result lines: engine, kind, q, target, ms, space-separated ids [, extra...].

* hit@1 / hit@10  the row the query was generated from is first / in the top 10.
                  Meaningful for specific queries (exact, typo, full digits); for
                  short prefixes and suffixes many rows are equally right.
* precision@10    share of returned rows that really match the query's intent
                  in some field: equal / starts with / contains / within one edit.
* empty           share of queries with no result at all.
"""

import sys
from collections import defaultdict


def osa_distance_le1(a: str, b: str) -> bool:
    """Optimal-string-alignment distance <= 1 (a substitution, insertion,
    deletion, or adjacent transposition)."""
    if a == b:
        return True
    la, lb = len(a), len(b)
    if abs(la - lb) > 1:
        return False
    i = 0
    while i < min(la, lb) and a[i] == b[i]:
        i += 1
    if la == lb:
        if a[i + 1:] == b[i + 1:]:
            return True  # substitution
        return i + 1 < la and a[i] == b[i + 1] and a[i + 1] == b[i] and a[i + 2:] == b[i + 2:]
    if la > lb:
        return a[i + 1:] == b[i:]
    return a[i:] == b[i + 1:]


def relevant(kind: str, q: str, fields) -> bool:
    q = q.upper()
    if kind.startswith("exact"):
        return any(f == q for f in fields)
    if kind.startswith("prefix"):
        return any(f.startswith(q) for f in fields)
    if kind.startswith("typo"):
        return any(osa_distance_le1(f, q) for f in fields)
    return any(q in f for f in fields)  # digits / suffix fragments


def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(round((len(v) - 1) * p)))]


def main(data_dir, paths):
    rows = []
    wanted = set()
    for p in paths:
        for line in open(p):
            parts = line.rstrip("\n").split("\t")
            engine, kind, q, target, ms, ids = parts[:6]
            ids = [int(x) for x in ids.split()] if ids.strip() else []
            rows.append((engine, kind, q, int(target), float(ms), ids))
            wanted.update(ids)
            wanted.add(int(target))
    data = {}
    with open(f"{data_dir}/shipments.csv") as f:
        for line in f:
            i, e, b, l = line.rstrip("\n").split(",")
            i = int(i)
            if i in wanted:
                data[i] = (e, b, l)

    agg = defaultdict(lambda: defaultdict(list))
    for engine, kind, q, target, ms, ids in rows:
        a = agg[(kind, engine)]
        a["ms"].append(ms)
        a["hit1"].append(1.0 if ids[:1] == [target] else 0.0)
        a["hit10"].append(1.0 if target in ids[:10] else 0.0)
        a["empty"].append(0.0 if ids else 1.0)
        if ids:
            a["prec"].append(sum(relevant(kind, q, data[i]) for i in ids[:10]) / len(ids[:10]))

    engines = sorted({e for (_, e) in agg})
    print("| Query kind | Engine | p50 | p99 | hit@1 | hit@10 | precision@10 | empty |")
    print("|---|---|---:|---:|---:|---:|---:|---:|")
    for kind in sorted({k for (k, _) in agg}):
        for engine in engines:
            a = agg.get((kind, engine))
            if not a:
                continue
            avg = lambda k: sum(a[k]) / len(a[k]) if a[k] else float("nan")
            print(
                f"| {kind} | {engine} | {pct(a['ms'], 0.5):.2f} ms | {pct(a['ms'], 0.99):.2f} ms | "
                f"{avg('hit1'):.1%} | {avg('hit10'):.1%} | {avg('prec'):.1%} | {avg('empty'):.1%} |"
            )


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2:])
