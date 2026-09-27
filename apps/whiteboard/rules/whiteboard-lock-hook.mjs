#!/usr/bin/env node
// Signed pre-commit hook endpoint for the whiteboard demo.
// Replaces the external JSON lock authority: this process holds no state,
// exposes no lock API and only answers allow/deny from the Core state that
// arrives inside each signed request.
import https from 'node:https';
import fs from 'node:fs';
import crypto from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { decide } from './whiteboard-lock-rule.mjs';

const SECRET_ENV = process.env.WHITEBOARD_LOCK_SECRET_ENV || 'ORBI_EXTENSION_SECRET_WHITEBOARD_LOCK';
const HOOK_PORT = Number(process.env.WHITEBOARD_LOCK_HOOK_PORT || 8843);
const MAX_BODY_BYTES = 256 * 1024;
const CLOCK_SKEW_SECONDS = 300;

function reply(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(value));
}

/**
 * Verifies the `sha256=` HMAC over `${timestamp}.${event_id}.` + raw body,
 * byte for byte as `orbisync-extensions::delivery::build_signed_webhook`
 * produces it. The timestamp header string is used verbatim, not re-formatted.
 */
export function verifySignature(raw, headers, secret) {
  const timestamp = headers['x-orbisync-timestamp'];
  const event = headers['x-orbisync-event-id'];
  const signature = headers['x-orbisync-signature'];
  if (typeof timestamp !== 'string' || typeof event !== 'string' || typeof signature !== 'string') return false;
  if (!/^\d+$/.test(timestamp)) return false;
  if (Math.abs(Date.now() / 1000 - Number(timestamp)) > CLOCK_SKEW_SECONDS) return false;
  const expected = Buffer.from(`sha256=${crypto.createHmac('sha256', secret)
    .update(`${timestamp}.${event}.`).update(raw).digest('hex')}`);
  const actual = Buffer.from(signature);
  return expected.length === actual.length && crypto.timingSafeEqual(expected, actual);
}

function handle(secret) {
  return (req, res) => {
    if (req.method !== 'POST' || req.url !== '/precommit') return reply(res, 404, { decision: 'deny' });
    const chunks = [];
    let size = 0;
    req.on('data', chunk => { size += chunk.length; if (size > MAX_BODY_BYTES) { req.destroy(); return; } chunks.push(chunk); });
    req.on('error', () => {});
    req.on('end', () => {
      const raw = Buffer.concat(chunks);
      if (!verifySignature(raw, req.headers, secret)) return reply(res, 401, { decision: 'deny' });
      let result;
      try {
        result = decide(JSON.parse(raw.toString('utf8')).payload);
      } catch {
        return reply(res, 400, { decision: 'deny' });
      }
      console.log(`whiteboard.precommit -> ${result.decision}${result.reason ? ` (${result.reason})` : ''}`);
      return reply(res, 200, result);
    });
  };
}

function main() {
  const secret = process.env[SECRET_ENV];
  if (!secret) throw new Error(`${SECRET_ENV} is required`);
  const cert = process.env.WHITEBOARD_LOCK_CERT;
  const key = process.env.WHITEBOARD_LOCK_KEY;
  if (!cert || !key || !fs.existsSync(cert) || !fs.existsSync(key)) {
    throw new Error('WHITEBOARD_LOCK_CERT / WHITEBOARD_LOCK_KEY must point at readable TLS files');
  }
  https.createServer({ cert: fs.readFileSync(cert), key: fs.readFileSync(key) }, handle(secret))
    .listen(HOOK_PORT, '127.0.0.1', () => console.log(`whiteboard precommit hook listening on https://127.0.0.1:${HOOK_PORT}/precommit`));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
