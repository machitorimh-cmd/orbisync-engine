#!/usr/bin/env bash
# Export and purge audit_events through the operator-only retention API.
#
# The export connection is read-only (normally the runtime role).  The
# maintenance connection must be a separate credential for
# orbisync_audit_maintenance; it cannot delete audit_events directly.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/audit-retention.sh --retention-days DAYS [options]

Options:
  --retention-days DAYS  Value of observability.audit_retention_days (required)
  --cutoff TIMESTAMP     Explicit UTC/RFC3339 cutoff; otherwise DB clock is used
  --batch-size N          Rows per maintenance call (default: 1000, max: 10000)
  --archive-dir DIR       Directory for the verified JSONL export and manifest
  --dry-run               Print the cutoff and eligible count, do not export/delete
  --count-only            Alias for --dry-run (count without any mutation)
  -h, --help              Show this help

Required environment:
  AUDIT_RETENTION_DATABASE_URL  Explicit operator-login URL; never runtime URL
  AUDIT_EXPORT_DATABASE_URL     Read-only URL used to create the archive
  (DATABASE_URL is accepted only as the export URL for local operation.)
EOF
}

retention_days=""
cutoff=""
batch_size=1000
archive_dir=""
dry_run=0

while (($# > 0)); do
  case "$1" in
    --retention-days)
      (($# >= 2)) || { echo "error: --retention-days needs a value" >&2; exit 2; }
      retention_days="$2"; shift 2 ;;
    --cutoff)
      (($# >= 2)) || { echo "error: --cutoff needs a value" >&2; exit 2; }
      cutoff="$2"; shift 2 ;;
    --batch-size)
      (($# >= 2)) || { echo "error: --batch-size needs a value" >&2; exit 2; }
      batch_size="$2"; shift 2 ;;
    --archive-dir)
      (($# >= 2)) || { echo "error: --archive-dir needs a value" >&2; exit 2; }
      archive_dir="$2"; shift 2 ;;
    --dry-run|--count-only)
      dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Keep the accepted ranges identical to the config and database function.
# The bounded digit patterns also prevent shell arithmetic overflow.
[[ "$retention_days" =~ ^[1-9][0-9]{0,3}$ ]] && ((retention_days <= 3650)) || {
  echo "error: --retention-days must be in 1..3650" >&2; exit 2;
}
[[ "$batch_size" =~ ^[1-9][0-9]{0,4}$ ]] && ((batch_size <= 10000)) || {
  echo "error: --batch-size must be in 1..10000" >&2; exit 2;
}

export_url="${AUDIT_EXPORT_DATABASE_URL:-${DATABASE_URL:-}}"
maintenance_url="${AUDIT_RETENTION_DATABASE_URL:-}"
[[ -n "$export_url" ]] || { echo "error: export database URL is not set" >&2; exit 2; }
[[ -n "$maintenance_url" ]] || {
  echo "error: AUDIT_RETENTION_DATABASE_URL must contain an explicit operator credential" >&2
  exit 2
}
command -v psql >/dev/null 2>&1 || { echo "error: psql is required" >&2; exit 2; }

psql_query() {
  local url="$1" query="$2"
  shift 2
  psql "$url" -X -q -A -t -v ON_ERROR_STOP=1 "$@" -c "$query"
}

# Prove the maintenance credential has the operator-only capability before
# doing even a count. This rejects a runtime URL and avoids SET ROLE here.
operator_capability="$(psql_query "$maintenance_url" "SELECT has_function_privilege(current_user, 'audit_operations.validate_retention_days(integer)'::regprocedure, 'EXECUTE');")"
[[ "$operator_capability" == "t" ]] || {
  echo "error: AUDIT_RETENTION_DATABASE_URL is not an audit operator credential" >&2
  exit 2
}
psql_query "$maintenance_url" \
  "SELECT audit_operations.validate_retention_days(${retention_days});" >/dev/null

if [[ -z "$cutoff" ]]; then
  cutoff="$(psql_query "$export_url" "SELECT clock_timestamp() - make_interval(days => ${retention_days});")"
else
  # Let PostgreSQL parse the explicit cutoff and print its canonical value.
  cutoff="$(psql_query "$export_url" "SELECT :'cutoff'::timestamptz;" --set=cutoff="$cutoff")"
fi
[[ -n "$cutoff" ]] || { echo "error: database returned an empty cutoff" >&2; exit 1; }

eligible="$(psql_query "$export_url" "SELECT count(*) FROM public.audit_events WHERE occurred_at < :'cutoff'::timestamptz;" --set=cutoff="$cutoff")"
eligible_oldest="$(psql_query "$export_url" "SELECT COALESCE(min(occurred_at)::text, 'none') FROM public.audit_events WHERE occurred_at < :'cutoff'::timestamptz;" --set=cutoff="$cutoff")"
echo "audit retention cutoff=${cutoff} eligible_rows=${eligible} eligible_oldest=${eligible_oldest} retention_days=${retention_days}"

if ((dry_run)); then
  exit 0
fi
[[ -n "$maintenance_url" ]] || { echo "error: AUDIT_RETENTION_DATABASE_URL is not set" >&2; exit 2; }
[[ -n "$archive_dir" ]] || { echo "error: --archive-dir is required unless --dry-run is used" >&2; exit 2; }
mkdir -p "$archive_dir"

archive_id="$(psql_query "$export_url" 'SELECT gen_random_uuid();')"
archive_file="${archive_dir%/}/audit-events-${archive_id}.jsonl"
manifest_file="${archive_file}.manifest"

# JSONL is used so each row has one physical line; PostgreSQL JSON escapes
# embedded newlines in metadata.  The query is read-only and ordered for a
# reproducible export.
psql "$export_url" -X -q -A -t -v ON_ERROR_STOP=1 \
  --set=cutoff="$cutoff" \
  -c "SELECT row_to_json(a)::text FROM public.audit_events AS a WHERE a.occurred_at < :'cutoff'::timestamptz ORDER BY a.occurred_at, a.id;" \
  > "$archive_file"

exported_rows="$(wc -l < "$archive_file" | tr -d '[:space:]')"
[[ "$exported_rows" == "$eligible" ]] || {
  echo "error: export row count mismatch (query=${eligible}, file=${exported_rows}); refusing deletion" >&2
  exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
  sha256="$(sha256sum "$archive_file" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  sha256="$(shasum -a 256 "$archive_file" | awk '{print $1}')"
else
  echo "error: sha256sum or shasum is required to verify the archive" >&2
  exit 1
fi
printf 'archive_id=%s\ncutoff=%s\nexported_rows=%s\nsha256=%s\n' \
  "$archive_id" "$cutoff" "$exported_rows" "$sha256" > "$manifest_file"

# This call is the fail-closed archive marker. The database rechecks the
# count while holding the retention advisory lock before accepting the marker.
psql_query "$maintenance_url" \
  "SELECT audit_operations.record_verified_archive(:'archive_id'::uuid, :'cutoff'::timestamptz, :'retention_days'::integer, :'exported_rows'::bigint, :'sha256');" \
  --set=archive_id="$archive_id" --set=cutoff="$cutoff" \
  --set=retention_days="$retention_days" --set=exported_rows="$exported_rows" --set=sha256="$sha256" >/dev/null

deleted_total=0
while :; do
  deleted="$(psql_query "$maintenance_url" \
    "SELECT audit_operations.purge_audit_events(:'archive_id'::uuid, ${batch_size});" \
    --set=archive_id="$archive_id")"
  [[ "$deleted" =~ ^[0-9]+$ ]] || { echo "error: invalid purge result" >&2; exit 1; }
  deleted_total=$((deleted_total + deleted))
  echo "audit retention archive_id=${archive_id} deleted_batch=${deleted} deleted_total=${deleted_total}"
  ((deleted == 0 || deleted < batch_size)) && break
done
remaining_count="$(psql_query "$maintenance_url" \
  "SELECT remaining_count FROM audit_operations.retention_status(:'"'"'archive_id'"'"'::uuid);" \
  --set=archive_id="$archive_id")"
remaining_oldest="$(psql_query "$maintenance_url" \
  "SELECT COALESCE(remaining_oldest::text, 'none') FROM audit_operations.retention_status(:'"'"'archive_id'"'"'::uuid);" \
  --set=archive_id="$archive_id")"
echo "audit retention completed archive_id=${archive_id} archive=${archive_file} manifest=${manifest_file} remaining_count=${remaining_count} remaining_oldest=${remaining_oldest}"
