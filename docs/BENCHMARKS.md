# Benchmarks (Phase 0)

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
| Build time | 12.21s (101,799 docs/s, 72.3 MB/s of text) |
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
| Conjunction | COUNT(*) | TIN | 1,043 | 118 µs | 789 µs | 17,591 |
| Conjunction | all tids | TIN | 1,043 | 143 µs | 1.30 ms | 14,771 |
| Conjunction | either | baseline | 1,043 | 107 µs | 2.70 ms | 12,022 |
| Disjunction | COUNT(*) | TIN | 111,977 | 444 µs | 1.02 ms | 8,822 |
| Disjunction | all tids | TIN | 111,977 | 1.20 ms | 3.14 ms | 2,748 |
| Disjunction | either | baseline | 111,977 | 280 µs | 4.58 ms | 5,840 |
| Mixed | COUNT(*) | TIN | 3,220 | 360 µs | 1.26 ms | 9,235 |
| Mixed | all tids | TIN | 3,220 | 390 µs | 1.66 ms | 7,418 |
| Mixed | either | baseline | 3,220 | 364 µs | 4.95 ms | 4,934 |
| Negation | COUNT(*) | TIN | 1,566 | 235 µs | 1.02 ms | 11,724 |
| Negation | all tids | TIN | 1,566 | 229 µs | 1.25 ms | 9,713 |
| Negation | either | baseline | 1,566 | 176 µs | 3.36 ms | 9,503 |


## Reading the numbers

- **Correctness**: 4,000 / 4,000 queries return exactly the baseline's tids.
- **Size**: 19.4% of the text, dictionary included. The TIN post puts a minimal index (no positions or frequencies) at "roughly 20% of the corpus", so we're in the same place.
  - Frequent terms cost 8.8 bits per posting, rare lists 27.6. TIN reports ~25 for rare.
- **Throughput** (4 threads): TIN beats the uncompressed in-RAM baseline on every kind of COUNT query, by 1.2–1.9×.
- **Tail latency**: p99 is better than the baseline everywhere: 3.3–4.5× for COUNT and 1.5–3× when materializing tids. Work per query is bounded by groups × pages, not by the length of the longest posting list.
- **Median latency**:
  - Conjunctions are within ~10% of the baseline, and mixed queries are level with it.
  - Negations are slower at p50 (235 µs vs 176 µs), and so are disjunctions (444 µs vs 280 µs).
  - Materializing all ~112k tids of a disjunction ("all tids") is the weakest spot, about 1.2 ms. That's per-tid emit cost, and it's on the backlog. In Postgres this path feeds a bitmap heap scan, and Phase 4's top-k avoids it entirely.
- **Build**: 12 s for 1.24M docs on 4 threads, ~100k docs/s or 72 MB/s of text. TIN built 85 GB in 8m10s (~173 MB/s) on 8 vCPUs.

**These numbers are not comparable to TIN's published figures.** Their tables measure end-to-end SQL against Postgres on 150M documents with disk I/O. Ours measure the engine alone, in memory, on a corpus about 1/100th the size. The comparison that matters comes in Phase 6, when the same query mix runs through Postgres against GIN and ParadeDB.

## How we got here

Every step was checked against the baseline on the full corpus and against the randomized oracle tests.

| Step | Change | Effect |
|---|---|---|
| 1 | Fixed 291-bit offset bitmaps per page (literal reading of the post) | Never chosen: lost to a plain list for **every** term (100k-doc sample, 13.4 bits/posting overall) |
| 2 | Offset bitmaps sized to each page's tuple count (page directory) | Frequent terms that switched to bitmaps: 6.9 bits/posting (vs 12–15 as lists); index 17.6% of text; conj p50 139 µs |
| 3 | Evaluate a whole group as one packed tuple-space bitmap; copy runs of pages at once | Disjunction COUNT p50 1.01 ms → 0.39 ms, QPS 3.3k → 9.5k |
| 4 | Precomputed group layouts; AND/NOT touch only the masked word range; skip tables on long sparse lists | Within noise (profiling showed the real cost was elsewhere) |
| 5 | callgrind: ~70% of AND time was varint-decoding mid-frequency sparse lists → bias the encoder to bitmaps for lists > 64 | Conj p50 ~165 → 118 µs for +10% index size |
