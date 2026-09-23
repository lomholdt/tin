-- Identifier benchmark: table + the plain-Postgres baseline indexes + tin.
-- Run from the directory holding shipments.csv.
DROP TABLE IF EXISTS shipments;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pg_tin;
CREATE TABLE shipments (
    id bigint PRIMARY KEY,
    equipment_no text NOT NULL,
    booking_no text NOT NULL,
    bl_no text NOT NULL,
    -- One searchable field for tin: each identifier becomes one term.
    search_text text GENERATED ALWAYS AS (equipment_no || ' ' || booking_no || ' ' || bl_no) STORED
);
\copy shipments (id, equipment_no, booking_no, bl_no) FROM 'shipments.csv' WITH (FORMAT csv)
VACUUM ANALYZE shipments;
SET maintenance_work_mem = '1GB';
\timing on
-- Baseline 1: B-tree, exact + prefix (LIKE 'x%').
CREATE INDEX shipments_eq_btree ON shipments (equipment_no text_pattern_ops);
CREATE INDEX shipments_bk_btree ON shipments (booking_no text_pattern_ops);
CREATE INDEX shipments_bl_btree ON shipments (bl_no text_pattern_ops);
-- Baseline 2: pg_trgm GIN, fragments (LIKE '%x%') + similarity (%).
CREATE INDEX shipments_eq_trgm ON shipments USING gin (equipment_no gin_trgm_ops);
CREATE INDEX shipments_bk_trgm ON shipments USING gin (booking_no gin_trgm_ops);
CREATE INDEX shipments_bl_trgm ON shipments USING gin (bl_no gin_trgm_ops);
-- tin (exact terms only until Phase 4).
CREATE INDEX shipments_tin ON shipments USING tin (search_text);
\timing off
SELECT relname, pg_size_pretty(pg_relation_size(oid)) FROM pg_class
WHERE relname LIKE 'shipments%' ORDER BY relname;
