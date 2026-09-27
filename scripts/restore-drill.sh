#!/usr/bin/env bash
# Automated restore drill for MVP condition 14.
#
# Implements the same commands as docs/operations/backup-restore.md
# Backup / Restore drill (pg_dump --format=custom --no-owner --no-acl
# and pg_restore --clean --if-exists --no-owner) and verifies:
#   1. migration version matches
#   2. table row counts match
#   3. bootstrapped administrator can actually log in on the restored DB (D-17: password via file, never logged)
#   4. isolated restore database is used (D-16: never restores into the source DB)
#
# Usage:
#   bash scripts/restore-drill.sh
#   RESTORE_DRILL_BACKUP_FILE=/secure/backup.dump \
#   RESTORE_DRILL_BACKUP_SHA256=<sha256> \
#   RESTORE_DRILL_LOGIN_ID=restore_smoke \
#   RESTORE_DRILL_PASSWORD_FILE=/secure/restore-smoke-password \
#     bash scripts/restore-drill.sh
#
# Environment:
#   Uses port 55432 and container orbisync-restore-drill-w19-32 to avoid
#   colliding with development PostgreSQL (5432) and other workers.
#   Set RESTORE_DRILL_CONTAINER / RESTORE_DRILL_PORT to override.
#   Set RESTORE_DRILL_POSTGRES_IMAGE when verifying a backup created by a
#   different supported PostgreSQL major version.
#   Requires docker, cargo, curl.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CONTAINER_NAME="${RESTORE_DRILL_CONTAINER:-orbisync-restore-drill-w19-32}"
PORT="${RESTORE_DRILL_PORT:-55432}"
POSTGRES_IMAGE="${RESTORE_DRILL_POSTGRES_IMAGE:-postgres:16}"
POSTGRES_USER="orbisync"
POSTGRES_PASSWORD="orbisync"
POSTGRES_DB="orbisync"
RESTORE_DB="orbisync_restore"
EXTERNAL_BACKUP_FILE="${RESTORE_DRILL_BACKUP_FILE:-}"
EXTERNAL_BACKUP_SHA256="${RESTORE_DRILL_BACKUP_SHA256:-}"
EXTERNAL_PASSWORD_FILE="${RESTORE_DRILL_PASSWORD_FILE:-}"
EXTERNAL_MODE=0
if [ -n "${EXTERNAL_BACKUP_FILE}" ]; then
  EXTERNAL_MODE=1
fi
ADMIN_LOGIN_ID="${RESTORE_DRILL_LOGIN_ID:-restore_drill_admin}"
ADMIN_DISPLAY_NAME="Restore Drill Admin"
# Use workspace-relative temp files to avoid MSYS /tmp -> C:\Program Files\Git\tmp translation
# issues when invoking Windows Docker/PG binaries from Git Bash.
TMP_DIR="$(mktemp -d)"
DUMP_FILE="${TMP_DIR}/restore.dump"
DENYLIST_FILE="${TMP_DIR}/denylist.txt"
PASSWORD_FILE="${TMP_DIR}/password.txt"
SERVER_BIND="127.0.0.1:18080"
SERVER_LOG="${TMP_DIR}/server.log"
SERVER_PID=""
DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@localhost:${PORT}/${POSTGRES_DB}"
RESTORE_DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@localhost:${PORT}/${RESTORE_DB}"

cleanup() {
  local exit_code=$?
  set +e
  if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  # Do not print exit_code here to avoid leaking secrets; just clean up.
  docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  rm -rf "${TMP_DIR}" 2>/dev/null || true
  # Preserve the original exit code; if we are in a trap from set -e, exit non-zero is kept.
  exit $exit_code
}
trap cleanup EXIT INT TERM

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: $1 is required but not installed" >&2
    exit 1
  fi
}

require docker
require cargo
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
  echo "error: curl or wget is required" >&2
  exit 1
fi

if [ "${EXTERNAL_MODE}" = "1" ]; then
  if [ ! -s "${EXTERNAL_BACKUP_FILE}" ]; then
    echo "error: RESTORE_DRILL_BACKUP_FILE must name a non-empty custom-format dump" >&2
    exit 1
  fi
  if [ -z "${RESTORE_DRILL_LOGIN_ID:-}" ] || [ ! -s "${EXTERNAL_PASSWORD_FILE}" ]; then
    echo "error: external backup mode requires RESTORE_DRILL_LOGIN_ID and a non-empty RESTORE_DRILL_PASSWORD_FILE" >&2
    exit 1
  fi
  if [ -n "${EXTERNAL_BACKUP_SHA256}" ] && ! printf '%s' "${EXTERNAL_BACKUP_SHA256}" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    echo "error: RESTORE_DRILL_BACKUP_SHA256 must contain exactly 64 hexadecimal characters" >&2
    exit 1
  fi
fi

http_get() {
  local url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -sf "$url" >/dev/null 2>&1
  else
    wget -qO- "$url" >/dev/null 2>&1
  fi
}

# 0. Generate password denylist (exactly 10,000 distinct lines, required by PasswordPolicy::production)
echo "==> generating password denylist (10,000 entries)"
# Use deterministic distinct lines that do not contain banned substrings.
for i in $(seq 1 10000); do
  printf "denylist-entry-%05d-xyz-%08d\n" "$i" "$i"
done > "${DENYLIST_FILE}"
if [ "$(wc -l < "${DENYLIST_FILE}")" != "10000" ]; then
  echo "error: denylist generation failed" >&2
  exit 1
fi

# 1. Start PostgreSQL container on 55432 and wait until healthy.
echo "==> starting PostgreSQL container ${CONTAINER_NAME} on port ${PORT}"
docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
DOCKER_LABEL_ARGS=()
if [ -n "${AO_SESSION_ID:-}" ]; then
  DOCKER_LABEL_ARGS+=(--label "ao.session=${AO_SESSION_ID}")
fi
docker run -d \
  --name "${CONTAINER_NAME}" \
  "${DOCKER_LABEL_ARGS[@]}" \
  -p "${PORT}:5432" \
  -e POSTGRES_USER="${POSTGRES_USER}" \
  -e POSTGRES_PASSWORD="${POSTGRES_PASSWORD}" \
  -e POSTGRES_DB="${POSTGRES_DB}" \
  "${POSTGRES_IMAGE}" >/dev/null

echo "==> waiting for PostgreSQL to accept connections"
for _ in $(seq 1 60); do
  if docker exec "${CONTAINER_NAME}" pg_isready -U "${POSTGRES_USER}" >/dev/null 2>&1; then
    break
  fi
  sleep 1
  if [ "$_" = "60" ]; then
    echo "error: PostgreSQL did not become ready within 60 seconds" >&2
    docker logs "${CONTAINER_NAME}" >&2 || true
    exit 1
  fi
done

# Generate ephemeral Ed25519 signing key (not logged, not on argv) for this drill.
if ! command -v openssl >/dev/null 2>&1; then
  echo "error: openssl is required to generate Ed25519 key" >&2
  exit 1
fi
RESTORE_KEY_FILE="${TMP_DIR}/ed25519.pem"
openssl genpkey -algorithm ed25519 -out "${RESTORE_KEY_FILE}" 2>/dev/null
RESTORE_TOKEN_KEY="$(awk '{printf "%s\\n", $0}' "${RESTORE_KEY_FILE}")"
rm -f "${RESTORE_KEY_FILE}"
RESTORE_HMAC_KEY="ci-only-pagination-hmac-key-for-restore-drill"
RESTORE_REFRESH_HMAC_KEY="ci-only-refresh-hmac-key-for-restore-drill"
RESTORE_REALTIME_TICKET_HMAC_KEY="ci-only-realtime-ticket-hmac-key-for-restore-drill"
RESTORE_IDEMPOTENCY_HMAC_KEY="ci-only-idempotency-hmac-key-for-restore-drill"
export ORBISYNC_EXTENSION_TOKEN_HMAC_KEY="ci-only-extension-hmac-key-for-restore-drill"

# 2-4. Build a fresh fixture backup, or stage an operator-supplied backup.
if [ "${EXTERNAL_MODE}" = "0" ]; then
# Apply migrations (same command as runbook / CI)
echo "==> applying migrations (cargo run -p orbisync-server -- migrate)"
DATABASE_URL="${DATABASE_URL}" ORBISYNC_TOKEN_SIGNING_KEY="${RESTORE_TOKEN_KEY}" ORBISYNC_PAGINATION_HMAC_KEY="${RESTORE_HMAC_KEY}" ORBISYNC_REFRESH_TOKEN_HMAC_KEY="${RESTORE_REFRESH_HMAC_KEY}" ORBISYNC_REALTIME_TICKET_HMAC_KEY="${RESTORE_REALTIME_TICKET_HMAC_KEY}" ORBISYNC_IDEMPOTENCY_HMAC_KEY="${RESTORE_IDEMPOTENCY_HMAC_KEY}" \
  cargo run --quiet -p orbisync-server -- migrate

# Seed data: bootstrap-admin and capture temporary password via file (D-17: never log it)
echo "==> bootstrapping administrator via bootstrap-admin --password-output (D-17)"
# Remove any stale password file so we verify creation.
rm -f "${PASSWORD_FILE}"
DATABASE_URL="${DATABASE_URL}" ORBISYNC_TOKEN_SIGNING_KEY="${RESTORE_TOKEN_KEY}" ORBISYNC_PAGINATION_HMAC_KEY="${RESTORE_HMAC_KEY}" ORBISYNC_REFRESH_TOKEN_HMAC_KEY="${RESTORE_REFRESH_HMAC_KEY}" ORBISYNC_REALTIME_TICKET_HMAC_KEY="${RESTORE_REALTIME_TICKET_HMAC_KEY}" ORBISYNC_IDEMPOTENCY_HMAC_KEY="${RESTORE_IDEMPOTENCY_HMAC_KEY}" \
  cargo run --quiet -p orbisync-server -- bootstrap-admin \
    --login-id "${ADMIN_LOGIN_ID}" \
    --display-name "${ADMIN_DISPLAY_NAME}" \
    --password-denylist "${DENYLIST_FILE}" \
    --password-output "${PASSWORD_FILE}"

if [ ! -s "${PASSWORD_FILE}" ]; then
  echo "error: bootstrap-admin did not create password file" >&2
  exit 1
fi
# Ensure password is not printed to stdout/logs; we only check length (27 chars per CreatedUserCredential).
PASSWORD_LEN=$(tr -d '\r\n' < "${PASSWORD_FILE}" | wc -c)
if [ "${PASSWORD_LEN}" != "27" ]; then
  echo "error: unexpected temporary password length: ${PASSWORD_LEN} (expected 27)" >&2
  exit 1
fi
echo "==> administrator bootstrapped (temporary password captured to file, not logged)"

# Backup with pg_dump --format=custom --no-owner --no-acl (same flags as runbook)
# Always use docker exec to avoid host pg_dump path-translation issues on Windows/Git Bash
# and to ensure the exact same binary as the server uses.
echo "==> taking backup with pg_dump --format=custom --no-owner --no-acl"
MSYS_NO_PATHCONV=1 docker exec "${CONTAINER_NAME}" pg_dump -U "${POSTGRES_USER}" --format=custom --no-owner --no-acl -d "${POSTGRES_DB}" -f /tmp/backup.dump
docker cp "${CONTAINER_NAME}:/tmp/backup.dump" "${DUMP_FILE}"
if [ ! -s "${DUMP_FILE}" ]; then
  echo "error: pg_dump produced empty file" >&2
  exit 1
fi
# SHA-256 checksum (same as runbook step 4)
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${DUMP_FILE}" | awk '{print $1}' > "${DUMP_FILE}.sha256"
  echo "==> backup SHA-256: $(cat "${DUMP_FILE}.sha256")"
elif command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "${DUMP_FILE}" | awk '{print $1}' > "${DUMP_FILE}.sha256"
  echo "==> backup SHA-256: $(cat "${DUMP_FILE}.sha256")"
fi
else
  echo "==> staging operator-supplied backup for isolated verification"
  cp -- "${EXTERNAL_BACKUP_FILE}" "${DUMP_FILE}"
  cp -- "${EXTERNAL_PASSWORD_FILE}" "${PASSWORD_FILE}"
  if [ ! -s "${DUMP_FILE}" ] || [ ! -s "${PASSWORD_FILE}" ]; then
    echo "error: failed to stage external backup or smoke-test password file" >&2
    exit 1
  fi
  ACTUAL_BACKUP_SHA256=""
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_BACKUP_SHA256=$(sha256sum "${DUMP_FILE}" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_BACKUP_SHA256=$(shasum -a 256 "${DUMP_FILE}" | awk '{print $1}')
  elif [ -n "${EXTERNAL_BACKUP_SHA256}" ]; then
    echo "error: sha256sum or shasum is required to verify the supplied checksum" >&2
    exit 1
  fi
  if [ -n "${EXTERNAL_BACKUP_SHA256}" ] && [ "${ACTUAL_BACKUP_SHA256}" != "${EXTERNAL_BACKUP_SHA256,,}" ]; then
    echo "error: supplied backup checksum does not match" >&2
    exit 1
  fi
  if [ -n "${ACTUAL_BACKUP_SHA256}" ]; then
    echo "==> supplied backup SHA-256: ${ACTUAL_BACKUP_SHA256}"
  fi
fi

# 5. Create isolated database and restore with pg_restore --clean --if-exists --no-owner (D-16)
echo "==> creating isolated restore database ${RESTORE_DB} (D-16: never restores into source DB)"
MSYS_NO_PATHCONV=1 docker exec "${CONTAINER_NAME}" psql -U "${POSTGRES_USER}" -d postgres -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS ${RESTORE_DB};" >/dev/null
MSYS_NO_PATHCONV=1 docker exec "${CONTAINER_NAME}" psql -U "${POSTGRES_USER}" -d postgres -v ON_ERROR_STOP=1 -c "CREATE DATABASE ${RESTORE_DB} OWNER ${POSTGRES_USER};" >/dev/null

echo "==> restoring with pg_restore --clean --if-exists --no-owner into ${RESTORE_DB}"
docker cp "${DUMP_FILE}" "${CONTAINER_NAME}:/tmp/restore.dump"
MSYS_NO_PATHCONV=1 docker exec "${CONTAINER_NAME}" pg_restore --clean --if-exists --no-owner -U "${POSTGRES_USER}" -d "${RESTORE_DB}" /tmp/restore.dump

# 6. Verification: migration version, table counts, and actual login (conditions 2,3,4)
echo "==> verifying migration version matches (condition 2)"
query_count() {
  local db="$1"
  local sql="$2"
  docker exec "${CONTAINER_NAME}" psql -U "${POSTGRES_USER}" -d "${db}" -t -A -v ON_ERROR_STOP=1 -c "${sql}" | tr -d '[:space:]'
}

RESTORE_MIG_COUNT=$(query_count "${RESTORE_DB}" "SELECT count(*) FROM _sqlx_migrations;")
if [ "${RESTORE_MIG_COUNT}" = "0" ] || [ "${RESTORE_MIG_COUNT}" = "" ]; then
  echo "error: migration table is empty" >&2
  exit 1
fi
if [ "${EXTERNAL_MODE}" = "0" ]; then
  SOURCE_MIG_COUNT=$(query_count "${POSTGRES_DB}" "SELECT count(*) FROM _sqlx_migrations;")
  echo "    source migrations: ${SOURCE_MIG_COUNT}, restore migrations: ${RESTORE_MIG_COUNT}"
  if [ "${SOURCE_MIG_COUNT}" != "${RESTORE_MIG_COUNT}" ]; then
    echo "error: migration version mismatch" >&2
    exit 1
  fi
else
  echo "    restored migrations: ${RESTORE_MIG_COUNT}"
fi

echo "==> verifying required restored tables (condition 3)"
tables=("users" "user_credentials" "roles" "permissions" "role_permissions" "user_roles" "auth_sessions" "refresh_tokens" "world_definitions" "world_instances" "instance_checkpoints")
for tbl in "${tables[@]}"; do
  if [ "${EXTERNAL_MODE}" = "1" ]; then
    DST=$(query_count "${RESTORE_DB}" "SELECT count(*) FROM ${tbl};" 2>/dev/null || echo "missing")
    if [ "${DST}" = "missing" ]; then
      echo "error: required restored table is missing: ${tbl}" >&2
      exit 1
    fi
    echo "    ${tbl}: restore=${DST}"
    continue
  fi
  # Skip if table does not exist yet (future migrations) – treat as mismatch only if one side has it.
  SRC=$(query_count "${POSTGRES_DB}" "SELECT count(*) FROM ${tbl};" 2>/dev/null || echo "missing")
  DST=$(query_count "${RESTORE_DB}" "SELECT count(*) FROM ${tbl};" 2>/dev/null || echo "missing")
  echo "    ${tbl}: source=${SRC} restore=${DST}"
  if [ "${SRC}" != "${DST}" ]; then
    echo "error: row count mismatch for ${tbl}: source=${SRC} restore=${DST}" >&2
    exit 1
  fi
done

# Condition 4: start server against restored DB and actually log in.
echo "==> verifying restored administrator can log in (condition 4)"
export DATABASE_URL="${RESTORE_DATABASE_URL}"
export ORBISYNC_TOKEN_SIGNING_KEY="${RESTORE_TOKEN_KEY}"
export ORBISYNC_PAGINATION_HMAC_KEY="${RESTORE_HMAC_KEY}"
export ORBISYNC_REFRESH_TOKEN_HMAC_KEY="${RESTORE_REFRESH_HMAC_KEY}"
export ORBISYNC_REALTIME_TICKET_HMAC_KEY="${RESTORE_REALTIME_TICKET_HMAC_KEY}"
export ORBISYNC_IDEMPOTENCY_HMAC_KEY="${RESTORE_IDEMPOTENCY_HMAC_KEY}"
export ORBISYNC_PASSWORD_DENYLIST_FILE="${DENYLIST_FILE}"
# Use an ephemeral file for the server PID log; cargo run will inherit env.
cargo run --quiet -p orbisync-server -- --bind "${SERVER_BIND}" > "${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
echo "    server PID ${SERVER_PID}, waiting for /health/ready on ${SERVER_BIND}"

# Wait for health endpoint (up to 30s)
HEALTH_OK=0
for _ in $(seq 1 30); do
  if http_get "http://${SERVER_BIND}/health/ready"; then
    HEALTH_OK=1
    break
  fi
  sleep 1
done
if [ "${HEALTH_OK}" != "1" ]; then
  echo "error: restored server did not become ready" >&2
  cat "${SERVER_LOG}" >&2 || true
  exit 1
fi

# Read password without logging it; trim newline. Build JSON via file to avoid leaking via /proc/*/cmdline.
RESTORE_PASSWORD="$(tr -d '\r\n' < "${PASSWORD_FILE}")"
LOGIN_JSON="${TMP_DIR}/login_payload.json"
if command -v jq >/dev/null 2>&1; then
  jq -n --arg id "${ADMIN_LOGIN_ID}" --arg pw "${RESTORE_PASSWORD}" '{"login_id":$id,"password":$pw}' > "${LOGIN_JSON}"
else
  printf '{"login_id":"%s","password":"%s"}' "${ADMIN_LOGIN_ID}" "${RESTORE_PASSWORD}" > "${LOGIN_JSON}"
fi
# Perform login via HTTP (same endpoint as W-15: POST /v1/auth/login)
LOGIN_RESPONSE="${TMP_DIR}/login.json"
HTTP_CODE=""
if command -v curl >/dev/null 2>&1; then
  HTTP_CODE=$(curl -s -o "${LOGIN_RESPONSE}" -w "%{http_code}" \
    -X POST "http://${SERVER_BIND}/v1/auth/login" \
    -H "Content-Type: application/json" \
    -d @"${LOGIN_JSON}")
else
  if wget --help 2>&1 | grep -q "body-file"; then
    HTTP_CODE=$(wget -qO "${LOGIN_RESPONSE}" --header="Content-Type: application/json" --body-file="${LOGIN_JSON}" --server-response "http://${SERVER_BIND}/v1/auth/login" 2>&1 | awk '/HTTP\//{code=$2} END{print code}')
  else
    HTTP_CODE=$(wget -qO "${LOGIN_RESPONSE}" --post-data="$(cat "${LOGIN_JSON}")" --header="Content-Type: application/json" --server-response "http://${SERVER_BIND}/v1/auth/login" 2>&1 | awk '/HTTP\//{code=$2} END{print code}')
  fi
fi
rm -f "${LOGIN_JSON}"
echo "    login HTTP code: ${HTTP_CODE}"
if [ "${HTTP_CODE}" != "200" ]; then
  echo "error: login on restored DB failed with HTTP ${HTTP_CODE}" >&2
  cat "${LOGIN_RESPONSE}" >&2 || true
  cat "${SERVER_LOG}" >&2 || true
  rm -f "${LOGIN_RESPONSE}"
  exit 1
fi
# Do not print the response body (contains token); just verify it contains access_token
if ! grep -q "access_token" "${LOGIN_RESPONSE}"; then
  echo "error: login response missing access_token" >&2
  cat "${LOGIN_RESPONSE}" >&2 || true
  rm -f "${LOGIN_RESPONSE}"
  exit 1
fi
rm -f "${LOGIN_RESPONSE}"
echo "==> login on restored DB succeeded (condition 4 verified)"

# Stop server before trap cleanup tries again
kill "${SERVER_PID}" >/dev/null 2>&1 || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""

# Condition 5 is implicit: set -e ensures any failure above (including pg_dump/pg_restore)
# returns non-zero; a corrupted dump file would make pg_restore fail.
echo "==> restore drill passed"

# Trapped cleanup will remove container and temp files.
