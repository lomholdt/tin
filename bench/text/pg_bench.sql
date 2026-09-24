-- Long-text benchmark: phrases and proximity over the Super User posts,
-- tin (==>, tin_score) vs Postgres full-text search (GIN on
-- to_tsvector('simple', body), @@, ts_rank). Needs the `posts` table and
-- both indexes (see README.md) and queries.tsv. Writes text_results_<engines>.tsv.
\set ON_ERROR_STOP on
-- psql -v engines=tin to run one engine.
\if :{?engines}
\else
  \set engines 'tin,fts'
\endif
SELECT set_config('bench.engines', :'engines', false) AS engines;
SET client_min_messages = warning;  -- to_tsvector notices about very long words
CREATE TEMP TABLE tq (n serial, kind text, tin_q text, tsq_text text);
\copy tq (kind, tin_q, tsq_text) FROM 'queries.tsv'
CREATE TEMP TABLE res (engine text, mode text, kind text, n int, ms float8, cnt bigint, ids bigint[]);

DO $$
DECLARE r record; t0 timestamptz; c bigint; ids bigint[]; pass int; q tsquery;
  tin boolean := 'tin' = ANY(string_to_array(current_setting('bench.engines'), ','));
  fts boolean := 'fts' = ANY(string_to_array(current_setting('bench.engines'), ','));
BEGIN
  SET LOCAL max_parallel_workers_per_gather = 0;
  SET LOCAL work_mem = '256MB';
  FOR pass IN 1..2 LOOP  -- pass 1 warms caches; keep pass 2
    DELETE FROM res;
    FOR r IN SELECT * FROM tq ORDER BY n LOOP
      q := to_tsquery('simple', r.tsq_text);
      IF tin THEN
        -- Every match: count(*).
        t0 := clock_timestamp();
        SELECT count(*) INTO c FROM posts WHERE body ==> r.tin_q;
        INSERT INTO res VALUES ('tin', 'count', r.kind, r.n, extract(epoch FROM clock_timestamp() - t0) * 1000, c);
        -- Best 10 by relevance.
        t0 := clock_timestamp();
        SELECT array_agg(id) INTO ids FROM (SELECT id FROM posts WHERE body ==> r.tin_q
          ORDER BY tin_score('posts_body_tin'::regclass, body, r.tin_q) DESC LIMIT 10) x;
        INSERT INTO res VALUES ('tin', 'top10', r.kind, r.n, extract(epoch FROM clock_timestamp() - t0) * 1000, NULL, ids);
      END IF;
      IF fts THEN
        t0 := clock_timestamp();
        SELECT count(*) INTO c FROM posts WHERE to_tsvector('simple', body) @@ q;
        INSERT INTO res VALUES ('fts', 'count', r.kind, r.n, extract(epoch FROM clock_timestamp() - t0) * 1000, c);
        t0 := clock_timestamp();
        SELECT array_agg(id) INTO ids FROM (SELECT id FROM posts WHERE to_tsvector('simple', body) @@ q
          ORDER BY ts_rank(to_tsvector('simple', body), q) DESC LIMIT 10) x;
        INSERT INTO res VALUES ('fts', 'top10', r.kind, r.n, extract(epoch FROM clock_timestamp() - t0) * 1000, NULL, ids);
      END IF;
    END LOOP;
  END LOOP;
END $$;

\set out 'text_results_' :engines '.tsv'
COPY (SELECT engine, mode, kind, n, ms, cnt FROM res ORDER BY n, engine, mode) TO STDOUT \g :out

-- Latency per kind.
SELECT mode, kind, engine, round(avg(cnt)) AS avg_matches,
       round(percentile_cont(0.5) WITHIN GROUP (ORDER BY ms)::numeric, 1) AS p50_ms,
       round(percentile_cont(0.9) WITHIN GROUP (ORDER BY ms)::numeric, 1) AS p90_ms,
       round(max(ms)::numeric, 1) AS max_ms
FROM res GROUP BY 1, 2, 3 ORDER BY 1, 2, 3 DESC;

-- Do the engines find the same rows? (Counts per query.)
SELECT kind, count(*) AS queries, sum((t.cnt = f.cnt)::int) AS same_count,
       round(avg(abs(t.cnt - f.cnt)::numeric / greatest(f.cnt, 1)) * 100, 2) AS avg_diff_pct
FROM res t JOIN res f USING (kind, n)
WHERE t.engine = 'tin' AND f.engine = 'fts' AND t.mode = 'count' AND f.mode = 'count'
GROUP BY 1 ORDER BY 1;
