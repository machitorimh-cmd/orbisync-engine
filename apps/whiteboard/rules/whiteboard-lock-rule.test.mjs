import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { COMPONENT, decide } from './whiteboard-lock-rule.mjs';
import { verifySignature } from './whiteboard-lock-hook.mjs';

const note = (overrides = {}) => ({
  component_key: COMPONENT, kind: 'object', visibility: 'global',
  text: '本文', color: 'yellow', locked: false, position_x: 10, position_y: 20, ...overrides,
});

const request = (locked, overrides = {}) => ({
  operation: 'precommit.entity.update',
  requester: 'owner',
  client_expected_revision: 7,
  current_entity: {
    entity_id: 'note-1', revision: 7, owner_id: 'owner',
    components: { [COMPONENT]: { encoding: 'json', value: note({ locked }) } },
  },
  component_key: COMPONENT,
  payload: note({ locked: !locked }),
  ...overrides,
});

test('spawn is allowed only when the client asks for an unlocked note', () => {
  const spawn = { operation: 'precommit.entity.spawn', requester: 'owner', client_expected_revision: null,
    current_entity: null, component_key: null, payload: note() };
  assert.equal(decide(spawn).decision, 'allow');
  assert.equal(decide({ ...spawn, payload: note({ locked: true }) }).decision, 'deny');
  assert.equal(decide({ ...spawn, current_entity: { revision: 1, owner_id: 'owner', components: {} } }).decision, 'deny');
});

test('the owner locks and unlocks through a normal component update', () => {
  assert.equal(decide(request(false, { payload: note({ locked: true }) })).decision, 'allow');
  assert.equal(decide(request(true, { payload: note({ locked: false }) })).decision, 'allow');
});

test('a peer may edit an unlocked note but may not lock it', () => {
  const peer = request(false, { requester: 'peer', payload: note({ text: '追記' }) });
  assert.equal(decide(peer).decision, 'allow');
  assert.equal(decide({ ...peer, payload: note({ locked: true }) }).decision, 'deny');
});

test('a locked note rejects peer unlock, edits, delete and other components', () => {
  assert.equal(decide(request(true, { requester: 'peer' })).decision, 'deny');
  // A text edit carries the current locked:true through, so it is frozen too.
  assert.equal(decide(request(true, { payload: note({ locked: true, text: '書き換え' }) })).decision, 'deny');
  assert.equal(decide(request(true, { operation: 'precommit.entity.delete', component_key: null, payload: null })).decision, 'deny');
  assert.equal(decide(request(true, { component_key: 'com.example.other' })).decision, 'deny');
});

test('an unlocked note may be deleted by a peer, leaving Core permissions to decide', () => {
  assert.equal(decide(request(false, { requester: 'peer', operation: 'precommit.entity.delete', component_key: null, payload: null })).decision, 'allow');
});

test('a note with no component yet counts as unlocked', () => {
  const fresh = request(false);
  fresh.current_entity.components = {};
  assert.equal(decide(fresh).decision, 'allow');
});

test('missing state, revision drift and non-JSON components fail closed', () => {
  const missing = request(true);
  delete missing.current_entity;
  assert.equal(decide(missing).decision, 'deny');
  assert.equal(decide(request(true, { current_entity: null })).decision, 'deny');
  assert.equal(decide(request(true, { client_expected_revision: 8 })).decision, 'deny');
  const opaque = request(true);
  opaque.current_entity.components[COMPONENT] = { encoding: 'base64', value: 'AAAA' };
  assert.equal(decide(opaque).decision, 'deny');
});

test('lock state forged inside the client payload is ignored', () => {
  const forged = request(true, { requester: 'peer' });
  forged.payload.current_entity = { owner_id: 'peer', locked: false };
  forged.payload.locked = false;
  assert.equal(decide(forged).decision, 'deny');
});

test('the signature covers the raw body bytes exactly as Core signs them', () => {
  const secret = 'test-secret';
  const raw = Buffer.from(JSON.stringify({ payload: { note: 'ロック' } }), 'utf8');
  const timestamp = String(Math.floor(Date.now() / 1000));
  const event = crypto.randomUUID();
  const sign = (ts, id, body) => `sha256=${crypto.createHmac('sha256', secret).update(`${ts}.${id}.`).update(body).digest('hex')}`;
  const headers = {
    'x-orbisync-timestamp': timestamp,
    'x-orbisync-event-id': event,
    'x-orbisync-signature': sign(timestamp, event, raw),
  };
  assert.equal(verifySignature(raw, headers, secret), true);
  assert.equal(verifySignature(Buffer.concat([raw, Buffer.from(' ')]), headers, secret), false);
  assert.equal(verifySignature(raw, { ...headers, 'x-orbisync-signature': sign(timestamp, event, raw).replace(/.$/, 'f') }, secret), false);
  assert.equal(verifySignature(raw, { ...headers, 'x-orbisync-event-id': crypto.randomUUID() }, secret), false);
  const stale = String(Math.floor(Date.now() / 1000) - 301);
  assert.equal(verifySignature(raw, { ...headers, 'x-orbisync-timestamp': stale, 'x-orbisync-signature': sign(stale, event, raw) }, secret), false);
  assert.equal(verifySignature(raw, { ...headers, 'x-orbisync-signature': undefined }, secret), false);
});
