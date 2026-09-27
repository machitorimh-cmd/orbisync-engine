# ADR-022: Chaos test injection and P2 scenario ownership

- Status: Accepted
- Date: 2026-08-29
- Decision Owners: avistoria

## Context

The test strategy requires chaos coverage for dependency loss and instance
failure, but the repository had no chaos test implementation.  The performance
plan also listed the database-stop and instance-panic cases as P2-04 and P2-05,
which duplicated the chaos-test plan.

## Decision

1. Do not add a chaos-specific framework or external chaos dependency.
2. Inject failures in test code: stop and restart a disposable PostgreSQL
   container for the database scenario, and use a test-only instance-task hook
   to induce a panic for the instance-isolation scenario.
3. Own P2-04 and P2-05 in E-4 chaos coverage.  The P2 table is a reference to
   the chaos test rather than a second copy of the scenarios.
4. The database scenario passes when readiness becomes false while PostgreSQL
   is stopped and returns to true after restart.  The instance scenario passes
   when the panicked task's mailbox closes while another instance task still
   accepts a tick.

## Implementation

- `tests/integration/tests/chaos_e4.rs` implements P2-04.  It requires
  `CHAOS_POSTGRES_CONTAINER` so the caller explicitly supplies a disposable
  container; database credentials remain in `DATABASE_URL` and are never put in
  process arguments.
- `crates/orbisync-world-runtime/src/registry.rs` implements P2-05 as a
  unit-level chaos test with a `cfg(test)` panic hook.  The hook is not compiled
  into the production API.
- `docs/design/scale-and-nfr.md` references this E-4 ownership for P2-04 and
  P2-05 to avoid duplicate tests.

## Consequences

- Chaos tests can run without chaos-mesh, toxiproxy, or another external
  framework.
- The database scenario needs Docker and a disposable PostgreSQL container.
- The instance panic check validates task isolation; checkpoint persistence is
  exercised by the existing checkpoint tests and remains a separate concern.
