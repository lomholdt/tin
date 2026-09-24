-- An index built under 0.1.0 keeps working after ALTER EXTENSION UPDATE,
-- and gains what 0.2.0 added (cost support for the planner).
CREATE EXTENSION pg_tin VERSION '0.1.0';
CREATE TABLE t (id int, body text);
INSERT INTO t SELECT i, 'row ' || i || CASE WHEN i % 10 = 0 THEN ' big bad wolf' ELSE ' wolf' END
  FROM generate_series(1, 5000) i;
CREATE INDEX t_tin ON t USING tin (body);
ALTER EXTENSION pg_tin UPDATE;
SET enable_seqscan = off;
SELECT count(*) AS phrase FROM t WHERE body ==> '"big bad wolf"';
SELECT id, round(tin_score('t_tin'::regclass, body, '"big bad" wolf')::numeric, 3) > 0 AS scored
  FROM t WHERE body ==> '"big bad wolf"' ORDER BY id LIMIT 2;
SELECT id FROM t WHERE body ~> 'ro 42' ORDER BY body <~> 'ro 42', id LIMIT 1;
