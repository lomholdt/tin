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

## Phase 6: update storm

Real load is **~700,000 updates a day**. In the worst case it's the same container rebooked again and again (`BOOK123456` → `BOOK89101112` → …). We replay a whole day's worth as fast as the machine allows (`bench/ids/storm/`):

- **Writes:** 700k updates of `booking_no` from 3 clients. 90% hit random rows; 10% hit the same **100 hot rows** (~700 updates each).
- **Reads, at the same time:** 1 client running the search box (`search_tin1`), plus hot-row checks. A check reads a hot row's current booking, then searches for it in the same snapshot; a miss aborts the run.
- **Tables:** each engine on its own table with only its own indexes. `shipments` has the primary key + tin; `shipments_plain` has the primary key + 3 B-trees + 3 `pg_trgm` GINs.
- **Autovacuum:** per-table threshold of 20k dead tuples, no cost delay.

| | tin | plain Postgres |
|---|---:|---:|
| 700k updates took | **168 s (4,170/s)** | 230 s (3,043/s) |
| All indexes (incl. primary key), before → after | 713 → 715 MB (**+0.3%**) | 827 → 937 MB (+13%) |
| Search during the storm, p50 / p99 | **3.5 / 6.8 ms** (quiet: 1.2 / 2.6) | — (typo search takes seconds) |
| Hot-row checks during the storm | 3,068, **0 misses** | — |
| After: tin vs a B-tree on the old and new bookings of 1,100 sampled rows | 1,470 probes, **0 mismatches** | — |

The whole day's load took under 3 minutes, about 500× the real rate. At the real rate (~8 updates/s) tin's write path is idle >99% of the time.

**Reading it**

- **Correct under churn.** Every hot row is found by its latest booking, both during the storm and after it. Old bookings no longer find it: the liveness bitmaps and the pending-list rewrite in VACUUM do their job.
- **Faster writes than plain Postgres.** One tin index costs less per update than six B-tree/GIN indexes, and it answers more kinds of search.
- **Size is stable.** Freed pages go back through the FSM, and the big segment built by `CREATE INDEX` only has bits cleared.
- **Search slows about 3× under a storm, and there are rare stalls.**
  - p50 3.5 ms vs 1.2 ms quiet. Part of that is CPU (4 cores shared by 3 writers, autovacuum and the searcher); part is the pending list, which every search scans.
  - 0.05% of searches took 0.3–1.9 s. They line up with pending-list flushes (every ~5 s at this rate) and 8-way segment merges (~40 s). Both run inline, holding the metapage lock that readers take to refresh their cache. Moving them off the lock is the first item of Phase 7.

**Tuning found on the way**

- **Pending list: scan it cheaply.** Before the fix, every search checked every pending record, at every tier, with an allocating edit-distance function. That took 38 ms per search with 75k pending records. Now the search box is compiled once per query (typo DFAs, cheap checks first) and each record's tier is computed once per scan: 5.9 ms.
- **`tin.pending_list_limit` default: 4 MB → 1 MB.**

  | Limit | Updates/s | Search p50 / p99 during a 200k storm | Index after |
  |---|---:|---:|---:|
  | 4 MB | 3,928 | 3.8 / 9.8 ms | 532 MB |
  | **1 MB** | **4,096** | **3.3 / 6.7 ms** | 532 MB |
  | 256 kB | 3,615 | 2.8 / 6.3 ms | 606 MB |

**Settings for this workload**

- **Autovacuum per table, aggressive.** Every booking change touches the indexed text, so none of these updates can be HOT. Each one leaves a dead heap tuple and a dead index entry until VACUUM runs:
  ```sql
  ALTER TABLE shipments SET (autovacuum_vacuum_scale_factor = 0, autovacuum_vacuum_threshold = 20000,
                             autovacuum_vacuum_insert_scale_factor = 0, autovacuum_vacuum_cost_delay = 0);
  ```
- **`fillfactor`** doesn't help here, for the same reason. It only pays off for updates that leave every indexed column alone, e.g. a status column outside the search text.
- **Keep `search_text` narrow.** Only the identifiers people search for belong in the indexed text; other columns can then change with HOT updates and no tin work at all.

## Phase 7, step 1: flushes and merges off the lock

The same storm (700k updates, hot rows, one searching client), after these changes:

- **Flushes are built without the metapage lock.** A snapshot of the pending list is built into a segment, and then installed under a brief exclusive lock. Records appended meanwhile stay pending. A heavyweight *flush mutex* (like GIN's pending-list cleanup) keeps one flush at a time, and inserters that find it taken just keep appending.
- **Merge factor 8 → 4.** Every search walks every segment's dictionary: 16 segments took quiet search p50 from 1.2 to 4.8 ms.
- **Compaction in VACUUM's cleanup**, i.e. in an autovacuum worker. When the small segments outweigh 25% of the largest, everything is merged into one segment and dead tuples are dropped. It takes its own lock, so flushes carry on meanwhile. It needs 2× the index in `maintenance_work_mem`.

| | Phase 6 | Phase 7 |
|---|---:|---:|
| Updates/s | 4,170 | 3,948 |
| Search p50 / p99 during the storm | 3.5 / 6.8 ms | 3.6 / 8.6 ms |
| **Worst search during the storm** | **1,924 ms** | **93 ms** |
| Searches over 100 ms | 31 | 0 |
| After: tin vs B-tree probes (1,683) | 0 mismatches | 0 mismatches |

**Trade-offs**

- p99 is a little higher: flushes now use CPU alongside everything else, where before they froze everyone.
- A compaction writes the new segment before the old pages are freed, at the next VACUUM. The file therefore holds about 2× the index (here 1.2 GB for ~0.4 GB of segments), and later flushes and compactions reuse that space.
- When a compaction installs, every backend reloads the whole index on its next query: a 0.9–1.2 s search, once per backend. Zero-copy reads (next) remove that.

## Phase 7, step 2: segments in shared memory

Before this, every backend decoded its own copy of every segment on its first query. At 5M rows that was ~400–500 MB and 1–5 s *per connection*, so 50 pooled connections would have held 20+ GB of identical copies.

Now the first backend to need a segment copies it into dynamic shared memory once, and every other backend maps it and parses it in place (`Segment::from_shared`). Only the page directory and docs bitmap (a few percent) are copied per backend. Flushes and compactions **publish** each new segment straight from memory, so no reader ever pays for the copy.

First search on a new connection (5M rows, 9 segments, 474 MB):

| | Before (private copies) | Now (shared) |
|---|---:|---:|
| First connection after a server restart | 5.5 s | 1.1 s (fills shared memory once) |
| Every later connection | 5.5 s | **~20 ms** |
| Memory | 474 MB × connections | 474 MB once |

- **Storm (700k updates) on a warm server:** p50 / p99 4.1 / 10.6 ms, max 114 ms, 0 searches over 300 ms. 1,730 probes, 0 mismatches.
- **Right after a server restart**, the first ~30 s of a storm showed 0.3–7 s searches (the first checkpoint's full-page writes on a cold cache). They're gone on a warm server.
- **Stress test** (8 clients, tiny pending list, VACUUM loop, so segments are created and freed every second): index == seqscan, no errors; shared vs private made no throughput difference.
- **Setting:** `tin.shared_cache_size` (default 1 GB; 0 turns sharing off). The least recently used segments are dropped beyond it.
