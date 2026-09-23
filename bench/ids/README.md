# Identifier search benchmark

Results and methodology: [docs/BENCHMARKS-IDS.md](../../docs/BENCHMARKS-IDS.md).

```sh
python3 bench/ids/generate.py data/ids                       # 5M rows + 10k queries (~1 min)
cd data/ids
psql -f ../../bench/ids/pg_setup.sql                         # table, B-tree, pg_trgm, tin indexes
psql -c "CREATE TABLE qs (kind text, q text, target bigint)" -c "\copy qs FROM 'queries.tsv'"
psql -f ../../bench/ids/pg_bench.sql                         # -> pg_results.tsv
typesense-server --data-dir=ts --api-key=xyz &               # Typesense 30.x
python3 ../../bench/ids/typesense_bench.py . --load          # -> ts_results.tsv
python3 ../../bench/ids/evaluate.py . pg_results.tsv ts_results.tsv
```
