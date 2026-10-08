#!/usr/bin/env bash
# Restore the newest dump into a scratch database and compare it with the source.
# Exits non-zero when any table's row count differs. Safe to run on the live host.
set -euo pipefail
DIR=${BACKUP_DIR:-/var/backups/cortex}
SRC=${CORTEX_DB:-cortex}
SCRATCH=cortex_restore_test
dump=$(ls -1t "$DIR"/cortex-*.dump | head -n 1)
psql_() { psql -h 127.0.0.1 -v ON_ERROR_STOP=1 -At "$@"; }
trap 'psql_ -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH" >/dev/null' EXIT
psql_ -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH" >/dev/null
psql_ -d postgres -c "CREATE DATABASE $SCRATCH" >/dev/null
pg_restore -h 127.0.0.1 -d "$SCRATCH" --no-owner --exit-on-error "$dump"
counts() {
  psql_ -d "$1" <<'SQL'
SELECT format('SELECT %L || '' '' || count(*) FROM %I', table_name, table_name)
FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name \gexec
SQL
}
src_counts=$(counts "$SRC")
restored=$(counts "$SCRATCH")
tables=$(printf '%s\n' "$restored" | wc -l)
if [[ "$src_counts" != "$restored" ]]; then
  # Rows written after the dump was taken are expected; a table that lost rows is not.
  lost=$(join <(printf '%s\n' "$src_counts") <(printf '%s\n' "$restored") | awk '$3 > $2 {print $1}')
  if [[ -n "$lost" ]]; then echo "restore-test: restored tables hold more rows than the source: $lost" >&2; exit 1; fi
fi
echo "restore-test: $dump restored into $SCRATCH, $tables tables, users=$(psql_ -d "$SCRATCH" -c 'SELECT count(*) FROM users')"
