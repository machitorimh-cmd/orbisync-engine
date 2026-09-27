#!/usr/bin/env bash
# Generates a placeholder password denylist for local development and CI.
#
# `PasswordPolicy::production` refuses to start unless the corpus contains
# exactly 10,000 entries (`password.rs`), so the server cannot run at all
# without one. OrbiSync does not ship a curated corpus: the useful lists are
# third-party data with their own licences, and vendoring one into this
# repository would make a licensing decision on the operator's behalf.
#
# The file this script writes therefore blocks nothing real. It exists so that
# `docker compose up` and the documented quickstart work on a fresh clone.
#
# BEFORE ANY DEPLOYMENT THAT REAL PEOPLE LOG INTO, replace it with an actual
# list of common passwords (for example the top 10,000 entries of a breach
# corpus). Keeping this placeholder in production means every common password
# your users pick will be accepted.
set -euo pipefail

OUT="${1:-deploy/dev-password-denylist.txt}"
COUNT=10000

mkdir -p "$(dirname "${OUT}")"

# Entries must be distinct and must not collide with the policy's other rules,
# so they are synthesised rather than drawn from real passwords.
seq 1 "${COUNT}" | awk '{printf "orbisync-dev-placeholder-%05d\n", $1}' > "${OUT}"

lines=$(wc -l < "${OUT}" | tr -d ' ')
if [ "${lines}" != "${COUNT}" ]; then
  echo "error: expected ${COUNT} entries, wrote ${lines}" >&2
  exit 1
fi

echo "wrote ${lines} placeholder entries to ${OUT}"
echo "WARNING: this list blocks no real password. Replace it before production."
