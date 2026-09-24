-- Exactness: the index must return what a sequential scan with the same
-- operator returns (sample of queries with fewer matches, to keep the
-- sequential scans affordable). Then one engine-disagreement example.
\set ON_ERROR_STOP on
CREATE TEMP TABLE tq (n serial, kind text, tin_q text, tsq_text text);
\copy tq (kind, tin_q, tsq_text) FROM 'queries.tsv'
CREATE TEMP TABLE chk (n int, kind text, q text, idx bigint[], seq bigint[]);
DO $$
DECLARE r record; a bigint[]; b bigint[];
BEGIN
  FOR r IN SELECT * FROM tq WHERE kind IN ('phrase3', 'then3', 'near5') AND n % 5 = 0 ORDER BY n LOOP
    SET LOCAL enable_seqscan = off;
    SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO a FROM posts WHERE body ==> r.tin_q;
    CONTINUE WHEN cardinality(a) > 20000;
    SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off; SET LOCAL enable_indexscan = off;
    SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO b FROM posts WHERE body ==> r.tin_q;
    RESET enable_seqscan; RESET enable_bitmapscan; RESET enable_indexscan;
    INSERT INTO chk VALUES (r.n, r.kind, r.tin_q, a, b);
  END LOOP;
END $$;
SELECT kind, count(*) AS queries, sum((idx = seq)::int) AS identical, sum(cardinality(idx)) AS rows FROM chk GROUP BY 1 ORDER BY 1;
