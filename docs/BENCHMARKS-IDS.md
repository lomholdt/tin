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
- **tin**: Phase 2, i.e. **exact terms only**, as the starting point.

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
