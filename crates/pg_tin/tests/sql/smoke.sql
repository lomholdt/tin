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
SET maintenance_work_mem = '1MB'; -- force several segments
CREATE INDEX docs_body_tin ON docs USING tin (body);
RESET maintenance_work_mem;
SELECT count(*) > 1 AS several_segments, sum(bytes) > 0 AS has_data
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
INSERT INTO docs (body) VALUES ('new row');

-- Deletes stay correct (Phase 1 keeps dead tids; visibility filters them).
DELETE FROM docs WHERE id = 1;
VACUUM docs;
SET enable_seqscan = off;
SELECT id FROM docs WHERE body ==> 'denim' ORDER BY id;

-- REINDEX builds a new generation; cached copies must not be reused.
REINDEX INDEX docs_body_tin;
SELECT id FROM docs WHERE body ==> 'denim' ORDER BY id;
TRUNCATE docs;
SELECT count(*) FROM docs WHERE body ==> 'denim';
