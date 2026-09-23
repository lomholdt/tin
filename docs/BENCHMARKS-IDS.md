# Identifier search benchmark

The target workload (see [ROADMAP](ROADMAP.md#target-use-case)): a search box over ~5M shipping identifiers.

## Data and queries

- **Data**: `bench/ids/generate.py`, seeded, stdlib only.
  - 5,000,000 rows, one per container.
  - Equipment numbers in ISO 6346 format, with valid check digits and ~40 owner codes skewed like a real fleet (`MSKU…`, `CMAU…`, …).
  - 2.7M bookings (9 digits, 1–4 containers each).
  - One bill of lading per booking (carrier SCAC + 9 digits).
- **Queries**: 10,000, 1,000 per kind, each generated from a known *target row*.

| Kind | Example | What a user is doing |
|---|---|---|
| `exact_equipment` / `_lower` | `MSKU6018200`, `msku6018200` | pasting a container number |
| `exact_booking`, `exact_bl` | `514551449`, `MAEU363950549` | pasting a booking or B/L |
| `prefix_equipment`, `prefix_bl` | `MSKU60`, `MAEU3639` | typing (search-as-you-type) |
| `equipment_digits` | `6018200` | container digits without the owner code |
| `equipment_suffix` | `18200` | the last digits |
| `bl_digits` | `363950549` | B/L without the carrier prefix |
| `typo_equipment` | `MSKU6012800` | one wrong / swapped / missing character |

**Metrics** (`bench/ids/evaluate.py`):

- **hit@1 / hit@10**: the target row is first / in the top 10.
  - This is the one that matters for specific queries: exact, digits, typos.
  - A booking has several containers, so the target is one of several equally right rows and hit@1 tops out around 55%.
  - For short prefixes and suffixes, thousands of rows are equally right.
- **precision@10**: the share of returned rows that really match the query in some field (equal, starts with, contains, or within one edit).
- **empty**: the share of queries with no result.

## Baselines (Phase 3)

Environment: PostgreSQL 18.6 and Typesense 30.1 on the same 4-vCPU container.

**Engines**

- **plain**: Postgres with the best-effort "search box" a developer would write (`bench/ids/pg_bench.sql`). It follows Typesense's default semantics:
  1. exact + prefix matches on B-tree `text_pattern_ops` indexes;
  2. only if none: fragments via `LIKE '%x%'` on `pg_trgm` GIN indexes;
  3. only if still none: typos via `pg_trgm` similarity.
- **typesense**: default search behaviour (prefix, up to 2 typos) with `infix` enabled on all three fields (`bench/ids/typesense_bench.py`).
- **tin**: Phase 2, i.e. **exact terms only**, as the starting point. (Phase 4 results are [below](#phase-4-prefix-typo-and-fragment-matching).)

**Timing**

- plain and tin: in-server, `clock_timestamp()` around each call, second of two passes.
- typesense: client wall time over keep-alive HTTP on localhost. Its own `search_time_ms` shows about 0.6 ms of that is HTTP and JSON: server time is under 1 ms for exact and prefix queries, and a mean of 0.45 ms for typos.
- Plain typo search takes seconds per query, so it was measured on a 57-query sample.

| Query kind | Engine | p50 | p99 | hit@1 | hit@10 | precision@10 | empty |
|---|---|---:|---:|---:|---:|---:|---:|
| bl_digits | plain | 15.61 ms | 21.76 ms | 52.6% | 99.8% | 100.0% | 0.0% |
| bl_digits | tin | 0.03 ms | 0.08 ms | 0.0% | 0.0% | 100.0% | 99.8% |
| bl_digits | typesense | 8.08 ms | 14.98 ms | 0.1% | 0.3% | 0.5% | 0.0% |
| equipment_digits | plain | 11.82 ms | 16.77 ms | 36.8% | 76.4% | 100.0% | 0.0% |
| equipment_digits | tin | 0.03 ms | 0.07 ms | 0.0% | 0.0% | nan% | 100.0% |
| equipment_digits | typesense | 1.94 ms | 5.40 ms | 0.0% | 0.0% | 25.5% | 0.0% |
| equipment_suffix | plain | 0.22 ms | 9.68 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | tin | 0.03 ms | 0.06 ms | 0.0% | 0.0% | nan% | 100.0% |
| equipment_suffix | typesense | 0.96 ms | 2.02 ms | 0.0% | 0.0% | 79.8% | 0.0% |
| exact_bl | plain | 0.11 ms | 0.21 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_bl | tin | 0.03 ms | 0.07 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_bl | typesense | 0.73 ms | 1.31 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_booking | plain | 0.11 ms | 0.26 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_booking | tin | 0.03 ms | 0.06 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_booking | typesense | 0.66 ms | 1.14 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_equipment | plain | 0.09 ms | 0.24 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment | tin | 0.03 ms | 0.07 ms | 100.0% | 100.0% | 100.0% | 0.0% |
| exact_equipment | typesense | 0.67 ms | 1.53 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | plain | 0.09 ms | 0.22 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | tin | 0.03 ms | 0.07 ms | 100.0% | 100.0% | 100.0% | 0.0% |
| exact_equipment_lower | typesense | 0.68 ms | 1.83 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| prefix_bl | plain | 0.30 ms | 74.27 ms | 21.9% | 51.6% | 100.0% | 0.0% |
| prefix_bl | tin | 0.03 ms | 0.06 ms | 0.0% | 0.0% | 100.0% | 99.6% |
| prefix_bl | typesense | 0.83 ms | 1.53 ms | 22.7% | 51.8% | 100.0% | 0.0% |
| prefix_equipment | plain | 0.75 ms | 552.65 ms | 22.5% | 35.8% | 100.0% | 0.0% |
| prefix_equipment | tin | 0.03 ms | 0.07 ms | 0.0% | 0.0% | nan% | 100.0% |
| prefix_equipment | typesense | 0.78 ms | 1.25 ms | 23.1% | 30.9% | 100.0% | 0.0% |
| typo_equipment | plain | 1193.44 ms | 7979.04 ms | 33.3% | 82.5% | 27.5% | 0.0% |
| typo_equipment | tin | 0.03 ms | 0.07 ms | 0.0% | 0.0% | 100.0% | 99.7% |
| typo_equipment | typesense | 1.53 ms | 3.54 ms | 47.9% | 83.4% | 82.0% | 0.0% |

**Reading it**

- **Exact numbers**: everyone finds them. Postgres B-trees answer in ~0.1 ms.
- **Prefixes**: plain Postgres has a long tail (p99 **553 ms** for short container prefixes: `LIKE 'MSKU%'` plus ranking exact first sorts ~1M rows). Typesense stays at ~1 ms.
- **Typos**: this is where plain Postgres fails. `pg_trgm` similarity over 5M rows takes **1.2 s median, 8 s p99**. Typesense does it in ~0.5 ms of server time, with the right row in the top 10 83% of the time.
- **Fragments**: this is where Typesense's defaults fail.
  - Container digits without the owner code, or a B/L without its carrier code, almost never find the row (0–0.3% hit@10).
  - Its typo tolerance matches *other* numbers first, and infix search only runs when nothing else matched.
  - Plain Postgres finds them (76% and 99.8%), but at 12–16 ms.
- **tin at Phase 2**: fastest on exact numbers (0.03 ms), and finds nothing else yet.

**The bar for Phases 4–5**

- Typesense-level latency (≤ ~1 ms server time) and hit rate on prefixes and typos.
- Plain-Postgres-level hit rate on fragments, without the 12–16 ms.
- All with transactional consistency, and no second system to keep in sync.

## Phase 4: prefix, typo and fragment matching

Same machine, data, queries and timing as above. **tin** now runs the same search box as plain Postgres (`search_tin` in `bench/ids/pg_bench.sql`), on one index built `WITH (grams = true)`:

1. exact (`q`), then prefix (`q* -q`);
2. only if none: fragments (`*q*`);
3. only if still none: one typo (`q~`), then two (`q~2`, 7+ characters).

| Query kind | Engine | p50 | p99 | hit@1 | hit@10 | precision@10 | empty |
|---|---|---:|---:|---:|---:|---:|---:|
| bl_digits | plain | 15.61 ms | 21.76 ms | 52.6% | 99.8% | 100.0% | 0.0% |
| bl_digits | tin | 0.60 ms | 0.80 ms | 52.6% | 99.8% | 100.0% | 0.0% |
| bl_digits | typesense | 8.08 ms | 14.98 ms | 0.1% | 0.3% | 0.5% | 0.0% |
| equipment_digits | plain | 11.82 ms | 16.77 ms | 36.8% | 76.4% | 100.0% | 0.0% |
| equipment_digits | tin | 0.47 ms | 2.60 ms | 36.8% | 76.4% | 100.0% | 0.0% |
| equipment_digits | typesense | 1.94 ms | 5.40 ms | 0.0% | 0.0% | 25.5% | 0.0% |
| equipment_suffix | plain | 0.22 ms | 9.68 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | tin | 0.34 ms | 1.08 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | typesense | 0.96 ms | 2.02 ms | 0.0% | 0.0% | 79.8% | 0.0% |
| exact_bl | plain | 0.11 ms | 0.21 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_bl | tin | 0.15 ms | 0.44 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_bl | typesense | 0.73 ms | 1.31 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_booking | plain | 0.11 ms | 0.26 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_booking | tin | 0.14 ms | 0.40 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_booking | typesense | 0.66 ms | 1.14 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_equipment | plain | 0.09 ms | 0.24 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment | tin | 0.14 ms | 0.31 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment | typesense | 0.67 ms | 1.53 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | plain | 0.09 ms | 0.22 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | tin | 0.14 ms | 0.29 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | typesense | 0.68 ms | 1.83 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| prefix_bl | plain | 0.30 ms | 74.27 ms | 21.9% | 51.6% | 100.0% | 0.0% |
| prefix_bl | tin | 0.36 ms | 10.07 ms | 21.5% | 51.6% | 100.0% | 0.0% |
| prefix_bl | typesense | 0.83 ms | 1.53 ms | 22.7% | 51.8% | 100.0% | 0.0% |
| prefix_equipment | plain | 0.75 ms | 552.65 ms | 22.5% | 35.8% | 100.0% | 0.0% |
| prefix_equipment | tin | 0.78 ms | 163.23 ms | 22.4% | 35.8% | 100.0% | 0.0% |
| prefix_equipment | typesense | 0.78 ms | 1.25 ms | 23.1% | 30.9% | 100.0% | 0.0% |
| typo_equipment | plain | 1193.44 ms | 7979.04 ms | 33.3% | 82.5% | 27.5% | 0.0% |
| typo_equipment | tin | 2.07 ms | 3.93 ms | 55.5% | 94.7% | 98.8% | 0.0% |
| typo_equipment | typesense | 1.53 ms | 3.54 ms | 47.9% | 83.4% | 82.0% | 0.0% |

**Reading it**

- **Fragments**: the same rows as plain Postgres (99.8% / 76.4% hit@10, 100% precision), **20–26× faster** (0.47–0.60 ms p50, p99 ≤ 2.6 ms). Typesense still misses them.
- **Typos**: the best hit rate of the three (**94.7%** hit@10, 98.8% precision vs Typesense's 83.4% / 82.0%) at 2.1 ms p50 / 3.9 ms p99. That's in-server time, while Typesense's 1.5 / 3.5 ms is client time (about 0.5 ms server time).
  - Likely why: OSA distance counts a swap of neighbours (`…6018200` → `…6012800`) as one edit, and every term within the distance is a candidate.
- **Exact**: 0.14 ms. That is two index scans (exact, then prefix), vs 0.03 ms for the bare exact lookup in Phase 3.
- **Short prefixes**: still the gap. prefix_equipment p99 is **163 ms** (plain 553 ms, Typesense 1.25 ms).
  - A prefix like `MSKU6` matches ~100k distinct numbers, and a bitmap scan must produce all of them before `LIMIT 10` applies.
  - Phase 5's ordered scan streams matches and stops at k.

**Build and size** (5M rows)

| | Build | Size |
|---|---:|---:|
| tin, one index over all three fields, `grams = true` | 91 s | 373 MB |
| tin, exact terms only (Phase 3) | 39 s | 160 MB |
| plain: 3 B-trees + 3 `pg_trgm` GINs | — | 688 MB |

**How we got here**

1. **Patterns in tin-core**: prefix (`term*`) expands the FST range into a tuple-space bitmap; typos (`term~`, `~2`) run an optimal-string-alignment automaton over the FST; fragments intersect n-gram terms and are rechecked. Everything is checked against brute force (`cargo test`) and against sequential scans in SQL.
2. **Planner estimates.** With the stock `contsel` estimate (a flat 0.1%), `WHERE col ==> q LIMIT 10` planned as a *sequential scan*. The planner expected a match every thousand rows, so the seq scan read all 5M rows (**2 s**) whenever q matched one.
   - `==>` now asks the index for its estimate (`tin_restrict`): exact document frequencies for terms, and sums over matching terms for prefixes and typos.
   - Where it is unsure, it guesses low: a low guess costs an index scan, a high one a full table scan. Exact queries dropped to 0.14 ms.
3. **Trigrams → 4-grams.** Identifiers are mostly digits, and there are only 1,000 digit trigrams, so each one is in ~2% of rows. A 7-digit fragment ANDed five lists of ~30k tids per segment (**4.4 ms** in tin-core, 60% of it varint decoding).
   - There are 10,000 digit 4-grams, so the lists are 10× shorter: **0.29 ms**.
   - Each term's last gram is padded with an end marker, so every 3-character substring still starts some gram. A 3-character fragment is an exact prefix search over grams.
   - The index grew 11%.
4. **Dense grams left out.** For a typo like `msku6018200`, the grams `msku` and `sku6` are in a third of all rows. Intersecting only grams within 10× of the rarest took the fragment tier from 1.3 to 0.48 ms; the recheck drops the few extra candidates.
5. **Allocation-free typo automaton**: fixed-size DP rows took the typo tier from 0.81 to 0.63 ms (tin-core).

Profile any of this without Postgres with `cargo run --release -p tin-bench --bin ids -- DATA_DIR`. It builds the same segments, runs the same tiers, and prints per-tier latency and candidate counts.

## Phase 5: ranked search box

tin is now one query, answered by an **ordered index scan**:

```sql
SELECT id FROM shipments
WHERE search_text ~> $1 ORDER BY search_text <~> $1 LIMIT 10;
```

`<~>` is the rank: 0 exact, 1 prefix, 2 fragment, 3 one typo, 4 two typos. The index produces rows tier by tier, streams prefix and typo matches from the dictionary a few terms at a time, and stops when the executor has 10 rows.

- **tin**: typo budget 2 (`tin.search_typos`, the default).
- **tin1**: budget 1 (`SET tin.search_typos = 1`, like Typesense's `num_typos = 1`).

| Query kind | Engine | p50 | p99 | hit@1 | hit@10 | precision@10 | empty |
|---|---|---:|---:|---:|---:|---:|---:|
| bl_digits | plain | 15.61 ms | 21.76 ms | 52.6% | 99.8% | 100.0% | 0.0% |
| bl_digits | tin | 4.11 ms | 5.83 ms | 52.6% | 100.0% | 25.5% | 0.0% |
| bl_digits | tin1 | 0.84 ms | 1.46 ms | 52.6% | 100.0% | 89.5% | 0.0% |
| bl_digits | typesense | 8.08 ms | 14.98 ms | 0.1% | 0.3% | 0.5% | 0.0% |
| equipment_digits | plain | 11.82 ms | 16.77 ms | 36.8% | 76.4% | 100.0% | 0.0% |
| equipment_digits | tin | 3.58 ms | 5.81 ms | 36.8% | 98.4% | 50.2% | 0.0% |
| equipment_digits | tin1 | 0.63 ms | 1.29 ms | 36.8% | 98.4% | 100.0% | 0.0% |
| equipment_digits | typesense | 1.94 ms | 5.40 ms | 0.0% | 0.0% | 25.5% | 0.0% |
| equipment_suffix | plain | 0.22 ms | 9.68 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | tin | 0.09 ms | 0.78 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | tin1 | 0.12 ms | 0.53 ms | 0.1% | 0.1% | 100.0% | 0.0% |
| equipment_suffix | typesense | 0.96 ms | 2.02 ms | 0.0% | 0.0% | 79.8% | 0.0% |
| exact_bl | plain | 0.11 ms | 0.21 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_bl | tin | 3.95 ms | 6.59 ms | 52.8% | 100.0% | 46.1% | 0.0% |
| exact_bl | tin1 | 1.27 ms | 2.00 ms | 52.8% | 100.0% | 97.3% | 0.0% |
| exact_bl | typesense | 0.73 ms | 1.31 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_booking | plain | 0.11 ms | 0.26 ms | 52.8% | 100.0% | 100.0% | 0.0% |
| exact_booking | tin | 4.02 ms | 6.12 ms | 52.8% | 100.0% | 24.0% | 0.0% |
| exact_booking | tin1 | 0.94 ms | 1.96 ms | 52.8% | 100.0% | 87.8% | 0.0% |
| exact_booking | typesense | 0.66 ms | 1.14 ms | 56.6% | 100.0% | 100.0% | 0.0% |
| exact_equipment | plain | 0.09 ms | 0.24 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment | tin | 2.03 ms | 3.89 ms | 100.0% | 100.0% | 10.1% | 0.0% |
| exact_equipment | tin1 | 0.88 ms | 1.65 ms | 100.0% | 100.0% | 91.2% | 0.0% |
| exact_equipment | typesense | 0.67 ms | 1.53 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | plain | 0.09 ms | 0.22 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| exact_equipment_lower | tin | 2.06 ms | 3.82 ms | 100.0% | 100.0% | 10.1% | 0.0% |
| exact_equipment_lower | tin1 | 0.91 ms | 1.80 ms | 100.0% | 100.0% | 91.2% | 0.0% |
| exact_equipment_lower | typesense | 0.68 ms | 1.83 ms | 100.0% | 100.0% | 99.3% | 0.0% |
| prefix_bl | plain | 0.30 ms | 74.27 ms | 21.9% | 51.6% | 100.0% | 0.0% |
| prefix_bl | tin | 0.40 ms | 5.06 ms | 21.2% | 52.7% | 75.2% | 0.0% |
| prefix_bl | tin1 | 0.53 ms | 1.73 ms | 21.2% | 52.7% | 94.2% | 0.0% |
| prefix_bl | typesense | 0.83 ms | 1.53 ms | 22.7% | 51.8% | 100.0% | 0.0% |
| prefix_equipment | plain | 0.75 ms | 552.65 ms | 22.5% | 35.8% | 100.0% | 0.0% |
| prefix_equipment | tin | 0.23 ms | 2.57 ms | 22.2% | 37.1% | 78.4% | 0.0% |
| prefix_equipment | tin1 | 0.27 ms | 1.55 ms | 22.2% | 37.1% | 94.5% | 0.0% |
| prefix_equipment | typesense | 0.78 ms | 1.25 ms | 23.1% | 30.9% | 100.0% | 0.0% |
| typo_equipment | plain | 1193.44 ms | 7979.04 ms | 33.3% | 82.5% | 27.5% | 0.0% |
| typo_equipment | tin | 1.92 ms | 4.32 ms | 57.6% | 100.0% | 23.8% | 0.0% |
| typo_equipment | tin1 | 0.93 ms | 1.83 ms | 57.6% | 100.0% | 99.4% | 0.0% |
| typo_equipment | typesense | 1.53 ms | 3.54 ms | 47.9% | 83.4% | 82.0% | 0.0% |

**Reading it**

- **Every kind, every percentile, under 7 ms (tin) or 2 ms (tin1).** The short-prefix p99 that was 163 ms (Phase 4) and 553 ms (plain) is now 1.5–2.6 ms.
- **Recall is the best of the three.**
  - Fragments: 98.4–100% hit@10. Phase 4's fallback search found 76.4%, because it gave up on typos once any fragment matched.
  - Typos: 100% hit@10, 57.6% hit@1 (Typesense: 83.4% / 47.9%).
- **Why exact is slower than a B-tree (0.9 ms vs 0.1 ms)**: `ORDER BY … LIMIT 10` asks for the ten *best* rows. When one row matches exactly, the other nine come from the typo tiers, and finding them means walking the dictionary. Plain Postgres and Typesense return just the one row.
  - That's also why tin's precision@10 is lower with budget 2: rows two typos away don't count as relevant in this metric.
  - With budget 1 the filler is at most one typo away: 88–99% precision.
- Latency is in-server; Typesense's is client time, about 0.5 ms of which is HTTP.

**How we got here**

1. **Ordered scans** (`amgettuple` + `amcanorderbyop`), and tiers produced lazily (`tin_core::rank::Ranked`). Checked against brute force with deleted rows, pending rows and extra conditions, and against sequential scans in SQL.
2. **Resuming dictionary streams**: prefixes resume from a key range, so a batch of 32 terms costs microseconds.
3. **Typo automaton → lazy DFA**: each DP state is computed once, and after that a transition is a table lookup. That halved the 2-typo walk (12 → 6 ms for 13-character B/Ls).
4. **One segment after `CREATE INDEX`**: the build keeps its segments in memory, up to half of `maintenance_work_mem`, and merges them at the end. Every dictionary walk then touches one FST instead of three, making typo walks 2–2.7× faster. The merge adds 14 s to the 5M build (91 → 103 s).
5. **Planner**: Postgres costs one index path for both the ordered and the bitmap scan. So the `~>` row estimate has to exceed the LIMIT before the planner sees that an ordered scan stops early. The estimate now includes a flat allowance of 20 rows for queries with typo tiers.
