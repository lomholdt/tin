-- While the storm runs: a hot row must be found by its current booking,
-- read in the same snapshot. A miss divides by zero and aborts the client.
\set id random(1, 100)
BEGIN ISOLATION LEVEL REPEATABLE READ;
SELECT booking_no AS b FROM shipments WHERE id = :id \gset
SELECT 1 / count(*) FROM (SELECT id FROM shipments WHERE search_text ~> ':b'
                          ORDER BY search_text <~> ':b' LIMIT 20) t WHERE id = :id;
COMMIT;
