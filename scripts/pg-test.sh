#!/usr/bin/env sh
# Build + install pg_tin, start a throwaway cluster and run the SQL regression
# test, diffing against the expected output.
#   PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config sh scripts/pg-test.sh
# Set TIN_ACCEPT=1 to overwrite the expected output.
set -eu
PG_CONFIG=${PG_CONFIG:-pg_config}
BIN=$("$PG_CONFIG" --bindir)
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SQL="$ROOT/crates/pg_tin/tests/sql/smoke.sql"
EXPECTED="$ROOT/crates/pg_tin/tests/sql/smoke.out"

(cd "$ROOT/crates/pg_tin" && cargo pgrx install --release --pg-config "$PG_CONFIG" >/dev/null)

DIR=$(mktemp -d)
PORT=${PORT:-54329}
# Postgres refuses to run as root; use the postgres user when we are root.
AS=""
if [ "$(id -u)" = 0 ]; then AS="runuser -u postgres --"; chown postgres "$DIR"; fi
# Pin encoding + collation, and keep autovacuum from flushing pending lists
# at random points, so the output is the same on every machine.
$AS "$BIN/initdb" -D "$DIR/data" -A trust -U postgres -E UTF8 --locale=C >/dev/null
$AS "$BIN/pg_ctl" -D "$DIR/data" -o "-p $PORT -k $DIR -c autovacuum=off" -l "$DIR/log" -w start >/dev/null
trap '$AS "$BIN/pg_ctl" -D "$DIR/data" -m immediate stop >/dev/null; rm -rf "$DIR"' EXIT

# Read from stdin so error lines don't embed this machine's path.
"$BIN/psql" -h "$DIR" -p "$PORT" -U postgres -X -a -q < "$SQL" > "$DIR/actual.out" 2>&1 || true
if [ "${TIN_ACCEPT:-0}" = 1 ]; then
  cp "$DIR/actual.out" "$EXPECTED"
  echo "accepted new expected output"
elif diff -u "$EXPECTED" "$DIR/actual.out"; then
  echo "pg_tin SQL tests passed"
else
  echo "pg_tin SQL tests FAILED" >&2
  exit 1
fi
