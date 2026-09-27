# Audit event retention runbook

This runbook is for the `audit_events` retention policy named by
`observability.audit_retention_days` (default: 365 days; valid range: 1..3650). It is **not** the
source-IP retention policy. `retention.source_ip_days` deletes only rows in
`audit_source_ips`; it does not delete audit events. Deleting an audit event
may cascade its remaining source-IP row through the foreign key, so source-IP
cleanup should normally run first.

## Database boundary

`audit_events` is append-only for the application runtime role:

- `orbisync_runtime` retains `SELECT, INSERT` only; it has no `UPDATE` or
  `DELETE` privilege on `audit_events`.
- Migration `0014_audit_retention_operator.sql` creates the `NOLOGIN`
  `orbisync_audit_maintenance` role. It has no table privileges and can only
  execute the validation, archive, purge, and status functions in `audit_operations`.
- An operator login must be separately provisioned as a member of the
  maintenance role by the DBA (or connect with an explicit `SET ROLE` to it).
  Do not add this membership to the runtime login, and do not make the
  maintenance role `SUPERUSER`, `CREATEROLE`, or `CREATEDB`.

The DBA grants membership to a dedicated login, for example
`GRANT orbisync_audit_maintenance TO audit_operator;`; the login must not be
the application login and must not receive direct table privileges.

The migration is additive. It does not convert the existing table to a
partitioned table or change the runtime grants.

## One manual run

Use a read-only export URL and a separate URL authenticated as an operator
login that has only the maintenance-role membership. Never use the runtime URL
for the purge connection.

```bash
export AUDIT_EXPORT_DATABASE_URL='postgres://audit_reader:...@db/orbisync'
export AUDIT_RETENTION_DATABASE_URL='postgres://audit_operator:...@db/orbisync'
mkdir -m 0700 -p /var/lib/orbisync/audit-archives

# Count only. This prints the exact database cutoff and eligible row count.
bash scripts/audit-retention.sh \
  --retention-days 365 \
  --dry-run

# Execute with a durable, access-controlled archive directory.
bash scripts/audit-retention.sh \
  --retention-days 365 \
  --archive-dir /var/lib/orbisync/audit-archives \
  --batch-size 1000
```

Pass the validated value of `observability.audit_retention_days`; the command
prints and records the exact cutoff used. A cutoff can be made explicit for a
change ticket or replay:

```bash
bash scripts/audit-retention.sh \
  --retention-days 365 \
  --cutoff '2026-09-02 00:00:00+00' \
  --archive-dir /var/lib/orbisync/audit-archives
```

The command is safe to interrupt. Each database call deletes at most one
batch (1..10000 rows), takes the transaction-scoped retention advisory lock,
and uses `ORDER BY occurred_at, id` plus `FOR UPDATE SKIP LOCKED`. It holds a
five-second lock timeout and a 30-second statement timeout, and the next
invocation resumes from the recorded archive marker. A failed transaction
rolls back both the bounded DELETE and its marker update. The archive
manifest contains the explicit cutoff, row count and SHA-256 of the JSONL
export. The database records the verified manifest before allowing any delete.
If the export count, cutoff, archive marker, or row count changes, the command
fails closed and deletes nothing for that batch. A second call after completion
returns zero deleted rows for the same archive ID.

`--dry-run`/`--count-only` never writes an archive and never calls a mutation
function. Keep the JSONL file and its `.manifest` in the backup-managed archive
volume; verify the checksum and retention ticket before removing either file.
The export can contain audit metadata and must have the same access controls as
the database backup.

## Scheduler and monitoring

The production scheduler is a host `systemd` timer, once daily at **02:15 UTC**
on the database maintenance host. Render the configured
`observability.audit_retention_days` value into the service environment; do not
hard-code a different policy in the timer. A representative local unit is:

```ini
# /etc/systemd/system/orbisync-audit-retention.service
[Service]
Type=oneshot
User=orbisync-audit-operator
EnvironmentFile=/etc/orbisync/audit-retention.env
ExecStart=/usr/local/bin/orbisync-audit-retention
```

```ini
# /etc/systemd/system/orbisync-audit-retention.timer
[Timer]
OnCalendar=*-*-* 02:15:00 UTC
Persistent=true
RandomizedDelaySec=10m
Unit=orbisync-audit-retention.service

[Install]
WantedBy=timers.target
```

`/usr/local/bin/orbisync-audit-retention` must invoke the checked-in script
with `--retention-days "$ORBISYNC_AUDIT_RETENTION_DAYS"` and the configured
archive directory. Credentials come from the service environment file or
secret manager, never command-line arguments or the archive manifest.

Monitor the service exit status and its structured lines containing `cutoff`,
`eligible_rows`, `deleted_batch`, and `deleted_total`. Alert when:

1. the timer has no successful completion in 26 hours;
2. any run exits non-zero (including lock timeout, count mismatch, or missing
   archive verification); or
3. the archive volume is unavailable, read-only, below its capacity threshold,
   or its manifest checksum cannot be verified.

The `OnFailure` unit should page the database on-call. Do not automatically
retry a count mismatch: preserve the export and manifest, investigate the
concurrent writer or clock/cutoff issue, then rerun with an explicit cutoff.
An alert for a lock timeout is availability protection, not permission to
grant the runtime role more privileges.

## Failure handling and restore drill

On failure, retain the archive and manifest, capture the service log and
archive ID, and verify that no rows were deleted from the failed transaction.
For a missing or corrupt archive, stop the purge and restore the PostgreSQL
backup/archive into an isolated database. Never restore over the production
database as part of this procedure. The incident commander approves any
manual retry after the row count and SHA-256 have been rechecked.

Quarterly, and after a migration that changes audit columns, perform a restore
drill in an isolated database:

1. restore the latest backup using `scripts/restore-drill.sh`;
2. apply the checked-in migrations and confirm the `0014` functions and role;
3. verify one retained event, one cutoff-boundary event, and one archived
   JSONL record against the manifest count and SHA-256; and
4. run `--dry-run` against the isolated database, then discard the database.

The drill proves that retention does not depend on the live runtime process and
that the archive is usable before production deletion. It is not a test against
production data.

## Future partition migration (separate change)

Partitioning is intentionally not part of this migration. If event volume
requires it, first add a new partitioned shadow table, dual-write and compare
counts, backfill in time-bounded batches, pause writes for a final consistency
check, then attach/rename during a reviewed maintenance window. Keep the
operator functions and runtime grants unchanged until the shadow table is
validated and the old table is archived. A separate expand/migrate/contract
migration and rollback/restore rehearsal is required.
