# Roadmap

| # | Phase | Done when | Status |
|---|---|---|---|
| 0 | Core engine, outside Postgres: tokenizer, dictionary, two-level bitmaps, AND/OR/NOT, COUNT | Correct on real data + first speed numbers | ✅ done |
| 1 | Postgres index access method (read-only) | `CREATE INDEX … USING tin (col)` and `WHERE col ==> 'query'` work | next |
| 2 | Writes + deletes | Inserts, updates and VACUUM are correct; survives a crash and a replica | |
| 3 | Background merging | Merges run in a background worker without blocking writes or readers | |
| 4 | Ranking + fast count | `ORDER BY tin.score(ctid) DESC LIMIT 10` and `COUNT(*)` are fast | |
| 5 | Phrases, fuzzy, wildcards | `"san francisco"`, `jeens~1`, `denim*`, regex | |
| 6 | Benchmarks | Same query mix vs GIN, ParadeDB and pg_textsearch on a bigger machine | |

## Phase 1 — Postgres index access method

Built with **pgrx** (Rust), as a new `pg_tin` crate that depends on `tin-core`.

- **Index AM handler**:
  - `ambuild`: parallel heap scan by block range → one segment per range, the same split `Index::build` uses.
  - `aminsert`: a stub that errors until Phase 2.
  - `ambeginscan` / `amrescan` / `amgetbitmap`: `drive()` a cursor and add tids to the `TIDBitmap`.
  - `amcostestimate`: from the terms' document frequencies.
  - `amvacuumcleanup`: a no-op for now.
- **Operator + opclass**: `text ==> text`; the right side is parsed with `Plan::parse`.
- **Storage in index pages**:
  - Segments are written into the index relation's 8 KB pages, logged through Generic WAL.
  - A metapage holds the segment manifest.
  - The segment format already has everything a page layout needs: postings are position-independent offsets, and there are 16 bytes of tail padding.
- **Page directory from the heap**: the build scan records each block's max offset (`PageGetMaxOffsetNumber`).
- **Done when**: `EXPLAIN` shows a Bitmap Index Scan on `tin`, and results match `to_tsvector(...) @@ ...` on the Super User corpus loaded into Postgres.

## Phase 2 — Writes, deletes, crash safety

- **Mutable segment** for new tuples, frozen when full.
  - Mutable segments may overlap immutable ones in block range, so the index-level driver must merge cursors across overlapping segments by group.
- **Liveness bitmap** per segment:
  - `ambulkdelete` clears the bit for each dead tid.
  - Queries AND against it.
  - It must be cleared *before* the heap can reuse a line pointer.
- **Crash + replica tests**: kill -9 during inserts, then check results after recovery; run the same queries on a streaming replica.

## Phase 3 — Merges

- Merge immutable segments in a background worker.
  - Groups that don't overlap move to the new segment unchanged, so their bytes are copied or re-pointed, never re-encoded.
  - Overlapping groups are ORed, with liveness applied.
- Keep a segment manifest with an epoch, so readers pin a manifest and a merge can't free pages a running scan still needs.

## Phase 4 — Ranking + fast count

- **BM25** needs term frequencies and document lengths.
  - Plan: a per-segment document-length column (1-byte quantized, Lucene-style).
  - Per-term frequencies only where tf > 1 (exceptions list).
  - Per-group max-score for block-max WAND top-k.
- `tin.score(ctid)` + a CustomScan that pushes `ORDER BY score LIMIT k` into the index.
- **`COUNT(*)`**:
  - AND page bitmaps with the visibility map's all-visible pages and popcount those directly.
  - Check only the other pages against the heap.

## Phase 5 — Phrases, fuzzy, wildcards

- **Positions**: store them per posting for phrase/span verification (the analyzer already emits positions).
- **Term dictionary**: fuzzy/prefix/regex via FST automata (`fst::automaton::Levenshtein`, `Str::starts_with`, `regex-automata`).

## Performance backlog (from Phase 0 profiling)

- **Sparse lists**: decode with fixed-width bit-packed blocks (PFor-style) instead of varints. Varint decoding is still the top cost for AND over mid-frequency sparse terms.
- **Cursors**: replace the boxed `dyn Cursor` tree with an enum, which removes per-group virtual calls.
- **Materialization into a `Vec<Tid>`**: ~5 ns/tid (disjunction "all tids" 875 µs vs a 273 µs baseline). Revisit only if it shows up in the Postgres path, which emits per page into a `TIDBitmap`.
- **Many-term OR**: a min-heap for 10+ children.
- **Portability**: runtime SIMD dispatch (`is_x86_feature_detected!`) instead of `target-cpu=native`, so one extension binary runs everywhere.
