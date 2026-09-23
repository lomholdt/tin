-- A booking changes on a random container.
\set id random(1, 5000000)
UPDATE :tbl SET booking_no = lpad(((random() * 999999999)::bigint)::text, 9, '0') WHERE id = :id;
