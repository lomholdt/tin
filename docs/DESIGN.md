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

### Query language

Modelled on TIN's query language (TINQL, as documented in `planetscale/lead`; syntax and ideas only, no code):

| Syntax | Meaning |
|---|---|
| `a b`, `a AND b` | both |
| `a OR b`, `[a b c]` | any (commas in `[ ]` optional) |
| `a AND NOT b`, `a -b`, `a NOT b` | `a` without `b` |
| `"a b c"` | a phrase: consecutive words, in order |
| `"a _ c"`, `"[a x] b"`, `"a b c"~2` | skip exactly one word; choices for one position; up to 2 extra words in all |
| `a THEN/N b`, `a NEAR/N b` | `b` after `a` (or either order) with at most N words between |
| `AT LEAST 2 OF [a b c]`, `AT LEAST 50% OF [...]`, `ALL OF [...]` | minimum match |
| `a^2` | boost: scales the item's weight in scores |

- **Precedence**, loosest first: `OR`, then `AND`/negation, then `THEN`/`NEAR` (left to right), then `^`. Keywords are upper case only.
- **A word the analyzer splits** (`e-mail`) is a phrase of its parts.
- **A bare negation** (`-a`, `a OR -b`) is an error, because it would need the whole table.
- **Differences from TINQL:** a leading `-` negates, where TINQL keeps hyphens in the term. `~N` on a word is an edit distance with no fixed prefix. Not supported yet: `?` wildcards, `TO` ranges, `MATCHES`, `WITHIN`, positional filters and span relations.

🔧 **Positions without a positional index.** The index stores only which rows hold a term, so a phrase or proximity query is planned as the terms it needs (`Plan::Recheck`), and each candidate row is rechecked against its word positions (`tin_core::span`):
- Every query node denotes its set of **minimal intervals** of word positions (the Clarke–Cormack–Burkowski algebra that Boldi and Vigna made lazy). `AND` gives minimal windows over its operands, a phrase or `THEN` gives ordered windows within the distance, and so on. A row matches if the root's set is non-empty.
- **Negation is row-level.** Under a negation the index may subtract only what it knows exactly: `a -"b c"` plans as `a`, not `a -b -c`.
- **The trade:** a phrase of common words costs a recheck of every row holding all its words. The alternative, positions in the index, would roughly triple its size; see [BENCHMARKS](BENCHMARKS.md#phase-8-phrases-proximity-and-scoring) for what the recheck costs.

### Scoring and highlighting

- `tin_score(index, doc, q)`: **BM25** (k1 = 1.2, b = 0.75) over the query's positive terms. Like TIN, it re-reads the row: term frequencies come from the row's text, and the collection statistics from the index (live rows, per-term document frequency from each segment's dictionary and the pending list).
- 🔧 **Length = distinct terms.** The index's postings add up to exactly the sum of each row's distinct terms (4-grams excluded), so the average length is exact without storing lengths.
- **Weights:** each leaf gets the product of the boosts above it. A term matched by several leaves gets the sum, as in TINQL (`"craft beer"^3 OR craft` weighs `craft` 4). Terms under a negation don't count. Patterns score each row term they match.
- `tin_score_inspect(index, doc, q)` returns the breakdown as JSON.
- `tin_highlight(doc, q)` and `tin_snippet(doc, q, words)` mark the words that made the row match. For phrase and proximity terms, only occurrences inside a match are marked.

Single-term patterns (Phase 4) combine with all of the above:

| Pattern | Matches terms that | Example |
|---|---|---|
| `term*` | start with `term` | `msku60*` |
| `*frag*` or `*frag` | contain `frag` | `*6018200*` |
| `term~`, `term~2` | are within 1 (2) edits of `term` | `msku6012800~` |

### Search box (Phase 5)

`col ~> q` matches `q` the way a search box should; `col <~> q` ranks how well it matched (`tin_core::search`):

| Rank | Some term of the row… | For query terms of |
|---:|---|---|
| 0 | equals it | any length |
| 1 | starts with it | any length |
| 2 | contains it | 3+ chars |
| 3 | is 1 typo away | 4+ chars |
| 4 | is 2 typos away | 7+ chars |

- With several query terms, every term must match, and the worst one decides the rank.
- `tin.search_typos` (0–2) drops typo ranks. Both the index and the per-row functions read it, so those functions are `STABLE`.
- 🔧 **Ordered scans** (`tin_core::rank::Ranked`, `amgettuple`): rank 0, then 1, … Each rank is either streamed from the dictionary (single-term prefixes and typos, 32 terms at a time) or evaluated as a plan over each segment. Every row is returned once, at its first rank. Fragment candidates carry their rank as a lower bound (`xs_recheckorderby`), and the executor re-ranks them. Other conditions on the same scan are ANDed in, and rows matching those but no rank come last, at infinity.
- 🔧 **Lazy DFA** for the typo automaton (`pattern::OsaDfa`): DP states are numbered as they are reached, and transitions per (state, byte class) are cached.
- 🔧 **One segment after `CREATE INDEX`**: closed build segments are held while they fit in half of `maintenance_work_mem`, then merged, so dictionary walks touch one FST.

### Identifier patterns (Phase 4)

- 🔧 **Prefix**: the FST's `starts_with` range. Every matching term's postings are ORed into one tuple-space bitmap per segment (the `Bits` cursor), which then behaves like any other cursor under AND/OR/NOT. Unique IDs are singletons stored in the dictionary value, so expansion reads no postings.
- 🔧 **Typo**: an automaton over the FST that accepts terms within *k* **optimal-string-alignment** edits: insert, delete, substitute, or swap two neighbours. Plain Levenshtein counts a swap as two edits, and a swapped pair is the most common typo in a typed number. The DP rows are fixed-size arrays, so following an FST edge allocates nothing.
- 🔧 **Fragments**, with `WITH (grams = true)`:
  - The index also stores every term's **4-grams**, with the last one padded by an end marker (`\u{2}`). Gram terms start with `\u{1}`, which the analyzer never emits.
  - A 4+-character fragment is an AND of its own 4-grams. That gives *candidates*: the grams may come from different terms. So those scans return tids with `recheck = true`, and the executor confirms each one with `tin_match`.
  - Grams more than 10× as frequent as the fragment's rarest are left out of the AND. They cost more to intersect than the few candidates they remove.
  - A 3-character fragment is an **exact** prefix search over grams (`\u{1}abc…`). The end padding makes every 3-character substring the start of some gram.
  - Without grams, or under NOT (where a candidate superset would drop real rows), fragments scan the whole dictionary instead.
  - 🔧 **Why 4-grams, not trigrams** (pg_trgm's choice): identifiers are mostly digits, and there are only 1,000 digit trigrams, so each is in ~2% of rows and a fragment ANDs long lists. With 10,000 digit 4-grams, fragment search was 14× faster at +11% index size.

## Analyzer

- UAX #29 word boundaries, lower-casing, and NFKD accent stripping.
- 🔧 **Fast, exact word splitting** (`tokenize::for_each_word`): ASCII text goes through a bitmask implementation of the UAX #29 rules that apply to ASCII (AVX2 when available). Pieces around non-ASCII bytes go through `unicode-segmentation`, cut only where UAX #29 must break and no rule looks across. The output is identical to `unicode_word_indices` (differential test; 0 differences on 1.24M posts), 5× faster on English text.
- ✅ No stemming and no stop words by default. The post explains why ("The Who").
- Tokens over 64 bytes are dropped: hashes, base64 blobs.
- Word positions (and byte offsets, for highlighting) are produced alongside terms; phrases and proximity are checked against them.

## Postgres integration (`crates/pg_tin`)

A pgrx extension for **PostgreSQL 18**, adding the `tin` index access method, the `==>` operator (`text ==> text`) and the default operator class `text_tin_ops`.

```sql
CREATE EXTENSION pg_tin;
CREATE INDEX posts_body_tin ON posts USING tin (body);
SELECT count(*) FROM posts WHERE body ==> 'grub (uefi OR bios) -windows';
SELECT * FROM tin_stats('posts_body_tin'::regclass);    -- segments, live/pending tuples
SELECT * FROM tin_segments('posts_body_tin'::regclass); -- one row per segment
SELECT tin_flush('posts_body_tin'::regclass);           -- flush the pending list now
```

### On disk (format 3)

- **Block 0**: the metapage, holding a generation counter, the segment table, the pending-list pointers, and the retired chains.
- **Everything else**: page chains, `[next u32][flags u32][data…]`, used for:
  - each segment's `to_bytes()`;
  - each segment's **liveness bitmap**;
  - the **pending list**.
- **Page reuse**: chains mean any freed page can be reused through the free space map. A freed page is first stamped `FREE` in a WAL-logged write, and the allocator only takes stamped pages. The FSM isn't WAL-logged and can be stale after a crash, so this check is what makes reuse safe.

### Build (`ambuild`)

- A serial heap scan through `index_build_range_scan`, with `allow_sync = false` so tids arrive in block order.
- Each page's tuples are sorted first, because HOT chains can report a root offset after higher ones.
- **Serial** (`max_parallel_maintenance_workers = 0`): a segment is closed at a page boundary once its builder passes `maintenance_work_mem`, so memory stays bounded.
- 🔧 **Parallel**: the backend only scans. It hands batches of whole blocks to Rust threads, each of which builds one segment per batch.
  - The threads never call into Postgres and run with every signal blocked, so Postgres's handlers only run on the backend.
  - While waiting, the backend checks for interrupts. The threads are joined however the build ends.
  - Batches are sized so the builders fit in half of `maintenance_work_mem`. The size uses the builder bytes per text byte seen so far.
  - Like Postgres's own parallel builds, each thread needs 32 MB of `maintenance_work_mem`.
- Closed segments are held and merged into one at the end. A serial build holds up to half of `maintenance_work_mem` and writes segments out as they close past that. A parallel build holds up to a quarter; past that, it merges what it holds into one segment and writes it.
  - 🔧 The merge is split by term range (`Merge::split_points` / `part`). Threads merge the ranges, while the backend joins finished ranges into the one FST in key order (`MergeWriter`).
  - Moving postings between ranges only shifts their offsets, so the result is byte-identical to a serial build.
- Pages are written unlogged, then WAL-logged in one pass with `log_newpage_range`, which is how GIN logs its build.

### Writes

- **`aminsert`** analyzes the value and appends a `(tid, sorted distinct terms)` record to the pending list.
  - It holds the metapage lock exclusively, so inserts into one index are serialized.
  - The record's last chunk and the metapage update go into the same Generic WAL record.
- **Flush**: past `tin.pending_list_limit` (default 1 MB; see [BENCHMARKS-IDS](BENCHMARKS-IDS.md#phase-6-update-storm)), at VACUUM, or on `tin_flush()`, the pending records become a new immutable segment (`SegmentBuilder::add_terms`).
  - 🔧 **Off the lock**: the flushing backend snapshots the segment table and the pending list, then builds (and merges) without the metapage lock. It takes the lock only to install the result, keeping records appended meanwhile. Searches and inserts wait milliseconds, not the seconds a big merge takes.
  - A **flush mutex** (a heavyweight lock on block 0, like GIN's pending-list cleanup) allows one flush at a time. An inserter that finds it taken keeps appending. VACUUM takes it for its pass 2 and cleanup, so neither can happen in the middle of a flush.
- **Merges**: segments are grouped by size into tiers (64 kB × 8ᵗ). When 4 segments share a tier they are merged into one (`Segment::merge` drops dead tuples). Every search walks every segment, so few segments matter.
  - Merges run as part of a flush (off the lock), except while a compaction runs.
  - Merged-away chains are *retired*, and only recycled by the next VACUUM's cleanup (see below).
- **Compaction**, in VACUUM's cleanup (an autovacuum worker): when the smaller segments add up to more than 25% of the largest (e.g. the one `CREATE INDEX` built, which is too big to merge inline), everything is merged into one segment.
  - It holds its own *compaction lock*. Flushes carry on meanwhile and just skip merging. The install keeps the segments flushed in the meantime.
  - It runs only if 2× the index fits in `maintenance_work_mem`.
- **VACUUM (`ambulkdelete`)** asks, for every live bit of every segment, whether that tid is dead, and clears the bits that are. It works in two passes:
  1. **Without the metapage lock**, so writers keep going: the segments that exist at the start.
  2. **With the metapage lock held exclusively**: segments flushed or merged in the meantime, plus the pending list, whose dead records are dropped by rewriting it.

  All of this happens before the heap marks the dead line pointers reusable. `amvacuumcleanup` then flushes the pending list and frees retired chains. It's safe to free them there, because the only lock-free reader (pass 1 of *this* VACUUM) has finished, and Postgres runs one VACUUM per table at a time.

### Reads

- 🔧 **Segments in shared memory** (`shared.rs`): a registry in a named DSM segment (`GetNamedDSMSegment`, PG 17+) maps (relfile, segment head block) to a pinned DSM segment holding the serialized segment.
  - The first backend to need a segment copies it in; the others map it. `Segment::from_shared` keeps the dictionary and postings as views into the mapping, copying only the page directory and docs bitmap.
  - Flushes and compactions publish new segments directly, and VACUUM unpins the ones it frees.
  - Least recently used entries are dropped past `tin.shared_cache_size`.
  - A hit is checked against the index's first page, in case a dropped index's relfile number was reused.
  - Mappings are detached before Postgres tears down DSM at backend exit.
- **Per-backend cache**, keyed by `(index OID, relfilenumber)`. REINDEX, TRUNCATE and VACUUM FULL change the relfilenumber, so a stale copy is never used. Under a shared metapage lock:
  - if the generation changed, the backend reloads every liveness bitmap and the whole pending list, and loads any segments it hasn't seen (segments are immutable and reused by id);
  - otherwise it reads only the new tail of the pending list.
- **Bitmap scans** (`amgetbitmap`) for `==>` and `~>`, and **ordered / plain index scans** (`amgettuple`, see [Search box](#search-box-phase-5)). Several conditions are ANDed into one plan.
  - Each segment is searched with its liveness bitmap, which is ANDed per 256-page group in the tuple space.
  - Pending records are matched directly.
  - Tids go to `tbm_add_tuples` **without recheck**, except for plans with gram-answered fragments (candidates, see above).
- **Why skipping the recheck is sound**: a dead tuple's bit is cleared before its line pointer can be reused. A backend whose cached liveness predates that VACUUM can only return tids that were dead when its snapshot was taken, and any tuple later placed in a reused slot is invisible to that snapshot. The SQL test forces line-pointer reuse; disabling the liveness filter makes it fail with 2,488 wrong rows.
- **Sequential scans / rechecks**: `tin_match(doc, query)` uses the same analyzer and `Plan::matches_text`, so both paths return identical rows.
- **Costing**: `genericcostestimate`, with row estimates from the index itself (`tin_restrict`, the operator's `RESTRICT` function).
  - It finds the tin index on the clause's column or expression and asks its segments (`Segment::estimate`): exact document frequencies for terms; sums over matching terms for prefixes and typos (at most 1,000 terms read); grams as if independent for fragments; independence for AND / OR / NOT. Pending records are sampled.
  - When unsure, it errs low. A flat guess (`contsel`, 0.1%) made `WHERE col ==> q LIMIT 10` a sequential scan: the planner expected a match every thousand rows, then read all 5M rows when q matched one.
  - `tin_match` is declared `COST 10`, since analyzing a row costs about ten simple operators.

### Known limits

- Inserts into one index are serialized on the metapage lock (appends are short; flushes and merges run off it).
- The first backend after a server restart copies each segment into shared memory (~1 s at 5M rows); every other backend maps it (~20 ms).

## What is not built yet

See the [roadmap](ROADMAP.md). Among them:
- **top-k by score inside the index** (WAND/block-max); today `ORDER BY tin_score(...) LIMIT k` scores every match;
- the rest of TINQL (ranges, regex, positional filters, span relations);
- the visibility-map `COUNT(*)`.
