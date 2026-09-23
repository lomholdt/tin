-- After the storm: tin must agree with a B-tree on booking_no for the old
-- and current bookings of the hot rows and a sample, and the ranked search
-- must find every hot row by its current booking. (A 9-digit booking is a
-- term of its own, so `search_text ==> b` means `booking_no = b`.)
CREATE INDEX IF NOT EXISTS shipments_bk_check ON shipments (booking_no);
CREATE TEMP TABLE probe AS
  SELECT booking_no b FROM storm_before
  UNION SELECT s.booking_no FROM shipments s JOIN storm_before USING (id);
SET enable_seqscan = off;
SELECT count(*) AS probes,
       count(*) FILTER (WHERE tin IS DISTINCT FROM btree) AS mismatches
FROM (SELECT b,
             (SELECT array_agg(id ORDER BY id) FROM shipments WHERE search_text ==> b) tin,
             (SELECT array_agg(id ORDER BY id) FROM shipments WHERE booking_no = b) btree
      FROM probe) x;
SELECT count(*) AS hot_rows,
       count(*) FILTER (WHERE s.id IN (SELECT t.id FROM shipments t WHERE t.search_text ~> s.booking_no
                                        ORDER BY t.search_text <~> s.booking_no LIMIT 20)) AS found
FROM shipments s WHERE s.id <= 100;
RESET enable_seqscan;
DROP INDEX shipments_bk_check;
