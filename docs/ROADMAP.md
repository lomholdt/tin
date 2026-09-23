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
| 2 | **Writes** | INSERT / UPDATE / DELETE / VACUUM correct under concurrency and after `kill -9`; index = seqscan | next |
| 3 | **Identifier dataset + baselines** | 5M synthetic container / booking / B/L numbers; B-tree + `pg_trgm` (+ Typesense) latency and recall measured | |
| 4 | **Prefix, typo, fragment matching** | `msku12*`, `msku1243565~1`, and fragment search each match a brute-force reference on 5M IDs | |
| 5 | **Ranked top-k** | `ORDER BY col <=> 'q' LIMIT 20` through an ordered index scan that stops early; search-box p99 < 10 ms at 5M rows | |
| 6 | **Identifier benchmark** | tin vs B-tree + `pg_trgm` vs Typesense: latency, recall@10, build time, size | |
| 7 | Merges + zero-copy reads | Background merging of small segments; segments read straight from shared buffers, no per-backend copy | |
| 8 | Long text | BM25 top-k, phrases, visibility-map `COUNT(*)`, big-corpus benchmark vs GIN / ParadeDB | |

## Phase 1: Postgres index access method ✅

Done: a pgrx 0.18 extension for PostgreSQL 18 (`crates/pg_tin`). See [DESIGN.md](DESIGN.md#postgres-integration-phase-1-cratespg_tin) and [BENCHMARKS.md](BENCHMARKS.md#inside-postgresql-18-phase-1).

- `CREATE INDEX … USING tin`, the `==>` operator, the `text_tin_ops` opclass, and bitmap scans.
- A memory-bounded streaming build: segments are cut at `maintenance_work_mem` and WAL-logged.
- The planner picks the index. 40/40 sampled Super User queries return the same ctids as a sequential scan.
- SQL regression test (`scripts/pg-test.sh`) and a PG18 CI job.

## Phase 2: Writes

- **Pending list** (like GIN's fast update):
  - `aminsert` appends `(tid, analyzed terms)` records to a WAL-logged page chain.
  - Queries evaluate pending records directly.
  - When the list passes `tin.pending_list_limit`, or at VACUUM / `tin_flush()`, it becomes a new immutable segment.
- **Liveness bitmap per segment**, one bit per tuple, in the same tuple-space layout the query engine already uses:
  - `ambulkdelete` clears the bits of dead tids, before the heap can reuse their line pointers.
  - Queries AND each group's result with it.
- **Overlapping segments**: flushed segments cover arbitrary blocks. Bitmap scans don't need ordered output, so each segment is evaluated separately.
- **Cache with generations**: the metapage carries a generation counter plus the pending length. Backends reload only what changed: new segments, liveness after a VACUUM, the new tail of the pending list.
- **Tests**:
  - SQL regression covering insert / update / delete / vacuum / flush.
  - pgbench with concurrent writers and readers.
  - `kill -9` during writes, then index = seqscan after recovery.

## Phase 4: Matching for identifiers

- **Prefix**: `term*` walks the dictionary FST's range `[term, term\xff)`. Unique IDs are singletons stored inline, so the range scan *is* the result list, like a B-tree range scan.
- **Typo**: `term~1` / `term~2` intersect the FST with a Levenshtein automaton (`fst` crate).
- **Fragments**: index character trigrams alongside whole terms, AND the query's trigrams, then verify (the heap recheck confirms the substring).
- **Search-box mode**: one query string expands to exact + prefix + fragment + typo.

## Phase 5: Ranked top-k

- An ordering operator `col <=> 'q'` (distance = match tier) plus `amgettuple` with `amcanorderbyop`, so `ORDER BY … LIMIT 20` becomes an ordered index scan.
- Tiers are produced lazily (exact first, then prefix, …), so the scan stops as soon as the executor has 20 visible rows.

## Performance backlog (from Phase 0 profiling)

- **Sparse lists**: decode with fixed-width bit-packed blocks (PFor-style) instead of varints.
- **Cursors**: replace the boxed `dyn Cursor` tree with an enum, which removes per-group virtual calls.
- **Materialization into a `Vec<Tid>`**: ~5 ns/tid. Revisit only if it shows up in the Postgres path.
- **Many-term OR**: a min-heap for 10+ children.
- **Portability**: runtime SIMD dispatch instead of `target-cpu=native`.
