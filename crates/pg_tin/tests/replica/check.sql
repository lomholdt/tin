-- Functions for the replica test (created on the primary, so the standby and
-- restored clusters have them too).

-- Rows matching `q`, as a fingerprint: through the index (bitmap scan), or
-- through a sequential scan with the same operator.
CREATE OR REPLACE FUNCTION rows_of(q text, via_index boolean) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text;
BEGIN
  PERFORM set_config('enable_seqscan', CASE WHEN via_index THEN 'off' ELSE 'on' END, true);
  PERFORM set_config('enable_bitmapscan', CASE WHEN via_index THEN 'on' ELSE 'off' END, true);
  PERFORM set_config('enable_indexscan', 'off', true);
  SELECT count(*) || ':' || coalesce(md5(string_agg(id::text, ',' ORDER BY id)), '') INTO r FROM t WHERE body ==> q;
  RETURN r;
END $$;

CREATE OR REPLACE FUNCTION queries() RETURNS SETOF text LANGUAGE sql AS $$
  SELECT unnest(ARRAY['grub', 'windows -linux', 'ssh OR vpn', 'usb drive', 'boot (uefi OR bios)', 'excel',
                      'the', 'a OR the', 'mac -windows', 'network (wifi OR ethernet) -router',
                      '"usb drive"', 'boot NEAR/5 uefi', 'install* windows', '*sql*', 'firefox~'])
$$;

-- Every query's fingerprint through the index, one line per query.
CREATE OR REPLACE FUNCTION snapshot() RETURNS TABLE (q text, via_index text) LANGUAGE sql AS $$
  SELECT q, rows_of(q, true) FROM queries() q ORDER BY q
$$;

-- Queries whose index result differs from a sequential scan, in one snapshot.
CREATE OR REPLACE FUNCTION mismatches() RETURNS TABLE (q text, via_index text, via_seqscan text) LANGUAGE sql AS $$
  SELECT q, rows_of(q, true), rows_of(q, false) FROM queries() q WHERE rows_of(q, true) <> rows_of(q, false)
$$;
