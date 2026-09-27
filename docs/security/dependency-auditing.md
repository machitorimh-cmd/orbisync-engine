# Dependency advisory auditing

OrbiSync has no published release, package registry, or container registry
workflow today. The repository therefore does not create a fictional publish,
SBOM, signing, or image-scan gate. Those gates belong in a future release
workflow once a real tag and distribution path are accepted.

## CI and scheduled checks

- `.github/workflows/ci.yml` runs on pull requests, `main` pushes, and manual
  dispatch. Its `PR CI` aggregate remains the required all-gates result and
  includes `cargo deny` plus the TypeScript SDK npm advisory check.
- `.github/workflows/dependency-audit.yml` runs every Monday at 03:17 UTC on
  the default branch (`main`) and can be manually dispatched. It runs
  `cargo deny check advisories` against the workspace, load-generator, and
  fuzz lockfiles, plus the TypeScript SDK npm audit, independently of the
  longer test suite.
- The npm check always runs `npm ci` with the tracked
  `sdk/typescript/package-lock.json`; it does not resolve a fresh dependency
  tree with `npm install`.
- Dependabot tracks that npm lockfile, the root workspace lockfile, the
  standalone load-generator and fuzz lockfiles, the pinned GitHub Actions, and
  the Docker base images.
- `high` and `critical` npm findings block the check. Lower severities are
  reported by npm but do not block under the current policy. A report with
  zero findings is only the result of that scan at that point in time; it is
  not a guarantee that future advisories will be absent.

The 2026-09-02 local baseline scan found zero npm vulnerabilities and passed
the Rust advisory checks. This is recorded only as the current observation;
the scheduled workflow continues to query the advisory databases so a later
disclosure still fails the gate.

## npm exceptions

`security/npm-audit-policy.json` is the reviewed policy file. It currently has
no exceptions. An exception, if one is approved, must contain:

```json
{
  "id": "npm-advisory-id",
  "package": "affected-package",
  "expires_on": "YYYY-MM-DD",
  "reason": "Document the remediation plan and expiry rationale."
}
```

`scripts/check_npm_audit.py` strictly parses npm audit v1 `advisories` and v2/v3
`vulnerabilities` reports. It fails closed for unknown or malformed formats,
missing metadata, report errors, duplicate or mismatched advisory/package
identities, malformed or expired entries, unmatched exceptions, and unexpected
findings at or above the threshold. CLI input paths are restricted to the
checkout and (for the generated report) `RUNNER_TEMP`; GitHub annotations escape
percent signs and line endings. This keeps the exception's owner-visible reason
and expiry in reviewable repository history rather than in a workflow secret.

## Image and action pinning

Actions are referenced by immutable commit SHA, with the upstream release tag
retained as a comment for Dependabot review. The CI PostgreSQL service and the
development Compose PostgreSQL images use a version tag plus registry digest.
The Dockerfile's Rust builder and Debian runtime base images are likewise
digest-pinned. When updating an image, update the human-readable tag and its
digest in the same reviewed change; do not silently restore a floating tag.

Because no release/tag workflow exists on the current `main` branch, container
SBOM, vulnerability scan, checksum publication, and signing are intentionally
follow-up work rather than simulated release gates.

## Repository settings

The repository default branch is `main`. Maintainers should protect `main` and
require the `PR CI` aggregate status from `.github/workflows/ci.yml`; the
workflow file cannot enforce that repository setting itself. The scheduled
workflow runs against the default branch automatically, while manual dispatch
is available for an operator-selected ref.
