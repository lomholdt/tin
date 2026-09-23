-- Index vs sequential scan, exact id sets, for a fixed query list.
CREATE OR REPLACE FUNCTION same_as_seqscan(q text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE a bigint[]; b bigint[];
BEGIN
  SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = on;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO a FROM t WHERE body ==> q;
  SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;
  SELECT coalesce(array_agg(id ORDER BY id), '{}') INTO b FROM t WHERE body ==> q;
  RETURN a = b;
END $$;
SELECT count(*) AS queries, sum(same_as_seqscan(q)::int) AS identical
FROM unnest(ARRAY['grub', 'windows -linux', 'ssh OR vpn', 'usb drive', 'boot (uefi OR bios)',
                  'excel', 'the', 'a OR the', 'mac -windows', 'network (wifi OR ethernet) -router']) q;
SELECT * FROM tin_stats('t_body_tin'::regclass);
