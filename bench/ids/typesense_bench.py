#!/usr/bin/env python3
"""Load shipments.csv into Typesense and run queries.tsv against it (stdlib only).

    python3 bench/ids/typesense_bench.py DATA_DIR [--load] [--host 127.0.0.1:8108] [--key xyz]

Typesense runs with its out-of-the-box search behaviour (prefix search, up to 2
typos) plus infix search enabled on all three fields so fragments can match.
Latency is client wall time over a keep-alive HTTP connection on localhost;
Typesense's own `search_time_ms` (1 ms resolution) is recorded too.
Writes DATA_DIR/ts_results.tsv (engine, kind, q, target, ms, ids).
"""

import http.client
import json
import sys
import time
import urllib.parse

args = sys.argv[1:]
data_dir = args[0]
host = args[args.index("--host") + 1] if "--host" in args else "127.0.0.1:8108"
key = args[args.index("--key") + 1] if "--key" in args else "xyz"
conn = http.client.HTTPConnection(host, timeout=600)
HEADERS = {"X-TYPESENSE-API-KEY": key}


def call(method, path, body=None, content_type="application/json"):
    headers = dict(HEADERS)
    if body is not None:
        headers["Content-Type"] = content_type
    conn.request(method, path, body=body, headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    if resp.status >= 300:
        raise RuntimeError(f"{method} {path}: {resp.status} {data[:300]!r}")
    return data


if "--load" in args:
    try:
        call("DELETE", "/collections/shipments")
    except RuntimeError:
        pass
    schema = {
        "name": "shipments",
        "fields": [
            {"name": "equipment_no", "type": "string", "infix": True},
            {"name": "booking_no", "type": "string", "infix": True},
            {"name": "bl_no", "type": "string", "infix": True},
        ],
    }
    call("POST", "/collections", json.dumps(schema))
    t0 = time.time()
    batch = []

    def flush():
        body = "\n".join(batch).encode()
        out = call("POST", "/collections/shipments/documents/import?action=create&batch_size=5000", body, "text/plain")
        bad = [l for l in out.decode().splitlines() if '"success":true' not in l]
        if bad:
            raise RuntimeError(f"import failed: {bad[:3]}")
        batch.clear()

    with open(f"{data_dir}/shipments.csv") as f:
        for n, line in enumerate(f, 1):
            i, e, b, l = line.rstrip("\n").split(",")
            batch.append(json.dumps({"id": i, "equipment_no": e, "booking_no": b, "bl_no": l}))
            if len(batch) == 50_000:
                flush()
                if n % 500_000 == 0:
                    print(f"{n:,} docs, {time.time() - t0:.0f}s", file=sys.stderr)
    if batch:
        flush()
    print(f"loaded in {time.time() - t0:.1f}s", file=sys.stderr)

queries = [l.rstrip("\n").split("\t") for l in open(f"{data_dir}/queries.tsv")]
base = {
    "query_by": "equipment_no,booking_no,bl_no",
    "per_page": "10",
    "infix": "fallback,fallback,fallback",
    "include_fields": "id",
}
rows = []
for pass_ in range(2):  # pass 1 warms up; keep pass 2
    rows.clear()
    for kind, q, target in sorted(queries):
        params = dict(base, q=q)
        path = "/collections/shipments/documents/search?" + urllib.parse.urlencode(params)
        t0 = time.perf_counter()
        out = json.loads(call("GET", path))
        ms = (time.perf_counter() - t0) * 1000
        ids = " ".join(h["document"]["id"] for h in out.get("hits", []))
        rows.append(f"typesense\t{kind}\t{q}\t{target}\t{ms:.4f}\t{ids}\t{out.get('search_time_ms', '')}")
with open(f"{data_dir}/ts_results.tsv", "w") as f:
    f.write("\n".join(rows) + "\n")
print(f"{len(rows)} queries -> ts_results.tsv", file=sys.stderr)
