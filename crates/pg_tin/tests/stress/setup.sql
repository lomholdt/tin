-- Stress-test schema: a corpus to draw documents from, and the indexed table.
DROP TABLE IF EXISTS t, corpus;
CREATE EXTENSION IF NOT EXISTS pg_tin;
CREATE TABLE corpus (id int PRIMARY KEY, body text);
\copy corpus(id, body) FROM 'corpus.csv' WITH (FORMAT csv)
CREATE TABLE t (id bigserial PRIMARY KEY, body text) WITH (autovacuum_vacuum_threshold = 200, autovacuum_vacuum_scale_factor = 0.01);
INSERT INTO t (body) SELECT body FROM corpus WHERE id <= 20000;
CREATE INDEX t_body_tin ON t USING tin (body);
ALTER DATABASE postgres SET tin.pending_list_limit = '256kB';
