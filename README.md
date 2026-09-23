# tin

Our own full-text index for Postgres, reverse-engineered from PlanetScale's TIN design posts ([anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine), [introducing TIN](https://planetscale.com/blog/introducing-tin)).

The core idea: **postings are Postgres ctids stored as two-level bitmaps**, with pages at one level and tuples at the other. That means:

- no document numbering;
- merges that don't renumber;
- results in heap order;
- boolean queries answered with wide bitwise operations.

## Status

**Phases 0–6 are done** (of 0–8): the Rust engine, and a **PostgreSQL 18 index access method** with writes, VACUUM, crash safety, and identifier search: prefix, fragment, and typo-tolerant matching.

```sql
CREATE EXTENSION pg_tin;
CREATE INDEX posts_body_tin ON posts USING tin (body);
SELECT count(*) FROM posts WHERE body ==> 'grub (uefi OR bios) -windows';

CREATE INDEX shipments_tin ON shipments USING tin (search_text) WITH (grams = true);
SELECT * FROM shipments WHERE search_text ==> 'msku60*' LIMIT 10;       -- prefix
SELECT * FROM shipments WHERE search_text ==> '*6018200*' LIMIT 10;     -- fragment
SELECT * FROM shipments WHERE search_text ==> 'msku6012800~' LIMIT 10;  -- one typo

-- search box: exact > prefix > fragment > typo, top 10 via an ordered index scan
SELECT * FROM shipments WHERE search_text ~> 'MSKU60128' ORDER BY search_text <~> 'MSKU60128' LIMIT 10;
```

| | |
|---|---|
| Correctness | 4,000 / 4,000 benchmark queries identical to an independent baseline; randomized differential tests (`cargo test`) |
| Corpus | Super User Stack Exchange, 1.24M posts, 0.88 GB |
| Index size | **19.4% of the text** (TIN post: "roughly 20%" for a minimal index) |
| Speed (4 threads, COUNT) | 11k–22k queries/s, 1.5–2.4× an uncompressed in-RAM baseline; p99 ≈ 1.4 ms or better |
| In PostgreSQL 18 vs GIN | build 35.5 s vs 55.8 s; size 157 MB vs 388 MB; median 2.2–2.5× faster on selective queries; index = seqscan on 40/40 sampled queries |
| 5M shipping IDs vs plain Postgres / Typesense | ranked search box, p99 ≤ 2 ms for every query kind at `tin.search_typos = 1`; best recall of the three (fragments 98–100% hit@10, typos 100%) ([details](docs/BENCHMARKS-IDS.md)) |
| Update storm (a day's 700k updates in 168 s, hot rows) | 0 wrong results; 4,170 updates/s vs 3,043 for plain Postgres; index size stable; search p99 6.8 ms during the storm ([details](docs/BENCHMARKS-IDS.md#phase-6-update-storm)) |

The details, including where we deviate from the posts and why, are in:

- [docs/DESIGN.md](docs/DESIGN.md)
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md) and [docs/BENCHMARKS-IDS.md](docs/BENCHMARKS-IDS.md) (identifier search)
- [docs/ROADMAP.md](docs/ROADMAP.md): aimed at Typesense-style search over ~5M shipping identifiers. Next up: Phase 7, flushes and merges off the lock, zero-copy reads, and a parallel build.

## Layout

```text
crates/tin-core    storage format + query engine (library)
  tid.rs           ctid type
  tokenize.rs      analyzer (UAX #29, case + accent folding)
  bitmap.rs        256-bit page / 512-bit offset bitsets, run iteration
  postings.rs      singleton / sparse / two-level-bitmap encodings + readers
  cursor.rs        group-at-a-time AND / OR / NOT over tuple-space bitmaps
  query.rs         query language -> Plan
  pattern.rs       typo automaton (OSA distance), 4-grams for fragments
  segment.rs       segment build, serialize, search, row estimates
  index.rs         parallel multi-segment build
crates/tin-bench   heap-layout simulator, query generator, baseline, report;
                   bin/ids: identifier search without Postgres
crates/pg_tin      PostgreSQL 18 extension (pgrx 0.18): index AM, ==> operator,
                   writes / VACUUM, row estimates (tin_restrict)
bench/ids/         5M-identifier dataset, plain-PG + Typesense baselines, scoring
scripts/           corpus download + preparation, SQL regression runner
```

## Try it

```sh
cargo test                                   # unit + randomized oracle tests (engine)
PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config sh scripts/pg-test.sh   # extension SQL tests
sh scripts/fetch-superuser.sh                # ~1.3 GB download, needs 7z + python3
cargo run --release -p tin-bench -- data/superuser.docs.txt --queries-per-kind 1000
```

```rust
use tin_core::{Analyzer, Index, Plan, Tid};

let docs = vec![(Tid::new(0, 1), "stretch denim jeans"), (Tid::new(0, 2), "raw denim jacket")];
let index = Index::build(&docs, 1);
let plan = Plan::parse("denim -jacket", &mut Analyzer::new()).unwrap();
assert_eq!(index.search_vec(&plan), vec![Tid::new(0, 1)]);
```

The extension needs PostgreSQL 18 with server headers (`postgresql-server-dev-18`), `libclang`, and `cargo install cargo-pgrx --version 0.18.1 --locked && cargo pgrx init --pg18 $(which pg_config)`. Install it with `cd crates/pg_tin && cargo pgrx install --release`.

Builds use `-C target-cpu=native` (see `.cargo/config.toml`) so the bitmap loops compile to AVX2/AVX-512. To profile under valgrind, which can't run AVX-512, build with `RUSTFLAGS="-C target-cpu=x86-64-v3"` and set `TIN_PROFILE=conjunction` to run just that query kind.
