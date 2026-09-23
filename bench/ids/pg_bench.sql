-- Search functions + a timing harness. Needs pg_setup.sql and a `qs` table
-- (kind, q, target) loaded from queries.tsv. Writes pg_results.csv.

-- Plain Postgres, best effort, with Typesense's default semantics:
--   1. exact + prefix matches (exact first), on the B-tree indexes;
--   2. only if none: fragments (LIKE '%x%'), on the pg_trgm GIN indexes;
--   3. only if still none: typos (pg_trgm similarity, best first).
-- Dynamic SQL so each tier gets a custom plan (LIKE 'x%' can only use the
-- B-tree with a literal pattern).
CREATE OR REPLACE FUNCTION search_plain(q text, k int DEFAULT 10)
RETURNS TABLE (id bigint, tier int) LANGUAGE plpgsql AS $$
DECLARE
  u text := upper(btrim(q));
  lit text := quote_literal(upper(btrim(q)));
  n int := 0;
  r record;
BEGIN
  FOR r IN EXECUTE format(
      'SELECT id, (equipment_no = %1$s OR booking_no = %1$s OR bl_no = %1$s) AS exact FROM shipments
        WHERE equipment_no LIKE %2$s OR booking_no LIKE %2$s OR bl_no LIKE %2$s
        ORDER BY 2 DESC LIMIT %3$s', lit, quote_literal(u || '%'), k) LOOP
    id := r.id; tier := CASE WHEN r.exact THEN 0 ELSE 1 END; n := n + 1; RETURN NEXT;
  END LOOP;
  IF n > 0 OR length(u) < 3 THEN RETURN; END IF;
  FOR r IN EXECUTE format(
      'SELECT id FROM shipments WHERE equipment_no LIKE %1$s OR booking_no LIKE %1$s OR bl_no LIKE %1$s LIMIT %2$s',
      quote_literal('%' || u || '%'), k) LOOP
    id := r.id; tier := 2; n := n + 1; RETURN NEXT;
  END LOOP;
  IF n > 0 THEN RETURN; END IF;
  FOR r IN EXECUTE format(
      'SELECT id FROM shipments WHERE equipment_no %% %1$s OR booking_no %% %1$s OR bl_no %% %1$s
        ORDER BY greatest(similarity(equipment_no, %1$s), similarity(booking_no, %1$s), similarity(bl_no, %1$s)) DESC
        LIMIT %2$s', lit, k) LOOP
    id := r.id; tier := 3; RETURN NEXT;
  END LOOP;
END $$;

-- tin today: exact identifiers only (prefix / fragment / typo arrive in Phase 4).
CREATE OR REPLACE FUNCTION search_tin(q text, k int DEFAULT 10)
RETURNS TABLE (id bigint, tier int) LANGUAGE sql AS $$
  SELECT s.id, 0 FROM shipments s WHERE s.search_text ==> q LIMIT k
$$;

CREATE TEMP TABLE res (engine text, kind text, q text, target bigint, ms float8, ids bigint[]);

DO $$
DECLARE r record; t0 timestamptz; ids bigint[]; pass int; eng text;
BEGIN
  SET LOCAL work_mem = '256MB';
  SET LOCAL max_parallel_workers_per_gather = 0;
  FOREACH eng IN ARRAY ARRAY['tin', 'plain'] LOOP
    FOR pass IN 1..2 LOOP  -- pass 1 warms caches; keep pass 2
      DELETE FROM res WHERE engine = eng;
      -- plain-Postgres typo search takes seconds per query at 5M rows
      -- (pg_trgm similarity): measure it on a 50-query sample.
      FOR r IN SELECT * FROM qs
               WHERE NOT (eng = 'plain' AND kind = 'typo_equipment' AND target % 20 <> 0)
               ORDER BY kind, q LOOP
        t0 := clock_timestamp();
        EXECUTE format('SELECT coalesce(array_agg(id ORDER BY tier), ''{}'') FROM search_%s($1, 10)', eng)
          INTO ids USING r.q;
        INSERT INTO res VALUES (eng, r.kind, r.q, r.target,
                                extract(epoch FROM clock_timestamp() - t0) * 1000, ids);
      END LOOP;
    END LOOP;
  END LOOP;
END $$;

\copy (SELECT engine, kind, q, target, ms, array_to_string(ids, ' ') FROM res) TO 'pg_results.tsv' WITH (FORMAT text)
SELECT engine, kind, round(percentile_cont(0.5) WITHIN GROUP (ORDER BY ms)::numeric, 3) p50_ms,
       round(percentile_cont(0.99) WITHIN GROUP (ORDER BY ms)::numeric, 3) p99_ms,
       round(avg((target = ANY(ids))::int), 3) hit10
FROM res GROUP BY 1, 2 ORDER BY 2, 1;
