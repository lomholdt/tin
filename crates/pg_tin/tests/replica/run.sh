#!/bin/sh
# Replication and point-in-time recovery with tin indexes.
#
#   PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config sh crates/pg_tin/tests/replica/run.sh
#
# Needs an installed pg_tin; uses the stress corpus when prepared (see
# ../stress/README.md), else generates one (corpus.py).
# Starts a primary with WAL archiving and a streaming hot standby, then:
#   1. rounds of writes (inserts, updates, deletes; a tiny pending list, so
#      many flushes and merges) with VACUUM (and compaction) in a loop;
#      meanwhile the standby is queried continuously, index vs seqscan in
#      one snapshot; after each round, the standby (caught up) must return
#      the primary's rows, and both must match a sequential scan;
#   2. mid-run, a restore point, with the primary's results recorded;
#   3. point-in-time recovery of a base backup to that restore point: the
#      restored cluster must return exactly the recorded rows;
#   4. the standby is promoted, written to and vacuumed: still exact.
# Exits non-zero on any mismatch. ROUNDS (default 4) and SECONDS per round
# (default 20) tune the length; KEEP_LOGS=dir keeps the server logs of a
# failed run; FEEDBACK=off runs the standby without hot_standby_feedback.
set -eu
PG_CONFIG=${PG_CONFIG:-pg_config}
BIN=$("$PG_CONFIG" --bindir)
HERE=$(cd "$(dirname "$0")" && pwd)
STRESS="$HERE/../stress"
ROUNDS=${ROUNDS:-4}
SECONDS_PER_ROUND=${SECONDS:-20}
W=$(mktemp -d)
AS=""
if [ "$(id -u)" = 0 ]; then AS="runuser -u postgres --"; chown postgres "$W"; chmod 755 "$W"; fi
P=54411; S=54412; R=54413
mkdir -p "$W/archive"; [ -n "$AS" ] && chown postgres "$W/archive"
cleanup() {
  touch "$W/stop.checker" "$W/stop.vacuum" 2>/dev/null || true
  for j in $(jobs -p); do kill "$j" 2>/dev/null || true; done
  for d in primary standby pitr; do
    [ -d "$W/$d" ] && $AS "$BIN/pg_ctl" -D "$W/$d" -m immediate stop >/dev/null 2>&1 || true
  done
  rm -rf "$W"
}
trap cleanup EXIT
# The stress scripts, with the Super User corpus if it was prepared (see
# ../stress/README.md), else a synthetic one.
mkdir -p "$W/stress"
cp "$STRESS"/setup.sql "$STRESS"/write.sql "$STRESS"/read.sql "$W/stress/"
if [ -f "$STRESS/corpus.csv" ]; then
  cp "$STRESS/corpus.csv" "$W/stress/"
else
  python3 "$HERE/corpus.py" > "$W/stress/corpus.csv"
fi
STRESS="$W/stress"
qp() { port=$1; shift; "$BIN/psql" -h "$W" -p "$port" -U postgres -XAtq -v ON_ERROR_STOP=1 "$@"; }
fail() {
  echo "FAIL: $*" >&2
  if [ -n "${KEEP_LOGS:-}" ]; then mkdir -p "$KEEP_LOGS" && cp "$W"/*.log "$KEEP_LOGS"/ 2>/dev/null || true; fi
  exit 1
}

echo "== primary"
$AS "$BIN/initdb" -D "$W/primary" -A trust -U postgres -E UTF8 --locale=C >/dev/null
cat >> "$W/primary/postgresql.conf" <<CONF
port = $P
unix_socket_directories = '$W'
wal_level = replica
max_wal_senders = 4
archive_mode = on
archive_command = 'cp %p $W/archive/%f'
autovacuum_naptime = 1s
max_standby_streaming_delay = 60s
CONF
$AS "$BIN/pg_ctl" -D "$W/primary" -l "$W/primary.log" -w start >/dev/null
(cd "$STRESS" && qp $P -f setup.sql >/dev/null)
qp $P -c "ALTER DATABASE postgres SET tin.pending_list_limit = '64kB'" -f "$HERE/check.sql" >/dev/null

echo "== base backups: standby (streaming) and one for point-in-time recovery"
$AS "$BIN/pg_basebackup" -h "$W" -p $P -U postgres -D "$W/standby" -R -X stream >/dev/null
$AS "$BIN/pg_basebackup" -h "$W" -p $P -U postgres -D "$W/pitr" -X none >/dev/null 2>&1 || \
  $AS "$BIN/pg_basebackup" -h "$W" -p $P -U postgres -D "$W/pitr" >/dev/null
sed -i "s/^port = $P/port = $S/" "$W/standby/postgresql.conf"
# As on a production replica: queries on the standby hold back cleanup on
# the primary instead of stalling replay (or being cancelled by it).
printf 'hot_standby = on\nhot_standby_feedback = %s\n' "${FEEDBACK:-on}" >> "$W/standby/postgresql.conf"
$AS "$BIN/pg_ctl" -D "$W/standby" -l "$W/standby.log" -w start >/dev/null

caught_up() {
  lsn=$(qp $P -c "SELECT pg_current_wal_lsn()")
  i=0
  until [ "$(qp $S -c "SELECT pg_last_wal_replay_lsn() >= '$lsn'")" = t ]; do
    i=$((i + 1))
    if [ $i -gt 600 ]; then
      echo "   primary at $lsn; standby received $(qp $S -c "SELECT pg_last_wal_receive_lsn()"), replayed $(qp $S -c "SELECT pg_last_wal_replay_lsn()")" >&2
      qp $S -c "SELECT backend_type, state, wait_event_type, wait_event, left(query, 60) FROM pg_stat_activity WHERE backend_type IN ('startup', 'client backend')" >&2 || true
      fail "standby did not catch up"
    fi
    sleep 0.2
  done
}

compare_standby() {
  caught_up
  qp $P -c "SELECT * FROM snapshot()" > "$W/p.snap"
  qp $S -c "SELECT * FROM snapshot()" > "$W/s.snap"
  diff "$W/p.snap" "$W/s.snap" >/dev/null || { diff "$W/p.snap" "$W/s.snap" >&2; fail "standby differs from primary"; }
  [ -z "$(qp $P -c "SELECT * FROM mismatches()")" ] || fail "primary index differs from seqscan"
  [ -z "$(qp $S -c "SELECT * FROM mismatches()")" ] || fail "standby index differs from seqscan"
}

# Queries the standby while it replays: index vs seqscan in one snapshot.
standby_checker() {
  n=0; bad=0; cancelled=0
  while [ ! -f "$W/stop.checker" ]; do
    out=$(qp $S -c "BEGIN ISOLATION LEVEL REPEATABLE READ" -c "SELECT count(*) FROM mismatches()" -c "COMMIT" 2>&1) || {
      case "$out" in *"conflict with recovery"*) cancelled=$((cancelled + 1)); continue ;; esac
      [ -d "$W/standby" ] || break
      echo "checker error: $out" >&2; bad=$((bad + 1)); continue; }
    n=$((n + 1)); [ "$out" = 0 ] || { bad=$((bad + 1)); echo "standby mismatch during replay: $out" >&2; }
  done
  echo "$n $bad $cancelled" > "$W/checker.result"
}

round() { # port, label
  port=$1
  ( while [ ! -f "$W/stop.vacuum" ]; do qp $port -c "VACUUM t" >/dev/null 2>&1 || true; sleep 2; done ) &
  vpid=$!
  (cd "$STRESS" && "$BIN/pgbench" -h "$W" -p $port -U postgres -n -c 4 -j 4 -T $SECONDS_PER_ROUND \
     -f write.sql@3 -f read.sql@1 postgres > "$W/pgbench.out" 2>&1) || { cat "$W/pgbench.out" >&2; fail "pgbench"; }
  touch "$W/stop.vacuum"; wait $vpid; rm -f "$W/stop.vacuum"
  grep -E "^tps|failed transactions" "$W/pgbench.out" | head -2 | sed 's/^/   /'
}

echo "== rounds on the primary, standby queried during replay"
rm -f "$W/stop.checker"
standby_checker & cpid=$!
r=1
while [ $r -le $ROUNDS ]; do
  echo "-- round $r"
  round $P
  compare_standby
  echo "   standby == primary, both == seqscan; $(qp $P -c "SELECT segments || ' segments, ' || pending_tuples || ' pending' FROM tin_stats('t_body_tin'::regclass)")"
  if [ $r -eq 2 ]; then
    qp $P -c "SELECT * FROM snapshot()" > "$W/restore_point.snap"
    qp $P -c "SELECT pg_create_restore_point('mid')" -c "SELECT pg_switch_wal()" >/dev/null
    echo "   restore point 'mid' created"
  fi
  if [ $r -eq 3 ]; then
    qp $P -c "REINDEX INDEX CONCURRENTLY t_body_tin"
    compare_standby
    echo "   REINDEX CONCURRENTLY replayed; standby == primary"
  fi
  r=$((r + 1))
done
touch "$W/stop.checker"; wait $cpid
read n bad cancelled < "$W/checker.result"
echo "   standby checks during replay: $n, mismatches: $bad, cancelled by replay conflicts: $cancelled"
[ "$bad" = 0 ] || fail "standby mismatches during replay"
[ "$n" -gt 0 ] || fail "no standby checks ran"

echo "== point-in-time recovery to 'mid'"
qp $P -c "SELECT pg_switch_wal()" >/dev/null
sleep 1
sed -i "s/^port = $P/port = $R/" "$W/pitr/postgresql.conf"
sed -i "s/^archive_mode = on/archive_mode = off/" "$W/pitr/postgresql.conf"
cat >> "$W/pitr/postgresql.conf" <<CONF
restore_command = 'cp $W/archive/%f %p'
recovery_target_name = 'mid'
recovery_target_action = 'promote'
CONF
$AS touch "$W/pitr/recovery.signal"
$AS "$BIN/pg_ctl" -D "$W/pitr" -l "$W/pitr.log" -w -t 600 start >/dev/null
i=0; until [ "$(qp $R -c "SELECT NOT pg_is_in_recovery()")" = t ]; do i=$((i + 1)); [ $i -gt 600 ] && fail "PITR did not finish"; sleep 0.5; done
qp $R -c "SELECT * FROM snapshot()" > "$W/pitr.snap"
diff "$W/restore_point.snap" "$W/pitr.snap" >/dev/null || { diff "$W/restore_point.snap" "$W/pitr.snap" >&2; fail "PITR differs from the primary at the restore point"; }
[ -z "$(qp $R -c "SELECT * FROM mismatches()")" ] || fail "PITR index differs from seqscan"
echo "   restored cluster == primary at the restore point, and == seqscan"

echo "== promote the standby, write to it"
$AS "$BIN/pg_ctl" -D "$W/standby" -w promote >/dev/null
$AS "$BIN/pg_ctl" -D "$W/primary" -m fast stop >/dev/null
round $S
qp $S -c "VACUUM t" >/dev/null
[ -z "$(qp $S -c "SELECT * FROM mismatches()")" ] || fail "promoted standby index differs from seqscan"
echo "   promoted standby == seqscan after writes and VACUUM"
echo "PASS: replication, point-in-time recovery and promotion"
