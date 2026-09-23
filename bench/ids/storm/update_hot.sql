-- The worst case: the same 100 containers rebooked over and over.
\set id random(1, 100)
UPDATE :tbl SET booking_no = lpad(((random() * 999999999)::bigint)::text, 9, '0') WHERE id = :id;
