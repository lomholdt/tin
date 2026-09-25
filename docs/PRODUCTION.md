# Running pg_tin in production

A checklist for the identifier workload tin was built for: ~5M rows of shipping IDs, ~700k updates a day, a search box on top. Numbers come from the benchmarks in [BENCHMARKS-IDS.md](BENCHMARKS-IDS.md); settings from the code.

## 1. Build and install

- **Build on the CPU type you run on.** Builds use `-C target-cpu=native` (`.cargo/config.toml`), so a binary built on a newer CPU crashes with an illegal instruction on an older one. For a fleet with mixed CPUs, build with `RUSTFLAGS="-C target-cpu=x86-64-v3"` (AVX2), or `x86-64-v2` for older hosts.
- **PostgreSQL 18** (shared memory uses `GetNamedDSMSegment`, PG 17+). No `shared_preload_libraries` entry needed.
- **Install** as a superuser: `cd crates/pg_tin && cargo pgrx install --release`, then `CREATE EXTENSION pg_tin;`.

## 2. Create the index

```sql
CREATE INDEX shipments_tin ON shipments USING tin (search_text) WITH (grams = true);
```

- **`grams = true`** for identifiers: fast `*fragment*` search, at 2.3× the index size (5M IDs: 160 → 373 MB). Leave it off for long text.
- **`CREATE INDEX CONCURRENTLY` and `REINDEX INDEX CONCURRENTLY` work** and don't block writes.
- **Build fast:** `SET max_parallel_maintenance_workers = <cores>; SET maintenance_work_mem = '2GB';` before `CREATE INDEX` / `REINDEX`. 5M rows: ~30 s on 4 cores. Each build thread needs 32 MB of `maintenance_work_mem`.
- **One segment after the build** if the index fits in half of `maintenance_work_mem`. Check with `SELECT count(*) FROM tin_segments('shipments_tin'::regclass);`.

## 3. Server settings

| Setting | Recommended | Why |
|---|---|---|
| `maintenance_work_mem` (postgresql.conf, not just a session) | **≥ 2.5 × index size**, e.g. 2GB for 5M IDs | Autovacuum compacts an index back into one segment only if it fits in half of this. Below that, segments pile up and searches slow down (16 segments made them ~4× slower). |
| `tin.shared_cache_size` | **≥ 1.5 × index size** (default 1GB) | One shared copy of the segments for all connections. Below the index size, segments get evicted and the next connection pays a reload (~1 s at 5M rows). Headroom covers a compaction's second copy. |
| `tin.search_typos` | **1** for ID search boxes | Like Typesense's `num_typos = 1`. p99 ≤ 4 ms for every query kind at 5M rows, and higher precision than 2. |
| `tin.pending_list_limit` | default (1MB) | Inserts collect here until a flush turns them into a segment. Larger lists make searches scan more unflushed rows. |
| `max_parallel_workers_per_gather` | default (2) or more | Only long-text queries use it; ID searches are single index scans. |

Per table with heavy updates (what the 700k/day storm used):

```sql
ALTER TABLE shipments SET (autovacuum_vacuum_scale_factor = 0, autovacuum_vacuum_threshold = 20000,
                           autovacuum_vacuum_insert_scale_factor = 0, autovacuum_vacuum_cost_delay = 0);
```

VACUUM clears dead tuples from the index, flushes the pending list, and compacts. Frequent small vacuums keep the index close to its rebuilt size (512 MB → 654 MB at the end of the storm, not growing without bound).

## 4. Queries

```sql
-- search box: exact > prefix > fragment > typo, top 10 from an ordered index scan
SELECT id FROM shipments WHERE search_text ~> $1 ORDER BY search_text <~> $1 LIMIT 10;

-- exact tin query language
SELECT id FROM shipments WHERE search_text ==> 'msku60* -*999*' LIMIT 50;
```

- **Always `LIMIT` search-box queries.** The ordered scan stops at k rows. Without a limit it walks every typo tier.
- **A bare negation** (`-foo`) is an error by design: it would need the whole table.

## 5. Monitoring

```sql
SELECT * FROM tin_stats('shipments_tin'::regclass);     -- segments, live/pending tuples, generation
SELECT * FROM tin_segments('shipments_tin'::regclass);  -- per segment: blocks, bytes, live tuples
SELECT * FROM tin_shared_stats();                       -- segments and bytes in shared memory
```

| Watch | Healthy | If not |
|---|---|---|
| `tin_stats.segments` | 1–12, back to ~1 after autovacuum compacts | Stays above 12: compaction isn't running. Raise `maintenance_work_mem` in postgresql.conf, or `REINDEX` in a quiet hour. |
| `tin_stats.pending_bytes` | below `tin.pending_list_limit` | Growing: inserts can't flush. Check the logs for errors, or run `SELECT tin_flush('shipments_tin'::regclass);`. |
| `tin_shared_stats().bytes` | below `tin.shared_cache_size` | At the limit: raise `tin.shared_cache_size`. |
| Index size (`pg_relation_size`) | within ~1.5× of a fresh build | Much larger: dead tuples not compacted (see the first row). |
| `pg_stat_user_tables.n_dead_tup` | falls after each autovacuum | Growing: autovacuum can't keep up; lower the threshold. |

## 6. Upgrades

- **Extension:** install the new build, then `ALTER EXTENSION pg_tin UPDATE;` in each database. Indexes are kept; upgrade scripts are idempotent and tested in CI (see [README](../README.md#upgrading)).
- **Index format:** versioned separately. A newer build that can't read an index says so ("REINDEX it"). Plan a `REINDEX INDEX CONCURRENTLY`: ~30 s of build work at 5M rows.
- **Reconnect** after installing a new `.so`: a backend keeps the library it loaded until it exits, so recycle pooled connections (or restart the server).

## 7. Durability and limits

- **Crash-safe:** every index write is WAL-logged (generic WAL). `kill -9` mid-storm, followed by recovery, gave 0 wrong results.
- **Replicas and point-in-time recovery: tested** (`crates/pg_tin/tests/replica/run.sh`, run in CI). It covers a streaming hot standby queried while it replays write storms, flushes, VACUUM/compaction and `REINDEX CONCURRENTLY`, then recovery to a restore point, then promotion. Every check returned the primary's rows and matched a sequential scan.
  - **Use pg_tin 0.2.1 or later on the primary.** WAL written by earlier builds could deadlock a hot standby: replay stops while a query runs, and it never resumes. The fix is in how the primary writes WAL, so upgrade the primary; replicas need the same build installed.
  - **Set `hot_standby_feedback = on` on replicas you query.** Without it, the standby cancels queries that conflict with replayed cleanup: 393 of ~500 test queries during heavy writes. That is standard Postgres behaviour, not specific to tin.
- **Inserts into one index are serialized** on its metapage lock. The appends are short; flushes and merges run off the lock. Measured: ~4,200 updates/s with concurrent search p99 11 ms.
- **At most 253 segments per index.** Compaction and tiered merges keep it far lower; if a build hits the limit, raise `maintenance_work_mem`.
- **First query after a server restart** copies each segment into shared memory (~1 s at 5M rows). Every later connection maps it (~20 ms). Warm up with one search after a restart.
- **Deleted rows count in scores** (`tin_score` document frequencies) until VACUUM compacts, as in most search engines. ID search doesn't use scores.
