# Long-text benchmark (Phase 8)

Phrases, proximity and relevance ranking over the 1.24M Super User posts,
tin vs PostgreSQL's own full-text search. Results are in
[docs/BENCHMARKS.md](../../docs/BENCHMARKS.md#phase-8-phrases-proximity-and-scoring).

## Setup

The `posts` table from Phase 1 (`scripts/fetch-superuser.sh`, then load
`superuser.docs.txt` as `posts(id bigserial, body text)`), with both indexes:

```sql
CREATE INDEX posts_body_tin ON posts USING tin (body);
CREATE INDEX posts_body_gin ON posts USING gin (to_tsvector('simple', body));
```

## Run

```sh
python3 queries.py path/to/superuser.docs.txt > queries.tsv   # 250 queries
psql -f pg_bench.sql                                          # writes text_results.tsv
```

Query kinds (50 each), all sampled from real posts so each has matches:

| Kind | tin | Postgres (`to_tsquery('simple', …)`) |
|---|---|---|
| `phrase2` | `"a b"` | `a <-> b` |
| `phrase3` | `"a b c"` | `a <-> b <-> c` |
| `near5` | `a NEAR/5 b` | `(a <1> b) \| … \| (a <6> b) \| (b <1> a) \| …` |
| `then3` | `a THEN/3 b` | `(a <1> b) \| … \| (a <4> b)` |
| `and3` | `a b c` | `a & b & c` |

Each query runs as `count(*)` (every match) and as the best 10 by
relevance (`tin_score` / `ts_rank`), twice; the second pass is kept.

Words are lower-case ASCII letters, 3+ long, so both engines split them
alike. They can still disagree on a few rows: Postgres's `simple` parser
and tin's Unicode word boundaries count positions differently around
some punctuation (URLs, file paths, `don't`), which moves words closer or
further apart.
