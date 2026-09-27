#!/usr/bin/env bash
# Prepare a local OrbiSync development environment
# (`repo-crate-conventions.md` §10).
#
#   ./scripts/bootstrap-dev.sh
#
# The script is idempotent: it checks the toolchain, starts PostgreSQL through
# Docker Compose, waits for it and applies the migrations.

set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: $1 is required but not installed" >&2
    exit 1
  fi
}

require cargo
require docker

if [ ! -f .env ]; then
  echo "==> creating .env from deploy/compose/.env.example"
  cp deploy/compose/.env.example .env
fi

# shellcheck disable=SC1091
set -a && . ./.env && set +a

echo "==> starting PostgreSQL"
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d postgres

echo "==> waiting for PostgreSQL to accept connections"
for _ in $(seq 1 60); do
  if docker compose -f deploy/compose/compose.dev.yml --env-file .env \
      exec -T postgres pg_isready -U "${POSTGRES_USER:-orbisync}" >/dev/null 2>&1; then
    postgres_ready=1
    break
  fi
  sleep 1
done

if [ "${postgres_ready:-0}" != 1 ]; then
  echo "error: PostgreSQL did not become ready within 60 seconds" >&2
  docker compose -f deploy/compose/compose.dev.yml --env-file .env ps >&2
  docker compose -f deploy/compose/compose.dev.yml --env-file .env logs --tail 50 postgres >&2
  exit 1
fi

echo "==> applying migrations"
cargo run --quiet -p orbisync-server -- migrate

echo "==> verifying the workspace"
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
python scripts/check_architecture.py
python scripts/validate_design.py --review

cat <<'MESSAGE'

Development environment is ready.

  cargo run -p orbisync-server          # start the server
  curl http://localhost:8080/health/live
  curl http://localhost:8080/health/ready

MESSAGE
