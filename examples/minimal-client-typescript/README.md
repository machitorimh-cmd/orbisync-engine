# Standalone TypeScript starter

This starter imports only public @orbisync/client exports. Consumers receive it
in the built kit with a local SDK tarball, lockfile, configuration template and
self-contained docs. Use Node 24.x with native WebSocket. No engine checkout,
Rust or Buf is needed by consumers.

See [consumer instructions](../../docs/consumer-kit/README.md),
[LLM brief](../../docs/consumer-kit/LLM-INTEGRATION.md), and developer-only
[kit assembly](../../docs/consumer-kit/PRODUCING.md).

With the SDK tarball present in this directory:

```sh
npm ci
npm run check
npm run build
```

Supply environment variables from connection.env.example, then npm start, or
copy it to .env, fill it locally and run `node --env-file=.env dist/main.js`.
Use an operator-provisioned owned entity and example.move rule. The CLI creates
neither worlds nor entities. The SDK owns POST /v1/realtime/tickets, the
realtime_ticket / expires_in contract, `/ws` and `orbisync.v1.protobuf`.

The producer-only npm test uses the existing helper selected through
ORBISYNC_E2E_HELPER; it is omitted from the standalone kit. Build first.
