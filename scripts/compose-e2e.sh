#!/usr/bin/env bash
# Brings up the full Docker Compose stack and proves it is usable.
#
# MVP condition 13 is "Docker Compose で起動できる", verified by an E2E test.
# A container that reaches "running" proves very little: before this script
# existed, `compose.dev.yml` defined a server that could not start at all,
# because `PasswordPolicy::production` requires a 10,000 entry denylist that
# nothing supplied. Nobody noticed, because nothing ever started it.
#
# So the check here is not "did it boot" but "can somebody use it": migrate,
# create the first administrator, log in, and get a realtime ticket — all
# against the composed server over HTTP.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

COMPOSE_FILE="deploy/compose/compose.dev.yml"
PROJECT="orbisync-compose-e2e"
ENV_FILE="$(mktemp)"
PASSWORD_FILE="$(mktemp)"
PORT="${ORBISYNC_E2E_PORT:-18081}"
PG_PORT="${ORBISYNC_E2E_PG_PORT:-55433}"

cleanup() {
  local code=$?
  docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" --env-file "${ENV_FILE}" \
    down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "${ENV_FILE}" "${PASSWORD_FILE}"
  exit "${code}"
}
trap cleanup EXIT INT TERM

echo "==> generating the development password denylist"
bash scripts/gen-dev-password-denylist.sh >/dev/null

echo "==> generating ephemeral Ed25519 signing key (not logged, not on argv)"
TOKEN_KEY_FILE="$(mktemp)"
if ! command -v openssl >/dev/null 2>&1; then
  echo "error: openssl is required to generate Ed25519 key" >&2
  exit 1
fi
openssl genpkey -algorithm ed25519 -out "${TOKEN_KEY_FILE}" 2>/dev/null
# Escape newlines for docker --env-file (single line, \n escapes are normalised by the server)
TOKEN_PEM_ESCAPED="$(awk '{printf "%s\\n", $0}' "${TOKEN_KEY_FILE}")"
rm -f "${TOKEN_KEY_FILE}"
# Ensure the key never appears in logs or argv: write via file descriptor, not command line
cat > "${ENV_FILE}" <<EOF
POSTGRES_USER=orbisync
POSTGRES_PASSWORD=orbisync-compose-e2e
POSTGRES_DB=orbisync
POSTGRES_PORT=${PG_PORT}
ORBISYNC_PORT=${PORT}
DATABASE_URL=postgres://orbisync:orbisync-compose-e2e@postgres:5432/orbisync
ORBISYNC_TOKEN_SIGNING_KEY=${TOKEN_PEM_ESCAPED}
ORBISYNC_PAGINATION_HMAC_KEY=compose-e2e-only-pagination-hmac-key
ORBISYNC_REFRESH_TOKEN_HMAC_KEY=compose-e2e-only-refresh-hmac-key
ORBISYNC_REALTIME_TICKET_HMAC_KEY=compose-e2e-only-realtime-ticket-hmac-key
ORBISYNC_IDEMPOTENCY_HMAC_KEY=compose-e2e-only-idempotency-hmac-key
ORBISYNC_EXTENSION_TOKEN_HMAC_KEY=compose-e2e-only-extension-hmac-key
EOF
# Clear variable that held the key material from shell memory (best effort)
TOKEN_PEM_ESCAPED="***"

echo "==> building and starting the stack"
docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" --env-file "${ENV_FILE}" up -d --build

echo "==> applying migrations inside the server container"
docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" --env-file "${ENV_FILE}" \
  run --rm --entrypoint orbisync-server server migrate

echo "==> waiting for /health/ready"
ready=""
for _ in $(seq 1 60); do
  code=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:${PORT}/health/ready" || true)
  if [ "${code}" = "200" ]; then ready="yes"; break; fi
  sleep 2
done
if [ -z "${ready}" ]; then
  echo "error: /health/ready never returned 200" >&2
  docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" --env-file "${ENV_FILE}" logs server >&2 || true
  exit 1
fi
echo "    ready"

echo "==> creating the first administrator"
# bootstrap-admin refuses to print to a non-TTY, so it writes to a file. The
# file is created and read inside a single throwaway container: the password
# never lands on a bind mount, in a named volume, or in the compose logs. Its
# only trip outside is into a shell variable here.
docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" --env-file "${ENV_FILE}" \
  run --rm --no-TTY --entrypoint sh server -c '
    orbisync-server bootstrap-admin \
      --login-id admin --display-name Administrator \
      --password-denylist /etc/orbisync/password-denylist.txt \
      --password-output /tmp/admin-password.txt >&2 &&
    cat /tmp/admin-password.txt
  ' > "${PASSWORD_FILE}" 2>/dev/null || true

ADMIN_PASSWORD="$(tr -d '\r\n' < "${PASSWORD_FILE}")"
if [ -z "${ADMIN_PASSWORD}" ]; then
  echo "error: bootstrap-admin produced no password" >&2
  exit 1
fi

echo "==> logging in through the composed server"
# Do not use `curl -d '{"password":"$PASS"}'` — it leaks via /proc/*/cmdline. Write JSON to a temp file.
LOGIN_JSON="$(mktemp)"
# Use jq if available, else printf to file (still not on cmdline)
if command -v jq >/dev/null 2>&1; then
  jq -n --arg pw "${ADMIN_PASSWORD}" '{"login_id":"admin","password":$pw}' > "${LOGIN_JSON}"
else
  printf '{"login_id":"admin","password":"%s"}' "${ADMIN_PASSWORD}" > "${LOGIN_JSON}"
fi
LOGIN_RESPONSE="$(mktemp)"
CODE=$(curl -s -o "${LOGIN_RESPONSE}" -w "%{http_code}" \
  -X POST "http://localhost:${PORT}/v1/auth/login" \
  -H 'content-type: application/json' -d @"${LOGIN_JSON}")
rm -f "${LOGIN_JSON}"
if [ "${CODE}" != "200" ]; then
  echo "error: login returned ${CODE}" >&2
  cat "${LOGIN_RESPONSE}" >&2
  rm -f "${LOGIN_RESPONSE}"
  exit 1
fi

ACCESS_TOKEN="$(sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p' "${LOGIN_RESPONSE}")"
rm -f "${LOGIN_RESPONSE}"
if [ -z "${ACCESS_TOKEN}" ]; then
  echo "error: login succeeded but returned no access_token" >&2
  exit 1
fi
echo "    logged in"

echo "==> requesting a realtime ticket"
CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  -X POST "http://localhost:${PORT}/v1/realtime/tickets" \
  -H "authorization: Bearer ${ACCESS_TOKEN}")
if [ "${CODE}" != "200" ]; then
  echo "error: realtime ticket returned ${CODE}" >&2
  exit 1
fi
echo "    ticket issued"

echo "==> compose E2E passed"
