export const NOTE_COMPONENT = "com.orbisync.whiteboard.note";

// Core stores the whole EntityCommand argument struct as the component payload
// and replaces it on every update, so a command must always carry the complete
// note. Omitting `locked` would silently unlock the note, and the pre-commit
// rule reads exactly this field as the lock authority.
export function notePayload(note, changes = {}) {
  const next = { ...note, ...changes };
  return {
    component_key: NOTE_COMPONENT,
    kind: "object",
    visibility: "global",
    text: next.text,
    color: next.color,
    locked: next.locked === true,
    position_x: next.x,
    position_y: next.y,
  };
}

export function createWhiteboardState() {
  return { notes: new Map(), pendingCommands: new Map(), deletedRevisions: new Map() };
}

function noteFromEntity(entity) {
  const fields = entity.properties?.fields?.[NOTE_COMPONENT]?.structValue?.fields?.value?.structValue?.fields ?? {};
  const envelope = entity.properties?.[NOTE_COMPONENT];
  const json = envelope?.encoding === "json" ? envelope.value : {};
  const transform = entity.transform ?? {};
  return {
    id: entity.entity_id ?? entity.entityId ?? "",
    text: json?.text ?? fields.text?.stringValue ?? "新しい付箋",
    x: json?.position_x ?? fields.position_x?.numberValue ?? transform.position_x ?? transform.positionX ?? 240,
    y: json?.position_y ?? fields.position_y?.numberValue ?? transform.position_y ?? transform.positionY ?? 140,
    color: json?.color ?? fields.color?.stringValue ?? "yellow",
    locked: json?.locked ?? fields.locked?.boolValue ?? false,
    revision: Number(entity.revision ?? 0),
  };
}

export function applyEntity(state, entity) {
  const note = noteFromEntity(entity);
  if (!note.id) return false;
  if (note.revision <= (state.deletedRevisions.get(note.id) ?? -1)) return false;
  const current = state.notes.get(note.id);
  if (current && note.revision < current.revision) return false;
  state.notes.set(note.id, note);
  state.pendingCommands.delete(note.id);
  return true;
}

// Only received Core confirmations belong here. Their expectedRevision field
// contains the resulting revision, unlike an outgoing client command.
export function applyEntityCommand(state, command) {
  const entityId = command.entityId;
  if (!entityId) return false;
  const revision = command.expectedRevision == null ? NaN : Number(command.expectedRevision);
  if (!Number.isSafeInteger(revision) || revision <= 0) return false;
  if (revision <= (state.deletedRevisions.get(entityId) ?? -1)) return false;
  const current = state.notes.get(entityId);
  if (current && revision <= current.revision) return false;
  if (command.operation === "delete") {
    state.notes.delete(entityId);
    state.deletedRevisions.set(entityId, revision);
    state.pendingCommands.delete(entityId);
    return true;
  }
  if (command.operation !== "spawn" && command.operation !== "update") return false;
  // protoc-gen-es represents google.protobuf.Struct as a plain JsonObject.
  const args = command.arguments ?? {};
  const fields = args.fields ?? {};
  const note = current ?? {
    id: entityId, text: "新しい付箋", x: 240, y: 140,
    color: "yellow", locked: false, revision: 0,
  };
  state.notes.set(entityId, {
    ...note,
    text: args.text ?? fields.text?.stringValue ?? note.text,
    color: args.color ?? fields.color?.stringValue ?? note.color,
    locked: args.locked ?? fields.locked?.boolValue ?? note.locked,
    x: args.position_x ?? fields.position_x?.numberValue ?? note.x,
    y: args.position_y ?? fields.position_y?.numberValue ?? note.y,
    revision,
  });
  state.pendingCommands.delete(entityId);
  return true;
}
