# tin

Our own full-text index for Postgres, reverse-engineered from PlanetScale's TIN design posts ([anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine), [introducing TIN](https://planetscale.com/blog/introducing-tin)).

The core idea: **postings are Postgres ctids stored as two-level bitmaps**, with pages at one level and tuples at the other. That means:

- no document numbering;
- merges that don't renumber;
- results in heap order;
- boolean queries answered with wide bitwise operations.

## Status

**Phase 0 of 7 is done**: a standalone Rust engine (no Postgres yet).

| | |
|---|---|
| Correctness | 4,000 / 4,000 benchmark queries identical to an independent baseline; randomized differential tests (`cargo test`) |
| Corpus | Super User Stack Exchange, 1.24M posts, 0.88 GB |
| Index size | **19.4% of the text** (TIN post: "roughly 20%" for a minimal index) |
| Speed (4 threads, COUNT) | 11k–22k queries/s, 1.5–2.4× an uncompressed in-RAM baseline; p99 ≈ 1.4 ms or better |

The details, including where we deviate from the posts and why, are in:

- [docs/DESIGN.md](docs/DESIGN.md)
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md)
- [docs/ROADMAP.md](docs/ROADMAP.md) (next: Phase 1, a pgrx index access method)

## Layout

```text
crates/tin-core    storage format + query engine (library)
  tid.rs           ctid type
  tokenize.rs      analyzer (UAX #29, case + accent folding)
  bitmap.rs        256-bit page / 512-bit offset bitsets, run iteration
  postings.rs      singleton / sparse / two-level-bitmap encodings + readers
  cursor.rs        group-at-a-time AND / OR / NOT over tuple-space bitmaps
  query.rs         query language -> Plan
  segment.rs       segment build, serialize, search
  index.rs         parallel multi-segment build
crates/tin-bench   heap-layout simulator, query generator, baseline, report
scripts/           corpus download + preparation
```

## Try it

```sh
cargo test                                   # unit + randomized oracle tests
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

Builds use `-C target-cpu=native` (see `.cargo/config.toml`) so the bitmap loops compile to AVX2/AVX-512. To profile under valgrind, which can't run AVX-512, build with `RUSTFLAGS="-C target-cpu=x86-64-v3"` and set `TIN_PROFILE=conjunction` to run just that query kind.
