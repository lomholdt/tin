# Benchmarks

## Setup

- **Corpus**: the Super User Stack Exchange dump (`superuser.com.7z`, archive.org, April 2024). Each post's title and body, HTML stripped (`scripts/fetch-superuser.sh`).
  - This is the same *kind* of data as TIN's headline benchmark, but about 1/100th of its size: 1.24M docs / 0.88 GB, against their 150M docs / 85 GB.
- **ctids**: the documents are laid out on simulated 8 KB Postgres heap pages (tuple headers, line pointers, TOAST above ~2 KB; see `crates/tin-bench/src/heap.rs`). That gives realistic ctid density: 13.2 tuples per page.
- **Machine**: the cloud container this was developed in. 4 vCPUs with AVX-512, 15 GB RAM, index fully in memory. No Postgres is involved yet.
- **Queries**: 4,000 seeded synthetic queries, split evenly across conjunction, disjunction, mixed and negation.
  - Terms come from the 5,520 terms that appear in ≥ 1,000 tuples; 30% of picks are from the top 500.
- **Correctness**: every query's full tid list is compared with an independent baseline index built from the same text.
- **Baseline**: uncompressed sorted `u64` posting arrays with merge / galloping set operations, held in RAM.
  - It is 5× larger than our postings and pays no decoding cost, so it's a tough bar for latency.
  - It is **not** ParadeDB or GIN; those comparisons are Phase 6.

Reproduce:

```sh
sh scripts/fetch-superuser.sh
cargo run --release -p tin-bench -- data/superuser.docs.txt --queries-per-kind 1000
```

## Results

### Corpus

| | |
|---|---|
| File | `superuser.docs.txt` |
| Documents | 1,243,027 |
| Text | 882.5 MB |
| Simulated heap | 94,223 pages of 8 KB, 13.2 tuples/page, 58,001 values TOASTed |
| Machine | 4 hardware threads; 4 used |

### Build

| | |
|---|---|
| Segments | 4 (built in parallel over disjoint block ranges) |
| Build time | 10.13s (122,655 docs/s, 87.1 MB/s of text) |
| Postings (tuple, term) pairs | 87,850,352 |
| Index size | 171.1 MB = dictionary 33.1 MB + postings 137.8 MB + page directory 188.4 KB |
| Index / text | 19.4% |
| Bits per posting, all terms | 12.55 |

#### Bits per posting by term frequency

High = term in >= 1% of tuples, Medium = >= 0.01%, Rare = fewer.

| Class | Encoding | Terms | Postings | Bytes | Bits/posting |
|---|---|---:|---:|---:|---:|
| High | two-level bitmap | 4,285 | 64,180,149 | 70.4 MB | 8.78 |
| Medium | sparse gap list | 41,621 | 2,924,268 | 8.4 MB | 23.04 |
| Medium | two-level bitmap | 24,099 | 16,493,230 | 49.3 MB | 23.92 |
| Rare | singleton (inline in dictionary) | 1,460,307 | 1,460,307 | 0.0 KB | 0.00 |
| Rare | sparse gap list | 548,176 | 2,792,384 | 9.6 MB | 27.59 |
| Rare | two-level bitmap | 2 | 14 | 0.0 KB | 9.71 |
| **High total** | | 4,285 | 64,180,149 | 70.4 MB | **8.78** |
| **Medium total** | | 65,720 | 19,417,498 | 57.7 MB | **23.79** |
| **Rare total** | | 2,008,485 | 4,252,705 | 9.6 MB | **18.11** |

### Queries

1000 queries per kind (4000 total), seed 42. Terms drawn from the 5,520 terms in >= 1,000 tuples (30% from the 500 most frequent).

- **Conjunction**: e.g. `shows suffer within`, `scanner lost`
- **Disjunction**: e.g. `socket OR some OR changes`, `troubleshooter OR got`
- **Mixed**: e.g. `(8 OR ext2) limitation`, `(reinstall OR that's) btrfs`
- **Negation**: e.g. `hyper router's -way`, `users searched -inside`

Correctness: **4000 / 4000 queries return exactly the baseline's tids**.

### Speed

Latency is single-threaded, one query at a time; QPS runs the whole set on 4 threads. Baseline = uncompressed sorted `u64` posting arrays with merge/galloping set ops (702.8 MB of postings vs TIN's 137.8 MB) — a generous textbook inverted index held fully in RAM.

| Query kind | Mode | Engine | Avg matches | p50 | p99 | QPS |
|---|---|---|---:|---:|---:|---:|
| Conjunction | COUNT(*) | TIN | 1,043 | 107 µs | 964 µs | 21,830 |
| Conjunction | all tids | TIN | 1,043 | 109 µs | 1.55 ms | 16,600 |
| Conjunction | either | baseline | 1,043 | 98 µs | 2.62 ms | 12,348 |
| Disjunction | COUNT(*) | TIN | 111,977 | 289 µs | 681 µs | 10,980 |
| Disjunction | all tids | TIN | 111,977 | 875 µs | 3.17 ms | 3,191 |
| Disjunction | either | baseline | 111,977 | 273 µs | 4.13 ms | 6,691 |
| Mixed | COUNT(*) | TIN | 3,220 | 197 µs | 1.10 ms | 11,703 |
| Mixed | all tids | TIN | 3,220 | 265 µs | 1.90 ms | 8,616 |
| Mixed | either | baseline | 3,220 | 331 µs | 4.72 ms | 4,987 |
| Negation | COUNT(*) | TIN | 1,566 | 180 µs | 1.41 ms | 14,119 |
| Negation | all tids | TIN | 1,566 | 185 µs | 1.56 ms | 11,526 |
| Negation | either | baseline | 1,566 | 171 µs | 3.34 ms | 9,566 |

## Reading the numbers

- **Correctness**: 4,000 / 4,000 queries return exactly the baseline's tids.
- **Size**: 19.4% of the text, dictionary included. The TIN post puts a minimal index (no positions or frequencies) at "roughly 20% of the corpus", so we're in the same place.
  - Frequent terms cost 8.8 bits per posting, rare lists 27.6. TIN reports ~25 for rare.
- **Throughput** (4 threads): TIN beats the uncompressed in-RAM baseline on every query kind and mode except "all tids" for disjunctions. For COUNT it's 1.5–2.4× faster.
- **Tail latency**: p99 is better than the baseline everywhere: 2.4–6× for COUNT and 1.3–2.5× when materializing tids. Work per query is bounded by groups × pages, not by the length of the longest posting list.
- **Median latency**:
  - Conjunctions and negations are within ~10% of the baseline.
  - Mixed queries are 40% faster than it.
  - Disjunction COUNT is level (289 µs vs 273 µs).
  - The one clear loss: materializing all ~112k tids of a disjunction ("all tids", 875 µs vs 273 µs). That is per-tid emit cost into a Rust `Vec` (~5 ns/tid). In Postgres, results go page by page into a `TIDBitmap` instead, so Phase 1 measures the path that actually matters.
- **Build**: 12 s for 1.24M docs on 4 threads, ~100k docs/s or 72 MB/s of text. TIN built 85 GB in 8m10s (~173 MB/s) on 8 vCPUs.

**These numbers are not comparable to TIN's published figures.** Their tables measure end-to-end SQL against Postgres on 150M documents with disk I/O. Ours measure the engine alone, in memory, on a corpus about 1/100th the size. The comparison that matters comes in Phase 6, when the same query mix runs through Postgres against GIN and ParadeDB.

## Inside PostgreSQL 18 (Phase 1)

The same corpus loaded into a real table, `posts (id bigserial, body text)`, with a tin index and, for comparison, a GIN index on `to_tsvector('simple', body)`. The `simple` config has no stemming and no stop words, like tin.

- **Setup**: PostgreSQL 18.6, same 4-vCPU container, `shared_buffers = 1GB`.
- **Real heap**: 100,232 pages, 12.4 tuples per page. The Phase 0 simulator predicted 13.2, so it was close.

### Build and size

| | tin | GIN (`simple`) |
|---|---:|---:|
| `CREATE INDEX` (`maintenance_work_mem = 1GB`) | **35.5 s** | 55.8 s |
| Index size | **157 MB** (2 segments) | 388 MB |
| Index / heap | 20% | 50% |

The first tin build took 3m05s. Its memory-limit check walked every term after every heap page; making that an O(1) counter took it to 35.5 s.

### Correctness

- 40 of the 1,000 queries (10 per kind) were run twice: through the index, and as a parallel sequential scan that calls `tin_match` on every row.
- **40/40 returned identical ctid sets**, compared as the md5 of the ordered ctid list; 898,798 matching rows in total.

### Query latency

- **Queries**: 400 of the Phase 0 queries, 100 per kind, each run as `SELECT count(*) … WHERE body ==> $1` and, for GIN, the equivalent `plainto_tsquery` expression.
- **Measurement**: timed inside the server with `clock_timestamp()`, one backend, no parallel workers, second of two passes (caches warm).
- **`work_mem = 256MB`**: with the default 4 MB, big results make the bitmap lossy, and both indexes then spend their time re-tokenizing rows on rechecks.

| Query kind | Engine | Avg matches | p50 | p99 | Queries/s (1 core) |
|---|---|---:|---:|---:|---:|
| Conjunction | **tin** | 710 | **0.29 ms** | **13.6 ms** | **847** |
| Conjunction | GIN | 707 | 0.74 ms | 17.4 ms | 483 |
| Disjunction | **tin** | 101,994 | **49.3 ms** | **235 ms** | **17** |
| Disjunction | GIN | 103,999 | 54.7 ms | 262 ms | 15 |
| Mixed | **tin** | 3,957 | **0.90 ms** | **71.1 ms** | **217** |
| Mixed | GIN | 3,940 | 1.96 ms | 82.4 ms | 146 |
| Negation | **tin** | 1,131 | **0.37 ms** | **25.0 ms** | **562** |
| Negation | GIN | 1,163 | 0.94 ms | 32.4 ms | 302 |

- **tin wins on every kind**: 2.2–2.5× on median latency for selective queries, and 1.1–1.9× on throughput.
- **Disjunctions** are dominated by the heap: `count(*)` over ~100k rows visits ~100k tuples either way. Phase 4's visibility-map count avoids those visits.
- **Match counts differ slightly** (for example 710 vs 707), because Postgres's text parser and UAX #29 split some tokens differently (`fox's`, `3.14`, URLs).
- **First query in a new connection: 257 ms**, the time to load the 157 MB index into that backend's cache. After that, 2.7 ms for the same query. Reading straight from shared buffers (Phase 3) removes this.

## How we got here

Every step was checked against the baseline on the full corpus and against the randomized oracle tests.

| Step | Change | Effect |
|---|---|---|
| 1 | Fixed 291-bit offset bitmaps per page (literal reading of the post) | Never chosen: lost to a plain list for **every** term (100k-doc sample, 13.4 bits/posting overall) |
| 2 | Offset bitmaps sized to each page's tuple count (page directory) | Frequent terms that switched to bitmaps: 6.9 bits/posting (vs 12–15 as lists); index 17.6% of text; conj p50 139 µs |
| 3 | Evaluate a whole group as one packed tuple-space bitmap; copy runs of pages at once | Disjunction COUNT p50 1.01 ms → 0.39 ms, QPS 3.3k → 9.5k |
| 4 | Precomputed group layouts; AND/NOT touch only the masked word range; skip tables on long sparse lists | Within noise (profiling showed the real cost was elsewhere) |
| 5 | callgrind: ~70% of AND time was varint-decoding mid-frequency sparse lists → bias the encoder to bitmaps for lists > 64 | Conj p50 ~165 → 118 µs for +10% index size |
| 6 | callgrind: OR/NOT spent ~70% in the run finder, because mid-frequency terms have many 1-page runs. Replaced with word-parallel run boundaries (start/end bitmasks, two `tzcnt` per run), a whole-run fast path when every page of the term is wanted, and a single 8-byte load for copies of ≤ 57 bits | Negation p50 235 → 180 µs; disjunction COUNT p50 444 → 289 µs, QPS 8.8k → 11.0k; mixed 360 → 197 µs |
| 7 | Emit tids page by page (extract each page's bits in one go) instead of locating the page for every bit; reserve output per group | Disjunction "all tids" p50 1.20 → 0.88 ms |

## Phase 8: phrases, proximity and scoring

The 1.24M Super User posts (842 MB of text) in PostgreSQL 18, tin vs Postgres full-text search: a GIN index on `to_tsvector('simple', body)`, queried with `@@` and ranked with `ts_rank`. Neither index stores word positions: GIN over a tsvector expression rechecks phrases against the row, as tin does. Setup and scripts are in [bench/text](../bench/text/README.md).

**Queries:** 250, 50 per kind, sampled from real posts so each has matches, e.g. `"monitor process and"`, `folder NEAR/5 and`. They include very common words, which is where a positionless index hurts most. Each runs as `count(*)` (every match) and as the best 10 by score. The second of two passes is kept, on one core (no parallel workers).

| Kind | Matches (avg) | tin `count` p50 / p90 | Postgres `count` p50 / p90 | tin top 10 p50 / p90 | Postgres top 10 p50 / p90 |
|---|---:|---:|---:|---:|---:|
| `a b c` (AND) | 5,111 | 10 / 85 ms | 10 / 71 ms | 32 / 392 ms | 250 / 2,763 ms |
| `"a b c"` | 32 | 38 / 264 ms | 294 / 2,071 ms | 24 / 160 ms | 303 / 2,026 ms |
| `"a b"` | 16,129 | 325 / 1,863 ms | 2,344 / 23,968 ms | 239 / 2,435 ms | 2,555 / 27,521 ms |
| `a THEN/3 b` | 18,071 | 145 / 1,808 ms | 1,043 / 22,246 ms | 100 / 2,249 ms | 1,203 / 25,217 ms |
| `a NEAR/5 b` | 32,973 | 344 / 1,975 ms | 2,789 / 25,828 ms | 457 / 3,742 ms | 3,584 / 38,069 ms |

- **tin is 7–12.6× faster** (p50) on everything that needs positions or a score. On plain AND counts, both engines answer from the index alone and tie.
- **Common words are still slow for both:** hundreds of milliseconds to seconds. `"a b"` with two frequent words means re-reading every post that has both. A positional index would fix that, at roughly 3× the index size (see [DESIGN](DESIGN.md#query-language)). WAND-style top-k would fix the ranked case.
- **Where the time goes:** re-reading a row is almost all splitting it into words. Two rounds of speed-ups:
  - **Recheck only the query's words.** The first version kept the positions of every word of each candidate row. Keeping only the query's words (`span::Wanted`) made positional queries 1.7–2× faster (`"a b"` p50: 994 → 538 ms).
  - **A faster word splitter** (see below): another 1.7–2.3× (`"a b"` p50: 538 → 325 ms), and the index builds in 10.7 s instead of 13.9 s.
- **Exact:** on 28 queries, the index returned the same rows as a sequential scan with the same operator: 58,676 rows, 0 differences.
- **tin and Postgres don't always agree on counts,** even for AND (0.9% average difference on 2-word phrases). The cause is tokenization, not matching: Postgres's parser indexes `kernel-devel` as a whole word *and* its parts, so for Postgres "yum install kernel" is not a phrase in "yum install kernel-devel" (tin: 40 rows, Postgres: 5). It also splits `you're` and keeps URLs whole. tin follows Unicode word boundaries.
- **The Super User index now builds in 13.9 s** with 4 threads (parallel `CREATE INDEX`), vs 35.5 s serial in Phase 1 on a faster host. It is 154 MB (GIN: 388 MB).

The Postgres numbers are from the first run. The tin numbers are from a later run of the same queries on the same server, after both speed-ups.

### Parallel query

The table above is one core. `tin_match` and `tin_score` are parallel-safe, and planner support functions now tell Postgres what a recheck costs (see [DESIGN](DESIGN.md#planner-costs)). So Postgres uses parallel workers for tin queries that read many rows. The same 250 queries with `max_parallel_workers_per_gather = 3` on 4 cores (`bench/text/parallel.py`, plain statements, because PL/pgSQL `SELECT INTO` never runs in parallel):

| Kind | `count` p50 / p90, 1 core | … 3 workers | top 10 p50 / p90, 1 core | … 3 workers |
|---|---:|---:|---:|---:|
| `a b c` (AND) | 10 / 85 ms | 6 / 48 ms | 32 / 392 ms | 32 / 161 ms |
| `"a b c"` | 38 / 264 ms | 30 / 194 ms | 24 / 160 ms | 26 / 105 ms |
| `"a b"` | 325 / 1,863 ms | 184 / 1,023 ms | 239 / 2,435 ms | 117 / 1,187 ms |
| `a THEN/3 b` | 145 / 1,808 ms | 97 / 968 ms | 100 / 2,249 ms | 64 / 1,113 ms |
| `a NEAR/5 b` | 344 / 1,975 ms | 199 / 1,729 ms | 457 / 3,742 ms | 199 / 1,989 ms |

- **The slowest query** went from 5.8 s to 2.6 s. A full scan for `"and the"` went from 5.3 s to 1.8 s (a parallel sequential scan); top 10 by score went from 7.8 s to 2.4 s.
- **Before the cost fix, Postgres never parallelized** these. It thought `==>` cost 10 operator calls a row whatever the text length, and parallel workers only split CPU cost. It even preferred a serial index scan (5.8 s) over a parallel sequential scan (1.7 s) for common words, because Postgres charges a plain index scan nothing for rechecking its own condition.
- **Small queries don't change:** rare phrases stay single index scans (~20–30 ms), and identifier plans are unchanged.
- The Postgres full-text numbers above are one core; with workers it gains too, so compare like with like.

### The word splitter

Splitting text into words (Unicode UAX #29, via `unicode-segmentation`) ran at ~130 MB/s, about 5 µs of every 710-byte post. `tokenize::for_each_word` now gives exactly the same words 5× faster:

| On the 1.24M posts (884 MB) | Speed |
|---|---:|
| `unicode_word_indices` (before) | 125 MB/s |
| `for_each_word` | 628 MB/s |
| … its ASCII part alone (`ascii_words`) | 1,156 MB/s |
| recheck of `"and the"` per post, splitting + matching | 8.2 → 3.9 µs |

- **ASCII, branch-light:** 94% of posts are pure ASCII. For those, UAX #29 comes down to a few local rules (letters, digits and `_` join; `.` `'` `:` join letters; `.` `'` `,` `;` join digits). Each 64-byte block becomes bitmasks (with AVX2 when the CPU has it), and word edges fall out of shifts and trailing-zero counts. A byte-at-a-time loop was 5× slower because of branch mispredictions.
- **Non-ASCII text** is cut into pieces where ASCII whitespace meets an ASCII non-space byte. UAX #29 always breaks there, and no rule looks across. Only the pieces around non-ASCII bytes go through `unicode-segmentation`.
- **Exact:** a randomized test compares the result with `unicode_word_indices` on 200,000 strings mixing ASCII, marks, joiners, quotes and spaces, and it caught two cut rules that were almost right. On all 1.24M posts: 0 differences. Existing indexes stay valid.
- **Pre-filter:** a recheck skips, before lower-casing and hashing, any ASCII word whose length and first letter match no query word.
