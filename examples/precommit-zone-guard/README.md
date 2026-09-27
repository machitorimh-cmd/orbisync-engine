# precommit-zone-guard — Minimal Pre-Commit Validation Hook Example

A working (not stub) example of an OrbiSync **state-confirmation-before**
extension (ADR-025, [`docs/design/extension-mechanism.md`](../../docs/design/extension-mechanism.md)
§12). It denies `spawn` commands whose position falls inside a configured
"no-fly zone" and allows everything else — demonstrating that an
application-specific rule can reject a Core-authorized update **without
modifying Core**.

This is a P0 item of
`docs/design/generalization-and-llm-app-platform.md` (internal record omitted from this source distribution)
§2.2. Implementation status, scope, and limits are tracked in
`docs/plans/generalization-2.2-pre-commit-validation-hook.md` (internal record omitted from this source distribution)
and [`docs/adr/ADR-025-pre-commit-extension-validation-hook.md`](../../docs/adr/ADR-025-pre-commit-extension-validation-hook.md).

## What this does and does not cover

- Covers: `SpawnEntity` position checks. `UpdateEntityComponent` and
  `DeleteEntity` are always allowed by this example (real deployments would
  add their own rules for those operations using the same signature
  verification and response shape).
- Does **not** cover `UpdateTransform` (the 20Hz realtime position stream —
  out of scope for synchronous hook validation by design, ADR-025 §2.3).
  Position updates after the initial spawn are not checked by this hook.
- **Known bypass, not just an out-of-scope operation**: the first
  `UpdateTransform` OrbiSync receives for an `entity_id` nobody has spawned
  yet auto-creates that entity at the submitted position (Core's existing
  M2 behavior). That creation never reaches this hook. A client that skips
  `SpawnEntity` and sends a raw `UpdateTransform` for a fresh entity ID can
  place it inside the no-fly zone without ever being asked. This example
  only demonstrates rejecting a position submitted through `SpawnEntity`;
  it is not a complete enforcement of "nothing may exist inside the zone."
  See ADR-025 "既知の迂回経路" for why closing this gap is out of scope for
  this round (it would require either putting a synchronous HTTP call on
  the 20Hz transform path, or having Core disable auto-create entirely —
  both larger changes than this improvement covers).
- Does **not** provide multi-entity atomic guarantees (e.g. "pick up item AND
  award score" as one unit) — see the plan document's stated limits.

## Prerequisites

- Node 18+ (no npm dependencies — the script uses only the Node standard
  library).
- OpenSSL, to generate a self-signed TLS certificate (ADR-007 requires the
  Webhook/pre-commit endpoint to be HTTPS; there is no way to opt out of TLS
  even for local testing).
- A running OrbiSync server (`cargo run -p orbisync-server -- --config
  orbisync.toml.example`, see `docs/operations/deployment.md`) with
  PostgreSQL migrated.

## 1. Generate a local TLS certificate

```bash
cd examples/precommit-zone-guard
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout key.pem -out cert.pem -days 3 \
  -subj "/CN=127.0.0.1"
```

`cert.pem` / `key.pem` are local-only artifacts; do not commit them.

## 2. Choose a signing secret and export it

The server resolves `signing_secret_ref` via `EnvSecretProvider`
(`crates/orbisync-extensions/src/delivery.rs`), which reads an environment
variable of that exact name on the **server** host. Pick a name and value,
export it on both the server host and this extension's host:

```bash
export ORBI_EXTENSION_SECRET_ZONE_GUARD="$(openssl rand -hex 32)"
```

## 3. Register the extension

Insert a row into `extension_registrations` (or use the admin registration
path once available). The capability
`hooks:entity:spawn` is what makes the server call this
extension before confirming a `spawn` command (ADR-025 §2.7); a registration
with no matching capability is never consulted (opt-in, `extension-mechanism.md`
§12).

```sql
INSERT INTO extension_registrations
  (extension_id, name, description, endpoint, subscribed_events,
   capabilities, token_scopes, status, signing_secret_ref)
VALUES (
  gen_random_uuid(),
  'zone-guard',
  'Denies spawns inside a no-fly zone',
  'https://127.0.0.1:8443/precommit',
  '[]'::jsonb,
  '["hooks:entity:spawn"]'::jsonb,
  '[]'::jsonb,
  'active',
  'ORBI_EXTENSION_SECRET_ZONE_GUARD'
);
```

## 4. Allow the local loopback endpoint (local testing only)

ADR-007's egress policy rejects loopback/private/link-local endpoints by
default (SSRF protection), and this applies to the pre-commit hook too
(ADR-025 §2.8). For local testing only, set in `orbisync.toml` /
environment:

```toml
[extensions]
allow_loopback_endpoints = true
```

```bash
export ORBISYNC_EXTENSIONS_ALLOW_LOOPBACK_ENDPOINTS=true
```

**Never set this to `true` in a production deployment.**

## 5. Run the extension

```bash
node server.js
```

It listens on `https://127.0.0.1:8443/precommit`.

## 6. Try it

Spawn an entity at a position inside the no-fly zone
(`-5 <= x <= 5, -5 <= z <= 5`) through any client (e.g.
`examples/minimal-client-typescript`, or the integration test helpers in
`tests/integration/tests/entity_w16.rs`). The command is rejected with
`code = "PRE_COMMIT_DENIED"` and the reason from this extension. A spawn
outside the zone is accepted normally.

The extension logs one line per decision to stdout, e.g.:

```
precommit.entity.spawn request_id=... -> deny (position (1, 2) is inside the no-fly zone)
```

## How the request/response contract works

- Request: the same HMAC-signed webhook shape ADR-007 defines
  (`X-OrbiSync-Signature: sha256=<hex>`, `X-OrbiSync-Event-Id`,
  `X-OrbiSync-Timestamp`), body `{"event_id", "event_kind", "timestamp",
  "payload": {"request_id", "instance_id", "entity_id", "operation",
  "requester", "client_expected_revision", "component_key", "payload"}}`.
  `payload.payload` is the client's original command arguments,
  forwarded as-is — Core does not interpret it
  (`domain-model.md` §5).
- Response: `{"decision": "allow"}` or `{"decision": "deny", "reason":
  "..."}`. Any other status code, a timeout, or an unparsable body is
  treated as `deny` by Core (fail closed, ADR-025).
- Timeout: Core waits at most `extensions.pre_commit_validation_timeout_ms`
  (default 500ms) for a response.
