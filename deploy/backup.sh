#!/usr/bin/env bash
# Dump the cortex database to /var/backups/cortex, keep the newest 14, and prove
# the dump is readable. Run by cortex-backup.timer; restore-test.sh proves a restore.
set -euo pipefail
DIR=${BACKUP_DIR:-/var/backups/cortex}
DB=${CORTEX_DB:-cortex}
KEEP=${BACKUP_KEEP:-14}
stamp=$(date -u +%Y%m%dT%H%M%SZ)
tmp=$DIR/.cortex-$stamp.dump.partial
out=$DIR/cortex-$stamp.dump
umask 077
pg_dump -h 127.0.0.1 -Fc -d "$DB" -f "$tmp"
pg_restore --list "$tmp" >/dev/null
mv "$tmp" "$out"
ln -sfn "$out" "$DIR/cortex.dump"
ls -1t "$DIR"/cortex-*.dump | tail -n +$((KEEP + 1)) | xargs -r rm -f
echo "backup: $out $(stat -c %s "$out") bytes"
