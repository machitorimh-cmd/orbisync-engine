# Producing the kit (engine developer only)

Consumers receive the archive, never these build prerequisites. From the checkout
root, with Node 24, Python 3, Buf and the repository's protoc plugins installed:

```sh
npm ci --prefix sdk/typescript
buf generate
python scripts/package_consumer_kit.py artifacts/consumer-kit/release
```

The output directory must not exist. `npm pack` builds SDK JS and declarations,
copies licenses and consumer guides, and includes generated protocol runtime.
Assembly uses an explicit starter file allowlist, omits producer tests, refreshes
tarball integrity in the starter lockfile, and writes ZIP, BUILD.json and SHA256SUMS.
BUILD.json records the source commit and whether the checkout was dirty.
Nothing is published. Commit source first when generating a final handoff.

To use the repository starter too, copy the produced tarball into
examples/minimal-client-typescript, run `npm install --package-lock-only` there to
refresh its tarball integrity, then `npm ci`, `npm run check`, `npm run build`.
SDK source-checkout maintenance notes are in sdk/typescript/DEVELOPMENT.md.

Minimal distribution verification: extract the ZIP outside the checkout, run
`npm ci`, `npm run check`, `npm run build` in its starter. Copy the producer's
scripts/consumer_kit_smoke.ts there and invoke `node --test consumer_kit_smoke.ts`
with ORBISYNC_E2E_HELPER pointing to a provenance-checked existing helper binary
outside the checkout. That fixture is verification infrastructure, not part of
the consumer kit. The test uses installed public exports and the built CLI.
Inspect tarball/archive contents and record checksums. No broad suite, Rust
rebuild, database or load acceptance is implied by this packaging check.
