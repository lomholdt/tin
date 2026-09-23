# Design

We are rebuilding PlanetScale's TIN from two public blog posts:

- [Anatomy of a Postgres search engine](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine) — how inverted indexes work and what embedding one in Postgres requires.
- [Introducing TIN](https://planetscale.com/blog/introducing-tin) — TIN's own design points and benchmarks.

Neither post publishes a file format, so this document keeps three things apart:

- ✅ **Known**: stated in the posts.
- 🧩 **Inferred**: our reading of what the posts imply.
- 🔧 **Ours**: where we measured something and deliberately did it differently.

## Ubiquitous language

| Term | Meaning |
|---|---|
| **Tid** | A Postgres heap tuple id (`ctid`): `(block, offset)`, offset 1-based, at most 291 on 8 KB pages. The posting *is* the tid; there are no document numbers. |
| **Group** | 256 consecutive heap blocks. The unit of query evaluation. |
| **Page bitmap** (`PageBits`) | 256 bits: which pages of a group contain a term. |
| **Offset bitmap** | Which line pointers on one page contain a term. |
| **Page directory** | Per segment: each block's line-pointer count, i.e. the width of every offset bitmap on that page. |
| **Tuple space** (`GroupSpace`) | Every line pointer of a group's pages laid end to end. A query's result for one group is one bitmap over this space. |
| **Segment** | A self-contained, immutable index over a block range: term dictionary + postings + page directory. |
| **Encoding** | How a term's postings are stored: singleton, sparse list, or groups (two-level bitmap). |

## Storage

### Postings are ctids

- ✅ Postings are ctids, not document ids. Merging segments therefore never renumbers anything: bitmaps change owner.
- ✅ Results come back in heap order, so Postgres fetches rows sequentially.
- Ours: `Tid::key() = block << 9 | (offset - 1)` gives a dense, order-preserving `u64`.

### Three encodings (per term, smallest wins, biased to bitmaps)

| Encoding | Used for | Layout | Cost on Super User |
|---|---|---|---|
| Singleton | df = 1 | ✅ tid stored inline in the dictionary value, no postings bytes | 0 bits |
| Sparse | rare / some mid-frequency | 🧩 varint gaps between keys; lists over 64 keys get a skip table | ~23–28 bits/posting |
| Groups | frequent / mid-frequency | ✅ two-level bitmap (see below) | ~9 bits/posting frequent, ~24 mid |

TIN says rare terms "approach 25 bits per posting". A gap-coded list naturally lands there (a gap of ~2²⁰ needs a 3-byte varint), so we infer that TIN's rare-term format is also a list. We measured 27.6 bits.

### The two-level bitmap

For each group containing the term:

```text
group      := group_delta body_len body          -- body_len lets seek() skip unread groups
body       := u8(n_pages - 1)
              ( u8 page * n_pages | 32-byte PageBits )   -- list if <= 31 pages
              bitstream                                  -- width(page) bits per page, ascending
```

- ✅ Page-level bitmaps are 256 bits.
- 🔧 **Offset bitmaps are exactly as wide as the page.** TIN describes offset bitmaps fitting a 512-bit register. We first stored fixed 291-bit bitmaps and *measured that they never won*: Stack Exchange pages hold ~13 tuples, so a fixed bitmap spends 291 bits to mark ~1–14 tuples. It lost to a plain list for every single term. TIN's claim that frequent terms "approach 1 bit per posting" is only reachable if the width tracks the page's real tuple count. So each segment stores a **page directory** (2 bytes per heap page; 188 KB for the whole 0.9 GB corpus), and a term pays `width(page)` bits per page it touches, with no per-page header. Frequent (≥1% of tuples) terms fell from ~12 to ~7–9 bits/posting.
- Byte-list vs bitmap page sets follow the same smallest-wins rule.

### Encoding choice

The encoder builds both a sparse and a groups encoding and keeps the smaller, **but lets groups be up to 1.5× larger** (`GROUPS_BIAS_*`). Profiling showed that AND/NOT over mid-frequency sparse lists spent ~70% of its time decoding varints. A bitmap lets AND discard pages from the 32-byte page set without touching postings. The trade was +10% index size for about −25% conjunction p50. Lists of ≤ 64 postings (rare terms) gain nothing from pruning and simply take the smaller encoding.

### Segment file

```text
"TIN\0" u32 version  u32 first_block  u32 end_block  u64 docs  u64 terms  u64 postings
u64 n_widths  u16 width * n_widths          -- page directory
u64 fst_len   fst bytes                     -- term -> u64 (2-bit tag | 62-bit offset or tid key)
u64 post_len  postings bytes  + 16 zero bytes   -- tail padding for unaligned 128-bit loads
```

The dictionary is an FST (the `fst` crate, as in Tantivy/Lucene). The dictionary is 19% of our index and is the structure fuzzy, prefix and regex matching need in Phase 5.

## Query execution

Queries run **group at a time** over a tree of cursors (`Term`, `And`, `Or`, `AndNot`). Each group is evaluated in two levels, mirroring the storage.

1. **`seek(group)` + `pages()`**
   - ✅ AND leapfrogs on group ids.
   - It then ANDs the 256-bit page bitmaps of its children and skips the whole group if the result is empty, without reading any offset data.
   - OR takes the minimum group and ORs the page bitmaps.
2. **`or_into(mask, space, out)`**
   - Each child ORs its exact matches, for the pages in `mask` only, into a bitmap over the group's **tuple space**.
   - AND/NOT combine scratch bitmaps word by word, restricted to the words the mask touches.
   - `COUNT(*)` is a popcount over the result. Materializing tids walks the set bits.

- 🔧 **Tuple space instead of one register per page.** ✅ TIN describes per-page 512-bit offset bitmaps processed with AVX-512. With ~13 tuples per page that is ~97% padding. We pack all of a group's pages end to end (~3,400 bits ≈ 53 words ≈ 7 AVX-512 registers), so one vector op covers ~39 pages. Because a term's consecutive pages are also contiguous on disk, a dense term fills the tuple space with a handful of straight bit copies (`copy_bits_or`), one per run of pages rather than one per page. Switching to this took disjunction COUNT p50 from 1.01 ms to 0.39 ms.
- 🔧 **Finding runs word-parallel.** Run boundaries come from two masks, `starts = b & !(b << 1)` and `ends = b & !(b >> 1)` (with carries across words), so each run costs two trailing-zero counts.
  - When a term's every page is wanted (OR, single terms), each run is one copy and the bitstream cursor just advances.
  - When only a subset is wanted (AND/NOT masks), the reader walks the term's pages to track bitstream positions and coalesces adjacent wanted pages.
  - Copies of ≤ 57 bits (a single ~13-tuple page) are one unaligned 8-byte load.
- ✅ Bit operations are plain word loops on aligned arrays. With `-C target-cpu=native`, rustc emits AVX2/AVX-512 and `POPCNT`/`VPOPCNTQ`.

### Query language (Phase 0)

`a b` (AND), `a OR b`, `a -b` / `a NOT b`, parentheses. Keywords are upper-case only. `"phrases"` are rejected until Phase 5. A bare negation (`-a`, `a OR -b`) is an error, because it would need the whole table.

## Analyzer

- UAX #29 word boundaries (`unicode-segmentation`), lower-casing, and NFKD accent stripping.
- ✅ No stemming and no stop words by default. The post explains why ("The Who").
- Tokens over 64 bytes are dropped: hashes, base64 blobs.
- Word positions are already produced, for phrases later.

## What Phase 0 does not have yet

These are all on the [roadmap](ROADMAP.md):

- Postgres integration: index access method, WAL, VACUUM, liveness bitmaps, the visibility-map `COUNT(*)` shortcut.
- Mutable segments and merges.
- BM25: term frequencies, document lengths, block-max top-k.
- Phrases/spans, fuzzy/wildcard/regex.
