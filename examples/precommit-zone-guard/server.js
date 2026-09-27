#!/usr/bin/env node
// Minimal working example of an OrbiSync pre-commit validation hook
// extension (ADR-025, docs/design/extension-mechanism.md §12).
//
// It rejects `spawn` commands whose position falls inside a configured
// "no-fly zone" bounding box, and allows everything else. This is the
// application-specific rule; Core is not modified to add it.
//
// Run:
//   ORBI_EXTENSION_SECRET_ZONE_GUARD=<same value the server resolves via
//     EnvSecretProvider for signing_secret_ref> node server.js
//
// See README.md in this directory for the full setup (TLS certificate,
// extension_registrations row, orbisync.toml.example keys).
//
// No dependencies beyond Node's standard library.

const https = require('node:https');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const PORT = Number(process.env.PORT || 8443);
const SECRET_ENV = 'ORBI_EXTENSION_SECRET_ZONE_GUARD';
const CLOCK_SKEW_SECONDS = 5 * 60; // ADR-007 §Decision: 5 minute replay window.
const MAX_BODY_BYTES = 64 * 1024; // Matches MAX_RESPONSE_BODY_BYTES on the Core side.

// The no-fly zone: any spawn whose position_x/position_z falls inside this
// axis-aligned box (in world units) is denied. This is the entire
// application-specific rule — everything else in this file is generic
// webhook signature verification that any pre-commit extension needs.
const NO_FLY_ZONE = {
  minX: -5,
  maxX: 5,
  minZ: -5,
  maxZ: 5,
};

function readSecret() {
  const secret = process.env[SECRET_ENV];
  if (!secret) {
    throw new Error(
      `${SECRET_ENV} is not set. It must match the value the OrbiSync ` +
        'server resolves for the registration\'s signing_secret_ref ' +
        '(EnvSecretProvider reads this same environment variable name on ' +
        'the server host).',
    );
  }
  return secret;
}

// Verifies the HMAC-SHA-256 signature the same way build_signed_webhook
// produces it on the Core side (crates/orbisync-extensions/src/delivery.rs):
// signing_input = "{timestamp}.{event_id}.{raw_body_utf8}",
// header value = "sha256=<hex digest>".
function verifySignature(rawBody, headers, secret) {
  const signatureHeader = headers['x-orbisync-signature'];
  const eventId = headers['x-orbisync-event-id'];
  const timestampHeader = headers['x-orbisync-timestamp'];
  if (!signatureHeader || !eventId || !timestampHeader) {
    return { ok: false, reason: 'missing signature headers' };
  }
  const timestamp = Number(timestampHeader);
  if (!Number.isFinite(timestamp)) {
    return { ok: false, reason: 'invalid timestamp header' };
  }
  const now = Math.floor(Date.now() / 1000);
  if (Math.abs(now - timestamp) > CLOCK_SKEW_SECONDS) {
    return { ok: false, reason: 'timestamp outside the replay window' };
  }
  const signingInput = `${timestamp}.${eventId}.${rawBody}`;
  const expected =
    'sha256=' +
    crypto.createHmac('sha256', secret).update(signingInput).digest('hex');
  const expectedBuf = Buffer.from(expected);
  const actualBuf = Buffer.from(signatureHeader);
  if (
    expectedBuf.length !== actualBuf.length ||
    !crypto.timingSafeEqual(expectedBuf, actualBuf)
  ) {
    return { ok: false, reason: 'signature mismatch' };
  }
  return { ok: true };
}

// `requestPayload` is the outer HTTP body's `payload` field — the
// PreCommitValidationRequest JSON built by
// crates/orbisync-extensions/src/precommit.rs (request_id, instance_id,
// entity_id, operation, requester, client_expected_revision, component_key,
// payload). Its own nested `payload` field is the client's original
// `arguments` Struct, forwarded as-is (Core does not interpret it,
// domain-model.md §5).
function decide(requestPayload) {
  // `requestPayload.operation` is one of "precommit.entity.spawn" /
  // "precommit.entity.update" / "precommit.entity.delete"
  // (PreCommitOperation::event_kind in crates/orbisync-extensions/src/precommit.rs).
  // This example only restricts spawn position; update/delete are allowed.
  if (requestPayload.operation !== 'precommit.entity.spawn') {
    return { decision: 'allow' };
  }
  // This example's client sends position_x/position_y/position_z at the top
  // level of `arguments`, the same shape entity_command_bytes/
  // spawn_args_global use in the OrbiSync integration tests
  // (tests/integration/tests/entity_w16.rs).
  const args = requestPayload.payload || {};
  const x = Number(args.position_x);
  const z = Number(args.position_z);
  if (
    Number.isFinite(x) &&
    Number.isFinite(z) &&
    x >= NO_FLY_ZONE.minX &&
    x <= NO_FLY_ZONE.maxX &&
    z >= NO_FLY_ZONE.minZ &&
    z <= NO_FLY_ZONE.maxZ
  ) {
    return {
      decision: 'deny',
      reason: `position (${x}, ${z}) is inside the no-fly zone`,
    };
  }
  return { decision: 'allow' };
}

function loadTlsOptions() {
  const certPath = path.join(__dirname, 'cert.pem');
  const keyPath = path.join(__dirname, 'key.pem');
  if (!fs.existsSync(certPath) || !fs.existsSync(keyPath)) {
    throw new Error(
      `${certPath} / ${keyPath} not found. Generate a self-signed ` +
        'certificate first — see README.md.',
    );
  }
  return {
    cert: fs.readFileSync(certPath),
    key: fs.readFileSync(keyPath),
  };
}

function main() {
  const secret = readSecret();
  const tls = loadTlsOptions();
  const server = https.createServer(tls, (req, res) => {
    if (req.method !== 'POST') {
      res.writeHead(405).end();
      return;
    }
    let received = 0;
    const chunks = [];
    req.on('data', (chunk) => {
      received += chunk.length;
      if (received > MAX_BODY_BYTES) {
        res.writeHead(413).end();
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      const rawBody = Buffer.concat(chunks).toString('utf8');
      const verified = verifySignature(rawBody, req.headers, secret);
      if (!verified.ok) {
        console.error('rejecting unverified request:', verified.reason);
        res.writeHead(401, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ decision: 'deny', reason: verified.reason }));
        return;
      }
      let body;
      try {
        body = JSON.parse(rawBody);
      } catch {
        res.writeHead(400, { 'content-type': 'application/json' });
        res.end(
          JSON.stringify({ decision: 'deny', reason: 'invalid JSON body' }),
        );
        return;
      }
      const decision = decide(body.payload || {});
      console.log(
        `${body.event_kind ?? 'unknown'} request_id=${body.event_id ?? '?'} -> ${decision.decision}${decision.reason ? ` (${decision.reason})` : ''}`,
      );
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(decision));
    });
  });
  server.listen(PORT, '127.0.0.1', () => {
    console.log(`precommit-zone-guard listening on https://127.0.0.1:${PORT}`);
  });
}

main();
