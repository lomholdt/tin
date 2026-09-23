\set r random(1, 50000)
\set k random(1, 100)
BEGIN;
INSERT INTO t (body) SELECT body FROM corpus WHERE id = :r;
UPDATE t SET body = (SELECT body FROM corpus WHERE id = :r) WHERE id = (SELECT max(id) - :k FROM t);
DELETE FROM t WHERE id = (SELECT max(id) - :k * 7 FROM t);
END;
