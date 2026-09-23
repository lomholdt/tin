-- Update-storm setup (run after ../pg_setup.sql, in the same database).
--
-- Each engine gets a table with only its own indexes, so each pays only its
-- own write cost:
--   shipments        primary key + tin (search_text, grams)
--   shipments_plain  primary key + 3 B-trees + 3 pg_trgm GINs (a copy)
DROP TABLE IF EXISTS shipments_plain;
CREATE TABLE shipments_plain (LIKE shipments INCLUDING GENERATED);
INSERT INTO shipments_plain (id, equipment_no, booking_no, bl_no)
  SELECT id, equipment_no, booking_no, bl_no FROM shipments;
ALTER TABLE shipments_plain ADD PRIMARY KEY (id);
SET maintenance_work_mem = '1GB';
CREATE INDEX ON shipments_plain (equipment_no text_pattern_ops);
CREATE INDEX ON shipments_plain (booking_no text_pattern_ops);
CREATE INDEX ON shipments_plain (bl_no text_pattern_ops);
CREATE INDEX ON shipments_plain USING gin (equipment_no gin_trgm_ops);
CREATE INDEX ON shipments_plain USING gin (booking_no gin_trgm_ops);
CREATE INDEX ON shipments_plain USING gin (bl_no gin_trgm_ops);
DROP INDEX IF EXISTS shipments_eq_btree, shipments_bk_btree, shipments_bl_btree,
                      shipments_eq_trgm, shipments_bk_trgm, shipments_bl_trgm;

-- Updates always change an indexed column (the booking is part of the
-- indexed text), so they can't be HOT and every one leaves a dead tuple:
-- vacuum often, and without throttling.
ALTER TABLE shipments SET (autovacuum_vacuum_scale_factor = 0, autovacuum_vacuum_threshold = 20000,
                           autovacuum_vacuum_insert_scale_factor = 0, autovacuum_vacuum_cost_delay = 0);
ALTER TABLE shipments_plain SET (autovacuum_vacuum_scale_factor = 0, autovacuum_vacuum_threshold = 20000,
                                 autovacuum_vacuum_insert_scale_factor = 0, autovacuum_vacuum_cost_delay = 0);
VACUUM ANALYZE shipments, shipments_plain;

-- Bookings before the storm (hot rows 1-100 and a sample), to check after
-- it that searching an old booking no longer finds the row.
DROP TABLE IF EXISTS storm_before;
CREATE TABLE storm_before AS
  SELECT id, booking_no FROM shipments WHERE id <= 100 OR id % 5000 = 0;

-- Queries by number, for pgbench.
DROP TABLE IF EXISTS qs_n;
CREATE TABLE qs_n AS SELECT row_number() OVER (ORDER BY kind, q)::int n, kind, q, target FROM qs;
ALTER TABLE qs_n ADD PRIMARY KEY (n);
