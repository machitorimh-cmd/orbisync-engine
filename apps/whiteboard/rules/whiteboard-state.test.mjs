import test from "node:test";
import assert from "node:assert/strict";
import { applyEntity, applyEntityCommand, createWhiteboardState, notePayload, NOTE_COMPONENT } from "./whiteboard-state.mjs";

const entity = (revision, text, color) => ({
  entity_id: "note-b",
  revision,
  properties: { fields: { "com.orbisync.whiteboard.note": { structValue: { fields: { value: { structValue: { fields: {
    text: { stringValue: text }, color: { stringValue: color }, locked: { boolValue: false },
  } } } } } } } },
});

test("late join restores the actual JSON Snapshot envelope and component position", () => {
  const state = createWhiteboardState();
  applyEntity(state, {
    entity_id: "late-note", revision: 4,
    transform: { position_x: 0, position_y: 0 },
    properties: { "com.orbisync.whiteboard.note": { encoding: "json", value: {
      text: "restored", color: "mint", position_x: 123, position_y: 456, locked: true,
    } } },
  });
  assert.deepEqual(state.notes.get("late-note"), {
    id: "late-note", revision: 4, text: "restored", color: "mint", x: 123, y: 456, locked: true,
  });
});

test("spawn confirmation supplies the revision for the next edit without a delta", () => {
  const state = createWhiteboardState();
  applyEntityCommand(state, { entityId: "note-a", operation: "spawn", expectedRevision: 0, arguments: {} });
  assert.equal(state.notes.has("note-a"), false);
  applyEntityCommand(state, { entityId: "note-a", operation: "spawn", expectedRevision: 1, arguments: { fields: { text: { stringValue: "confirmed" } } } });
  assert.equal(state.notes.get("note-a").revision, 1);
  assert.equal(state.notes.get("note-a").text, "confirmed");
  applyEntityCommand(state, { entityId: "note-a", operation: "update", expectedRevision: 2, arguments: { fields: { text: { stringValue: "edited" } } } });
  assert.equal(state.notes.get("note-a").revision, 2);
  assert.equal(state.notes.get("note-a").text, "edited");
});

test("delete confirmations prevent delayed commands and snapshots from resurrecting a note", () => {
  const state = createWhiteboardState();
  applyEntity(state, entity(2, "B", "blue"));
  assert.equal(applyEntityCommand(state, { entityId: "note-b", operation: "delete", expectedRevision: 3 }), true);
  assert.equal(applyEntityCommand(state, { entityId: "note-b", operation: "spawn", expectedRevision: 1 }), false);
  assert.equal(applyEntity(state, entity(2, "B", "blue")), false);
  assert.equal(state.notes.has("note-b"), false);
});

test("duplicate command confirmations do not overwrite state", () => {
  const state = createWhiteboardState();
  applyEntity(state, entity(2, "B", "blue"));
  assert.equal(applyEntityCommand(state, { entityId: "note-b", operation: "update", expectedRevision: 2, arguments: { fields: { text: { stringValue: "wrong" } } } }), false);
  assert.equal(state.notes.get("note-b").text, "B");
});

test("generated SDK JsonObject command payload restores peer text, color and position", () => {
  const state = createWhiteboardState();
  applyEntityCommand(state, { entityId: "peer", operation: "spawn", expectedRevision: 1n,
    arguments: { text: "peer text", color: "rose", position_x: 81, position_y: 92, locked: false } });
  assert.deepEqual(state.notes.get("peer"), {
    id: "peer", revision: 1, text: "peer text", color: "rose", x: 81, y: 92, locked: false,
  });
});

test("an old command cannot roll back an authoritative revision", () => {
  const state = createWhiteboardState();
  applyEntity(state, entity(2, "B", "blue"));
  assert.equal(applyEntityCommand(state, { entityId: "note-b", operation: "update", expectedRevision: 1, arguments: {} }), false);
  assert.equal(state.notes.get("note-b").text, "B");
  assert.equal(state.notes.get("note-b").revision, 2);
});

test("authoritative B body and color are restored through the entity path", () => {
  const state = createWhiteboardState();
  applyEntity(state, entity(1, "A", "yellow"));
  applyEntity(state, entity(2, "B", "blue"));
  assert.deepEqual(state.notes.get("note-b"), { id: "note-b", text: "B", x: 240, y: 140, color: "blue", locked: false, revision: 2 });
});

test("every note command payload is complete, because Core replaces the component", () => {
  const note = { id: "n", text: "本文", x: 12, y: 34, color: "mint", locked: true, revision: 5 };
  const payload = notePayload(note, { x: 40 });
  assert.deepEqual(payload, {
    component_key: NOTE_COMPONENT, kind: "object", visibility: "global",
    text: "本文", color: "mint", locked: true, position_x: 40, position_y: 34,
  });
  for (const key of ["text", "color", "locked", "position_x", "position_y"]) {
    assert.equal(Object.hasOwn(payload, key), true, `${key} must always be sent`);
  }
});

test("a move keeps the lock flag so the update cannot silently unlock a note", () => {
  const locked = { id: "n", text: "t", x: 0, y: 0, color: "yellow", locked: true, revision: 3 };
  assert.equal(notePayload(locked, { x: 9, y: 9 }).locked, true);
  assert.equal(notePayload(locked, { locked: false }).locked, false);
  assert.equal(notePayload({ ...locked, locked: undefined }).locked, false);
});

test("a lock command confirmed by Core is what flips the local lock state", () => {
  const state = createWhiteboardState();
  applyEntityCommand(state, { entityId: "n", operation: "spawn", expectedRevision: 1,
    arguments: notePayload({ text: "t", x: 5, y: 6, color: "rose", locked: false }) });
  assert.equal(state.notes.get("n").locked, false);
  applyEntityCommand(state, { entityId: "n", operation: "update", expectedRevision: 2,
    arguments: notePayload(state.notes.get("n"), { locked: true }) });
  assert.equal(state.notes.get("n").locked, true);
  assert.equal(state.notes.get("n").x, 5);
  // A denied command produces no confirmation, so the lock survives.
  assert.equal(applyEntityCommand(state, { entityId: "n", operation: "update", expectedRevision: 2,
    arguments: notePayload(state.notes.get("n"), { locked: false }) }), false);
  assert.equal(state.notes.get("n").locked, true);
});

test("a snapshot restores the lock flag from the authoritative component", () => {
  const state = createWhiteboardState();
  applyEntity(state, { entity_id: "n", revision: 9, properties: { [NOTE_COMPONENT]: {
    encoding: "json", value: notePayload({ text: "t", x: 1, y: 2, color: "blue", locked: true }) } } });
  assert.equal(state.notes.get("n").locked, true);
  assert.equal(state.notes.get("n").revision, 9);
});
