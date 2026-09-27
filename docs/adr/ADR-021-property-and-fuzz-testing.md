# ADR-021: Property and fuzz testing frameworks

- Status: Accepted
- Date: 2026-08-29
- Decision Owners: avistoria

## Context

The test strategy requires property tests for finite transforms, protocol
round trips, spatial-grid containment, permission combinations, and sequence
ordering (`docs/design/test-and-ci.md` §2.4). It also requires fuzz targets for
the Protobuf decoder, WebSocket frame handling, custom component validation,
login ID parsing, and configuration parsing (§2.5). The framework choices were
left for TC-02 and TC-03.

## Decision

1. Use `proptest` for property tests (TC-02). Property tests run through
   `cargo test` and remain in the owning crate or `tests/` as recommended by
   the test strategy.
2. Use `cargo-fuzz` with libFuzzer and `arbitrary` for fuzz tests (TC-03).
   Fuzz targets are kept separate from the normal workspace test path and run
   in the nightly/release fuzz stages defined by the test strategy.

## Rationale

`proptest` provides shrinking for failing generated cases and integrates with
the existing Rust test runner. `cargo-fuzz`/libFuzzer is the established Rust
workflow for sustained external-input fuzzing, while `arbitrary` supplies
structured inputs where a target needs them.

## Consequences

- Owning crates that contain property tests add `proptest` as a development
  dependency; production binaries do not link it.
- Fuzzing requires the `cargo-fuzz` tooling and a nightly toolchain in its
  dedicated CI job. It is not part of the regular PR test command.
- The five property and five fuzz target areas remain separate acceptance
  work. This ADR selects frameworks but does not define additional target
  behavior beyond `test-and-ci.md` §2.4 and §2.5.
