# Roadmap

## Target use case

Typesense-style search over **~5 million shipping identifiers**, inside PostgreSQL:

- **Equipment (container) numbers**: ISO 6346, 11 characters, e.g. `MSKU1234565`.
- **Booking numbers**: e.g. `234567890`.
- **Bill of lading numbers**: e.g. `MAEU234567890`.

A search box should find a row from:

- the exact number;
- a prefix while typing (`MSKU12…`);
- a fragment (`1234565`, or the last digits);
- a number with a typo (`MSKU1243565`).

Results come back ranked (exact, then prefix, then fragment, then typo), top 20, in a few milliseconds, and stay correct while rows are inserted and deleted.

Long-text ranking (BM25, phrases) and TIN-style big-corpus benchmarks still matter, but they come after this.

## Phases

| # | Phase | Done when | Status |
|---|---|---|---|
| 0 | Core engine (Rust): tokenizer, dictionary, two-level bitmaps, AND/OR/NOT, COUNT | Correct on real data + first speed numbers | ✅ done |
| 1 | PostgreSQL 18 index access method (read-only) | `CREATE INDEX … USING tin`; `WHERE col ==> 'q'`; index = seqscan | ✅ done |
| 2 | **Writes** | INSERT / UPDATE / DELETE / VACUUM correct under concurrency and after `kill -9`; index = seqscan | ✅ done |
| 3 | **Identifier dataset + baselines** | 5M synthetic container / booking / B/L numbers; B-tree + `pg_trgm` (+ Typesense) latency and recall measured | ✅ done ([results](BENCHMARKS-IDS.md)) |
| 4 | **Prefix, typo, fragment matching** | `msku12*`, `msku1243565~`, and fragment search each match a brute-force reference on 5M IDs | ✅ done ([results](BENCHMARKS-IDS.md#phase-4-prefix-typo-and-fragment-matching)) |
| 5 | **Ranked top-k** | `ORDER BY col <~> 'q' LIMIT 10` through an ordered index scan that stops early; search-box p99 < 10 ms at 5M rows | ✅ done ([results](BENCHMARKS-IDS.md#phase-5-ranked-search-box)) |
| 6 | **Identifier benchmark** | tin vs B-tree + `pg_trgm` vs Typesense: latency, recall@10, build time, size; **update storm** (700k updates/day, incl. the same rows over and over) | ✅ done ([results](BENCHMARKS-IDS.md#phase-6-update-storm)) |
| 7 | Flush/merge off the lock, zero-copy reads, parallel build | No search stalls during flushes/merges; no ~1 s index copy on a backend's first query; `CREATE INDEX` on all cores | next |
| 8 | Long text | BM25 top-k, phrases, visibility-map `COUNT(*)`, big-corpus benchmark vs GIN / ParadeDB | |

## Phase 1: Postgres index access method ✅

Done: a pgrx 0.18 extension for PostgreSQL 18 (`crates/pg_tin`). See [DESIGN.md](DESIGN.md#postgres-integration-cratespg_tin) and [BENCHMARKS.md](BENCHMARKS.md#inside-postgresql-18-phase-1).

- `CREATE INDEX … USING tin`, the `==>` operator, the `text_tin_ops` opclass, and bitmap scans.
- A memory-bounded streaming build: segments are cut at `maintenance_work_mem` and WAL-logged.
- The planner picks the index. 40/40 sampled Super User queries return the same ctids as a sequential scan.
- SQL regression test (`scripts/pg-test.sh`) and a PG18 CI job.

## Phase 2: Writes ✅

Design details are in [DESIGN.md](DESIGN.md#writes).

- ✅ **Pending list** for inserts, flushed into immutable segments past `tin.pending_list_limit`, at VACUUM, or on `tin_flush()`.
- ✅ **Liveness bitmap per segment**, cleared by `ambulkdelete` before line pointers can be reused.
- ✅ **Tiered merges**, so the segment count stays logarithmic.
- ✅ **Page chains + FSM reuse**, so the index size levels off under churn.
- ✅ **Generation-based cache**: backends reload only what changed.

**Tests**
- **SQL regression**: insert / update / delete / rollback, flush, auto-flush, forced line-pointer reuse, REINDEX, TRUNCATE.
- **Stress** (`crates/pg_tin/tests/stress`): about 195k pgbench transactions in 60 s from 8 clients, plus a VACUUM loop.
  - 0 failures; index = seqscan on every check.
  - Segment count 3–20; index size stable at 15 MB.
- **Crash**: `kill -9` of the postmaster mid-write, while merges were running; index = seqscan after recovery, after VACUUM, and after more writes.

**Still open (Phase 7)**: inserts are serialized per index, merges run inline, and segments over 64 MB only compact on REINDEX.

## Phase 4: Matching for identifiers ✅

Design in [DESIGN.md](DESIGN.md#identifier-patterns-phase-4); numbers in [BENCHMARKS-IDS.md](BENCHMARKS-IDS.md#phase-4-prefix-typo-and-fragment-matching).

- ✅ **Prefix** `term*`: the FST range, expanded into one tuple-space bitmap.
- ✅ **Typo** `term~` / `term~2`: an optimal-string-alignment automaton over the FST (a swap of neighbours is one edit).
- ✅ **Fragment** `*frag*`: 4-grams (`WITH (grams = true)`), ANDed and rechecked; 3-character fragments are exact.
- ✅ **Planner estimates from the index** (`tin_restrict`), so `==> q LIMIT k` uses the index.
- ✅ **Search box** in SQL (`search_tin`): exact → prefix → fragment → typo.

At 5M rows, in Postgres:
- fragments 0.5–0.6 ms p50 (plain Postgres: 12–16 ms), same rows;
- typos 2.1 ms p50 / 3.9 ms p99, 94.7% hit@10 (Typesense 83.4%, plain Postgres 1.2 s);
- exact 0.14 ms.

**Carried into Phase 5**: short prefixes (p99 163 ms). A bitmap scan must produce every match before `LIMIT` applies, and `MSKU6*` matches ~100k numbers.

## Phase 5: Ranked top-k ✅

- ✅ `~>` (search-box match) and `<~>` (rank: 0 exact … 4 two typos). `ORDER BY col <~> q LIMIT k` is an ordered index scan (`amgettuple`, `amcanorderbyop`) that produces tiers lazily and stops at k.
- ✅ Prefixes and typos stream from the dictionary 32 terms at a time.
- ✅ Typo automaton as a lazy DFA; one merged segment after `CREATE INDEX`.
- ✅ `tin.search_typos` (0–2), like Typesense's `num_typos`.

At 5M rows: every query kind has p99 < 7 ms (budget 2) or < 2 ms (budget 1), with the best recall of the three engines. Exact lookups (0.9 ms at budget 1) stay slower than a B-tree's 0.1 ms, because a top-10 must find the nine next-best rows too.

## Phase 6: Update storm ✅

700k updates (a day's load), 10% of them on the same 100 rows, replayed in 168 s while searching ([results](BENCHMARKS-IDS.md#phase-6-update-storm)):

- 0 wrong results during and after; the index size was stable (+0.3%).
- 4,170 updates/s vs 3,043 for plain Postgres's six indexes.
- Search p50 / p99 3.5 / 6.8 ms during the storm.
- Fixed on the way: pending-list scans (38 → 5.9 ms at 75k records), and `tin.pending_list_limit` now defaults to 1 MB.
- Documented: per-table autovacuum; why `fillfactor` / HOT don't apply to booking changes.

**Found**: 0.05% of searches stall 0.3–1.9 s while an insert flushes or merges inline under the metapage lock. That leads Phase 7.

## Phase 7: Flush/merge off the lock, zero-copy reads, parallel build

1. **Build flush and merge output outside the metapage lock**, and take the lock only to swap it in. Liveness bits cleared meanwhile have to be carried over to the new segment, since VACUUM may run in between.
2. **Zero-copy reads**: segments read in place from shared buffers, so there's no ~1 s per-backend copy on the first query.
3. **Parallel `CREATE INDEX`**: build segments on all cores, then merge (103 s → ~35 s at 5M rows).

## Performance backlog (from Phase 0 profiling)

- **Sparse lists**: decode with fixed-width bit-packed blocks (PFor-style) instead of varints. Varint decoding was 60% of a trigram fragment AND before the switch to 4-grams.
- **Cursors**: replace the boxed `dyn Cursor` tree with an enum, which removes per-group virtual calls.
- **Materialization into a `Vec<Tid>`**: ~5 ns/tid. Revisit only if it shows up in the Postgres path.
- **Many-term OR**: a min-heap for 10+ children.
- **Portability**: runtime SIMD dispatch instead of `target-cpu=native`.
