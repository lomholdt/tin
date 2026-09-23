#!/bin/sh
# Update storm: UPDATES booking changes (90% random rows, 10% the same 100
# hot rows) from 3 clients against TABLE, while (for the tin table) one
# client runs search-box queries and hot-row checks.
#   sh run.sh shipments|shipments_plain [UPDATES] [OUT_DIR]
# Env: PGHOST PGPORT PGUSER PGDATABASE as for psql / pgbench.
set -eu
TABLE=$1
UPDATES=${2:-700000}
OUT=${3:-/tmp/storm}
S=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"; cd "$OUT"; rm -f "$TABLE".*
Q="psql -XAtq -F,"
size() { $Q -c "SELECT pg_size_pretty(sum(pg_relation_size(indexrelid))) FROM pg_index WHERE indrelid = '$TABLE'::regclass"; }
echo "indexes before: $(size)"
if [ "$TABLE" = shipments ]; then
  pgbench -n -c 1 -T 100000 -f "$S/search.sql@9" -f "$S/check_hot.sql@1" \
    --log --log-prefix="$OUT/$TABLE.search" > "$TABLE.search.out" 2>&1 &
  sleep 5 # past the search backend's first (cold) index load
fi
( while [ ! -f "$TABLE.done" ]; do
    $Q -c "SELECT now()::time(0), n_dead_tup, autovacuum_count FROM pg_stat_user_tables WHERE relname = '$TABLE'" >> "$TABLE.stats"
    sleep 15
  done ) &
T0=$(date +%s)
pgbench -n -c 3 -j 3 -t $((UPDATES / 3)) -D tbl="$TABLE" \
  -f "$S/update_random.sql@9" -f "$S/update_hot.sql@1" > "$TABLE.update.out" 2>&1
T1=$(date +%s)
touch "$TABLE.done"
for p in $(pgrep -f "log-prefix=$OUT/$TABLE.search" || true); do kill -TERM "$p"; done
echo "updates: $((UPDATES / 3 * 3)) in $((T1 - T0)) s"
grep -E "^tps|number of failed" "$TABLE.update.out" | head -2
if [ "$TABLE" = shipments ]; then
  grep -E "aborted|ERROR" "$TABLE.search.out" || echo "search client: no errors (hot-row checks all passed)"
  python3 "$S/latency.py" "$OUT/$TABLE.search" "$T0" "$T1" search check_hot
fi
echo "indexes after: $(size)"
echo "autovacuum runs, peak dead tuples: $(tail -1 "$TABLE.stats" | cut -d, -f3), $(cut -d, -f2 "$TABLE.stats" | sort -n | tail -1)"
