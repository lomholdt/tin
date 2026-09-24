#!/usr/bin/env sh
# Build + install pg_tin, start a throwaway cluster and run the SQL regression
# test, diffing against the expected output; then check that upgrading from
# the previous version (ALTER EXTENSION pg_tin UPDATE) gives the same
# catalog as a fresh install, with existing indexes still working.
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

# Upgrade: install the previous version's script (a test fixture; releases
# ship only upgrade scripts), upgrade, compare with a fresh install.
UP="$ROOT/crates/pg_tin/tests/upgrade"
cp "$UP/pg_tin--0.1.0.sql" "$("$PG_CONFIG" --sharedir)/extension/"
PSQL="$BIN/psql -h $DIR -p $PORT -U postgres -X -q"
$PSQL -c "CREATE DATABASE fresh" -c "CREATE DATABASE upgraded"
$PSQL -d fresh -c "CREATE EXTENSION pg_tin"
$PSQL -d upgraded -a < "$UP/upgrade.sql" > "$DIR/upgrade.out" 2>&1
$PSQL -d fresh < "$UP/catalog.sql" > "$DIR/fresh.cat"
$PSQL -d upgraded < "$UP/catalog.sql" > "$DIR/upgraded.cat"
if ! diff -u "$DIR/fresh.cat" "$DIR/upgraded.cat"; then
  echo "pg_tin upgrade FAILED: catalog differs from a fresh install" >&2
  exit 1
fi
if [ "${TIN_ACCEPT:-0}" = 1 ]; then
  cp "$DIR/upgrade.out" "$UP/upgrade.out"
elif ! diff -u "$UP/upgrade.out" "$DIR/upgrade.out"; then
  echo "pg_tin upgrade FAILED: queries after the upgrade" >&2
  exit 1
fi
echo "pg_tin upgrade test passed"
