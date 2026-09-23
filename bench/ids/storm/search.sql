-- A search-box query from the benchmark set.
\set n random(1, 10000)
SELECT count(*) FROM search_tin1((SELECT q FROM qs_n WHERE n = :n));
