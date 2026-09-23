-- pg_tin regression test: run with scripts/pg-test.sh
CREATE EXTENSION pg_tin;
CREATE TABLE docs (id serial PRIMARY KEY, body text);
INSERT INTO docs (body) VALUES
  ('stretch denim jeans'), ('raw denim jacket'), ('stretch chinos'), (NULL),
  ('Crème brûlée recipe'), ('grub bootloader uefi'), ('grub bios legacy boot'),
  ('København harbour'), ('e-mail from GRUB'), ('');
-- Enough rows to span several heap pages and index segments.
INSERT INTO docs (body)
  SELECT 'filler ' || i || CASE WHEN i % 7 = 0 THEN ' seven' ELSE '' END
                        || CASE WHEN i % 11 = 0 THEN ' eleven' ELSE '' END
  FROM generate_series(1, 20000) i;
SET maintenance_work_mem = '64kB'; -- several segments while building, merged into one
CREATE INDEX docs_body_tin ON docs USING tin (body);
RESET maintenance_work_mem;
SELECT count(*) = 1 AS merged, sum(bytes) > 0 AS has_data
  FROM tin_segments('docs_body_tin'::regclass);
SELECT * FROM tin_segments('docs'::regclass);

SET enable_seqscan = off;
EXPLAIN (costs off) SELECT id FROM docs WHERE body ==> 'denim -jacket';
SELECT id, body FROM docs WHERE body ==> 'denim -jacket' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'stretch' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'creme' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'KØBENHAVN' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'grub (uefi OR bios)' AND body ==> 'boot' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'e-mail' ORDER BY id;
SELECT count(*) FROM docs WHERE body ==> 'seven';
SELECT count(*) FROM docs WHERE body ==> 'seven eleven';
SELECT count(*) FROM docs WHERE body ==> 'seven OR eleven';
SELECT count(*) FROM docs WHERE body ==> 'filler -seven -eleven';
SELECT count(*) FROM docs WHERE body ==> 'nothingmatches';

-- Index and sequential scan must agree exactly.
CREATE TEMP TABLE via_index AS
  SELECT q, array_agg(id ORDER BY id) ids FROM docs,
    unnest(ARRAY['seven', 'seven eleven', 'filler -seven', '(seven OR eleven) -filler', 'grub OR denim']) q
  WHERE body ==> q GROUP BY q;
RESET enable_seqscan;
SET enable_bitmapscan = off;
SELECT q, (SELECT array_agg(id ORDER BY id) FROM docs WHERE body ==> q) = ids AS same_as_seqscan
  FROM via_index ORDER BY q;
RESET enable_bitmapscan;

-- Errors.
SELECT 'x' ==> '"a phrase"';
SELECT 'x' ==> '-only';
SELECT * FROM tin_stats('docs'::regclass);

-- A helper: does the index return exactly what a sequential scan returns?
CREATE FUNCTION same_as_seqscan(q text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE via_index int[]; via_seq int[];
BEGIN
  SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = on;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO via_index FROM docs WHERE body ==> q;
  SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO via_seq FROM docs WHERE body ==> q;
  RETURN via_index = via_seq;
END $$;

-- Writes land in the pending list and are searchable immediately.
INSERT INTO docs (body) VALUES ('fresh denim overalls'), ('fresh linen shirt');
SELECT pending_tuples FROM tin_stats('docs_body_tin'::regclass);
SET enable_seqscan = off;
SELECT id, body FROM docs WHERE body ==> 'fresh' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'denim' ORDER BY id;
-- Updating the indexed column: old version gone, new version found.
UPDATE docs SET body = 'stretch denim shorts' WHERE id = 3;
SELECT id, body FROM docs WHERE body ==> 'stretch' ORDER BY id;
SELECT id FROM docs WHERE body ==> 'chinos';
-- Rolled-back inserts never show up.
BEGIN; INSERT INTO docs (body) VALUES ('ghost row'); ROLLBACK;
SELECT count(*) FROM docs WHERE body ==> 'ghost';
RESET enable_seqscan;

-- Flush the pending list into a new segment.
SELECT tin_flush('docs_body_tin'::regclass) AS flushed;
SELECT segments > 1 AS has_segments, pending_tuples FROM tin_stats('docs_body_tin'::regclass);
SELECT same_as_seqscan(q) FROM unnest(ARRAY['fresh', 'denim', 'stretch -shorts', 'seven eleven']) q;

-- Deletes + VACUUM clear liveness bits, then the freed line pointers get
-- reused by new rows: old queries must not see the newcomers.
CREATE TEMP TABLE seven_slots AS SELECT ctid AS slot FROM docs WHERE body ==> 'seven';
DELETE FROM docs WHERE body ==> 'seven';
VACUUM docs;
SELECT live_tuples < 20012 AS some_dead FROM tin_stats('docs_body_tin'::regclass);
INSERT INTO docs (body) SELECT 'reborn ' || i FROM generate_series(1, 3000) i;
SELECT count(*) > 100 AS reused_slots FROM docs WHERE body ==> 'reborn' AND ctid IN (SELECT slot FROM seven_slots);
SELECT count(*) AS seven_left FROM docs WHERE body ==> 'seven';
SELECT count(*) AS reborn FROM docs WHERE body ==> 'reborn';
SELECT same_as_seqscan(q) FROM unnest(ARRAY['seven', 'reborn', 'filler -eleven', 'eleven OR reborn']) q;

-- Many inserts with a tiny pending limit: automatic flushes.
SET tin.pending_list_limit = '64kB';
INSERT INTO docs (body) SELECT 'bulk ' || i || ' batch' FROM generate_series(1, 5000) i;
SELECT segments >= 4 AS auto_flushed, pending_bytes < 65536 + 1000 AS bounded FROM tin_stats('docs_body_tin'::regclass);
SELECT count(*) FROM docs WHERE body ==> 'bulk batch';
SELECT same_as_seqscan(q) FROM unnest(ARRAY['bulk', 'bulk -batch', 'reborn OR bulk', 'fresh']) q;
DELETE FROM docs WHERE body ==> 'bulk' AND id % 2 = 0;
VACUUM docs;
SELECT count(*) FROM docs WHERE body ==> 'bulk';
SELECT same_as_seqscan('bulk');
RESET tin.pending_list_limit;

-- Identifier patterns on an index with grams: prefix, fragment, typo.
CREATE TABLE ids (id serial PRIMARY KEY, txt text);
INSERT INTO ids (txt)
  SELECT 'MSKU' || lpad((i * 7919 % 10000000)::text, 7, '0') || ' ' || lpad((i * 104729 % 1000000000)::text, 9, '0')
  FROM generate_series(1, 20000) i;
SET maintenance_work_mem = '1MB'; -- too small to hold them all for a merge: several segments
CREATE INDEX ids_tin ON ids USING tin (txt) WITH (grams = true);
RESET maintenance_work_mem;
SELECT count(*) > 1 AS several_segments FROM tin_segments('ids_tin'::regclass);
CREATE FUNCTION ids_same_as_seqscan(q text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE a int[]; b int[];
BEGIN
  SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = on;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO a FROM ids WHERE txt ==> q;
  SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO b FROM ids WHERE txt ==> q;
  RETURN a = b;
END $$;
SET enable_seqscan = off;
SELECT txt FROM ids WHERE txt ==> 'msku0007919' ORDER BY id;
SELECT count(*) FROM ids WHERE txt ==> 'msku00079*';
SELECT txt FROM ids WHERE txt ==> 'msku0007991~' ORDER BY id;  -- swapped 1 and 9
SELECT count(*) FROM ids WHERE txt ==> '*07919*';
RESET enable_seqscan;
SELECT q, ids_same_as_seqscan(q) FROM unnest(ARRAY[
  'msku00*', '*79190*', '*4729*', 'msku0007919~', 'msku0007991~2', 'msku00* -*9*',
  '(msku001* OR *4729*) msku*', '*0007*', 'msku1*', '*791*', 'msku00* -*791*', '*791* -*7919*']) q;
-- Row estimates come from the index (exact for terms), so the search-box
-- shape `==> q LIMIT k` uses the index when q is selective.
CREATE FUNCTION est_rows(q text) RETURNS int LANGUAGE plpgsql AS $$
DECLARE j json;
BEGIN
  EXECUTE format('EXPLAIN (FORMAT JSON) SELECT * FROM ids WHERE txt ==> %L', q) INTO j;
  RETURN (j->0->'Plan'->>'Plan Rows')::int;
END $$;
SELECT q, est_rows(q) AS estimate, (SELECT count(*) FROM ids WHERE txt ==> q) AS actual
  FROM unnest(ARRAY['msku0007919', 'nosuchterm', 'msku00079*', 'msku0007991~', '*07919*',
                    'msku0007919 OR 000104729', 'msku00* -*9*']) q;
EXPLAIN (costs off) SELECT id FROM ids WHERE txt ==> 'msku0007919* -msku0007919' LIMIT 9;
EXPLAIN (costs off) SELECT id FROM ids WHERE txt ==> '*07919*' LIMIT 10;

-- New rows go to the pending list; patterns must see them too.
INSERT INTO ids (txt) VALUES ('MSKU7777777 999999999'), ('MRKU7777770 888888888');
SELECT q, ids_same_as_seqscan(q) FROM unnest(ARRAY['*777777*', 'msku7777777~', 'mrku*', '*99999*']) q;
SELECT tin_flush('ids_tin'::regclass) AS flushed;
SELECT q, ids_same_as_seqscan(q) FROM unnest(ARRAY['*777777*', 'msku7777777~', 'mrku*', '*99999*']) q;
SELECT 'x' ==> 'e-mail*';

-- Ranked search box: `~>` matches, `<~>` ranks (0 exact, 1 prefix,
-- 2 fragment, 3-4 typos), ORDER BY runs as an ordered index scan.
INSERT INTO ids (txt) VALUES ('MSKU0007918 555555555'), ('XMSKU0007919 000000001');
EXPLAIN (costs off) SELECT id FROM ids WHERE txt ~> 'msku0007919' ORDER BY txt <~> 'msku0007919' LIMIT 5;
SELECT txt, txt <~> 'msku0007919' AS rank FROM ids
  WHERE txt ~> 'msku0007919' ORDER BY txt <~> 'msku0007919', id LIMIT 6;
SELECT txt, txt <~> 'msku00079' AS rank FROM ids
  WHERE txt ~> 'msku00079' ORDER BY txt <~> 'msku00079', id LIMIT 4;
-- Index order == ranks non-decreasing, and the same rows as a seqscan.
CREATE FUNCTION ranked_ok(q text, filter text DEFAULT NULL) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE d float8[]; a int[]; b int[]; plan text := ''; line text;
BEGIN
  SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; SET LOCAL enable_sort = off;
  IF filter IS NULL THEN
    -- (Separate queries: an ORDER BY aggregate would sort the input.)
    SELECT array_agg(r) INTO d FROM (SELECT txt <~> q r FROM ids WHERE txt ~> q ORDER BY txt <~> q LIMIT 100000) s;
    SELECT array_agg(id ORDER BY id) INTO a FROM
      (SELECT id FROM ids WHERE txt ~> q ORDER BY txt <~> q LIMIT 100000) s;
    FOR line IN EXECUTE format('EXPLAIN (costs off) SELECT id FROM ids WHERE txt ~> %L ORDER BY txt <~> %L LIMIT 100000', q, q) LOOP
      plan := plan || line;
    END LOOP;
  ELSE
    -- (Separate queries: an ORDER BY aggregate would sort the input.)
    SELECT array_agg(r) INTO d FROM (SELECT txt <~> q r FROM ids WHERE txt ==> filter ORDER BY txt <~> q LIMIT 100000) s;
    SELECT array_agg(id ORDER BY id) INTO a FROM
      (SELECT id FROM ids WHERE txt ==> filter ORDER BY txt <~> q LIMIT 100000) s;
    FOR line IN EXECUTE format('EXPLAIN (costs off) SELECT id FROM ids WHERE txt ==> %L ORDER BY txt <~> %L LIMIT 100000', filter, q) LOOP
      plan := plan || line;
    END LOOP;
  END IF;
  SET LOCAL enable_seqscan = on; SET LOCAL enable_indexscan = off; SET LOCAL enable_sort = on;
  IF filter IS NULL THEN
    SELECT array_agg(id ORDER BY id) INTO b FROM ids WHERE txt ~> q;
  ELSE
    SELECT array_agg(id ORDER BY id) INTO b FROM ids WHERE txt ==> filter;
  END IF;
  RETURN plan LIKE '%Index Scan using ids_tin%' AND a IS NOT DISTINCT FROM b
     AND d IS NOT DISTINCT FROM (SELECT array_agg(x ORDER BY x) FROM unnest(d) x);
END $$;
SELECT q, filter, ranked_ok(q, filter) FROM (VALUES
  ('msku0007919', NULL), ('msku00079', NULL), ('msku0', NULL), ('07919', NULL), ('msku0007991', NULL),
  ('msku0007991x', NULL), ('msku0007919 000104729', NULL), ('msku00079 0001', NULL), ('ms', NULL),
  ('nosuchthing', NULL), ('msku0007919', 'msku000*'), ('7919', '*791*')) v(q, filter);

-- A typo budget (like Typesense's num_typos) drops typo tiers; index and
-- seqscan agree under it too.
SET tin.search_typos = 1;
SELECT txt, txt <~> 'msku0007919' AS rank FROM ids
  WHERE txt ~> 'msku0007919' ORDER BY txt <~> 'msku0007919', id LIMIT 6;
SELECT q, ranked_ok(q) FROM unnest(ARRAY['msku0007919', 'msku0007991', 'msku0007991x']) q;
SET tin.search_typos = 0;
SELECT q, ranked_ok(q) FROM unnest(ARRAY['msku0007919', 'msku0007991']) q;
RESET tin.search_typos;

-- REINDEX compacts everything into fresh segments; cached copies must not be reused.
REINDEX INDEX docs_body_tin;
SELECT pending_tuples FROM tin_stats('docs_body_tin'::regclass);
SELECT same_as_seqscan(q) FROM unnest(ARRAY['bulk', 'reborn', 'fresh', 'denim']) q;
TRUNCATE docs;
SET enable_seqscan = off;
SELECT count(*) FROM docs WHERE body ==> 'denim';
