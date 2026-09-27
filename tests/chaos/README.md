# E-4 chaos tests

E-4 uses test-code fault injection rather than a chaos-specific framework.

## P2-04: PostgreSQL stop/restart

Run the integration test against a disposable PostgreSQL container and provide
its name separately from the database URL:

```powershell
$env:DATABASE_URL = "postgres://..."
$env:CHAOS_POSTGRES_CONTAINER = "orbisync-chaos-pg"
cargo test -p orbisync-integration-tests --test chaos_e4 -- --nocapture
```

The test stops and restarts only the named container.  It asserts that the
database readiness probe becomes false during the outage and recovers after
the restart.  Do not put credentials in the container name or command-line
arguments.

## P2-05: instance panic

`RuntimeRegistry` contains a `cfg(test)` hook used by
`chaos_instance_panic_isolated_to_one_task`.  The test induces a panic in one
instance owner task, verifies that its mailbox closes, and verifies that a
second instance continues to process a tick.  The panic hook is not available
in production builds.

The P2-04 and P2-05 entries in `docs/design/scale-and-nfr.md` intentionally
refer here and to the Rust tests instead of defining duplicate scenarios.
