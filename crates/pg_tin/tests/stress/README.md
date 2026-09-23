# Stress tests for writes

Manual tests. They need a running PostgreSQL 18 with `pg_tin` installed.

```sh
# 50k Super User posts as the document pool (after scripts/fetch-superuser.sh)
head -50000 data/superuser.docs.txt | python3 -c "import sys,csv; w=csv.writer(sys.stdout); [w.writerow([i, l.rstrip('\n')]) for i, l in enumerate(sys.stdin, 1)]" > crates/pg_tin/tests/stress/corpus.csv
cd crates/pg_tin/tests/stress
psql -f setup.sql                       # 20k rows, tin index, 256kB pending limit

# Concurrent writers (insert + update + delete per txn) and readers, with VACUUM in a loop
(for i in $(seq 12); do psql -qc 'VACUUM t'; sleep 5; done) &
pgbench -n -c 8 -j 4 -T 60 -f write.sql@3 -f read.sql@1
psql -f verify.sql                      # queries | identical  ->  10 | 10

# Crash: run the same load, `kill -9` the postmaster mid-run, restart, then
psql -f verify.sql; psql -c 'VACUUM t'; psql -f verify.sql
```
