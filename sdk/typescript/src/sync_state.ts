import type { EntityCommand, StateDelta } from "./generated/orbisync/v1/realtime_pb.js";

export const STATE_SYNC_FEATURE = "orbisync.state-sync.v1";
export type SyncErrorCode = "INVALID_SNAPSHOT" | "INVALID_UPDATE" | "REVISION_CONFLICT" | "RESOURCE_LIMIT"
  | "UNSUPPORTED_SERVER" | "SYNC_TIMEOUT" | "DISCONNECTED" | "CLOSED" | "ABORTED" | "NOT_READY"
  | "AUTHENTICATION_FAILED" | "RECOVERY_EXHAUSTED" | "HANDLER_FAILED";
export class SyncError extends Error {
  constructor(readonly code: SyncErrorCode) {
    super(`OrbiSync synchronization: ${code}`);
    this.name = "SyncError";
  }
}

export type StateValue = null | boolean | string | number | bigint | StateValue[] | { [key: string]: StateValue };
export type StateObject = { [key: string]: StateValue };
export type SyncedEntity = {
  entityId: string;
  revision: bigint;
  instanceRevision: bigint;
  kind?: string;
  transform?: StateObject;
  velocity?: StateObject;
  animation?: StateObject;
  presence?: StateObject;
  properties: StateObject;
};
export type InstanceState = {
  revision: bigint;
  entities: ReadonlyMap<string, SyncedEntity>;
  presences: ReadonlyMap<string, StateObject>;
  metadata: StateObject;
};
export type StateEvent = { event: "entityUpdated" | "entitySpawned" | "entityDeleted"; payload: SyncedEntity | { entityId: string } };

/** Strict, depth-bounded JSON decoding. Integer tokens never pass through a lossy number. */
export function parseStateJson(text: string): StateValue {
  let offset = 0;
  let nodes = 0;
  const bad = (): never => { throw new SyncError("INVALID_SNAPSHOT"); };
  const whitespace = (): void => { while (/[\x20\t\r\n]/.test(text[offset] ?? "!")) offset++; };
  const string = (): string => {
    const start = offset++;
    while (offset < text.length) {
      if (text[offset] === "\\") { offset += 2; continue; }
      if (text[offset++] === '"') {
        try { return JSON.parse(text.slice(start, offset)) as string; } catch { return bad(); }
      }
    }
    return bad();
  };
  const value = (depth: number): StateValue => {
    if (depth > 64 || ++nodes > 1_000_000) throw new SyncError("RESOURCE_LIMIT");
    whitespace();
    const char = text[offset];
    if (char === '"') return string();
    if (char === "{" || char === "[") {
      offset++;
      const object = char === "{";
      const result: StateObject | StateValue[] = object ? Object.create(null) as StateObject : [];
      const end = object ? "}" : "]";
      whitespace();
      if (text[offset] === end) { offset++; return result; }
      while (true) {
        whitespace();
        if (object) {
          if (text[offset] !== '"') return bad();
          const key = string();
          if (Object.hasOwn(result, key)) return bad();
          whitespace();
          if (text[offset++] !== ":") return bad();
          (result as StateObject)[key] = value(depth + 1);
        } else (result as StateValue[]).push(value(depth + 1));
        whitespace();
        const next = text[offset++];
        if (next === end) return result;
        if (next !== ",") return bad();
      }
    }
    for (const [token, primitive] of [["true", true], ["false", false], ["null", null]] as const) {
      if (text.startsWith(token, offset)) { offset += token.length; return primitive; }
    }
    const match = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(text.slice(offset));
    if (!match) return bad();
    const token = match[0];
    if (token.length > 1024) throw new SyncError("RESOURCE_LIMIT");
    offset += token.length;
    if (/^-?\d+$/.test(token)) {
      const exact = BigInt(token);
      return exact > BigInt(Number.MAX_SAFE_INTEGER) || exact < BigInt(Number.MIN_SAFE_INTEGER) ? exact : Number(token);
    }
    const numeric = Number(token);
    if (!Number.isFinite(numeric)) return bad();
    // Integral exponent/decimal values beyond the safe range cannot be used
    // as revisions, nor silently rounded in custom state.
    if (Number.isInteger(numeric) && !Number.isSafeInteger(numeric)) return bad();
    return numeric;
  };
  const result = value(0);
  whitespace();
  if (offset !== text.length) return bad();
  return result;
}

function object(value: unknown, code: SyncErrorCode = "INVALID_SNAPSHOT"): StateObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new SyncError(code);
  return value as StateObject;
}
function id(value: unknown, code: SyncErrorCode = "INVALID_SNAPSHOT"): string {
  if (typeof value !== "string" || !value || value.length > 256) throw new SyncError(code);
  return value;
}
export function uint64(value: unknown, code: SyncErrorCode = "INVALID_SNAPSHOT"): bigint {
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) throw new SyncError(code);
    value = BigInt(value);
  }
  if (typeof value !== "bigint" || value < 0n || value > 0xffff_ffff_ffff_ffffn) throw new SyncError(code);
  return value;
}

export function equalState(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (a === null || b === null || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ak = Object.keys(a), bk = Object.keys(b);
  return ak.length === bk.length && ak.every(key => Object.hasOwn(b, key)
    && equalState((a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key]));
}

function properties(value: unknown): StateObject {
  const result = value == null ? Object.create(null) as StateObject : object(value);
  if (Object.keys(result).length > 1024) throw new SyncError("RESOURCE_LIMIT");
  for (const [key, component] of Object.entries(result)) {
    if (!key.includes(".") || key.startsWith("core.")) throw new SyncError("INVALID_SNAPSHOT");
    const envelope = object(component);
    if (!Object.hasOwn(envelope, "value") || !["json", "base64"].includes(String(envelope.encoding))) throw new SyncError("INVALID_SNAPSHOT");
    if (envelope.encoding === "base64" && (typeof envelope.value !== "string"
      || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(envelope.value))) throw new SyncError("INVALID_SNAPSHOT");
  }
  return result;
}

/** Conservative retained-value budget, including UTF-16 strings and container entries. */
export function stateBytes(value: unknown, limit = 32 * 1024 * 1024, allowBigInt = true): number {
  let bytes = 0, nodes = 0;
  const visit = (value: unknown, depth: number): void => {
    if (depth > 64 || ++nodes > 1_000_000) throw new SyncError("RESOURCE_LIMIT");
    bytes += 16;
    if (typeof value === "string") bytes += value.length * 2;
    else if (typeof value === "number") {
      if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value))) throw new SyncError("INVALID_UPDATE");
    } else if (typeof value === "bigint") {
      if (!allowBigInt) throw new SyncError("INVALID_UPDATE");
    } else if (value !== null && typeof value === "object") {
      for (const [key, item] of Object.entries(value)) {
        bytes += 24 + key.length * 2;
        if (bytes > limit) throw new SyncError("RESOURCE_LIMIT");
        visit(item, depth + 1);
      }
    } else if (value !== null && typeof value !== "boolean") throw new SyncError("INVALID_UPDATE");
    if (bytes > limit) throw new SyncError("RESOURCE_LIMIT");
  };
  visit(value, 0);
  return bytes;
}

type EntityRecord = { value: SyncedEntity; fields: Map<string, bigint>; birth: bigint; propertiesFloor: bigint; bytes: number };

/** A synchronous transaction's write set; the base is untouched until commit. */
class StagedMap<V> {
  private changes = new Map<string, V | undefined>();
  private removed = new Set<string>();
  size: number;
  constructor(private readonly base: Map<string, V>) { this.size = base.size; }
  get(key: string): V | undefined { return this.changes.has(key) ? this.changes.get(key) : this.base.get(key); }
  has(key: string): boolean { return this.get(key) !== undefined; }
  set(key: string, value: V): this {
    if (!this.has(key)) {
      this.size++;
      // Reinsertion must retain native Map insertion order.
      this.changes.delete(key);
    }
    this.changes.set(key, value);
    return this;
  }
  delete(key: string): boolean {
    if (!this.has(key)) return false;
    this.size--;
    this.removed.add(key);
    this.changes.set(key, undefined);
    return true;
  }
  commit(): void {
    for (const key of this.removed) this.base.delete(key);
    for (const [key, value] of this.changes) {
      if (value !== undefined) this.base.set(key, value);
    }
  }
  *[Symbol.iterator](): IterableIterator<[string, V]> {
    for (const [key, value] of this.base) {
      if (!this.removed.has(key)) yield [key, this.changes.get(key) ?? value];
    }
    for (const [key, value] of this.changes) {
      if (value !== undefined && (this.removed.has(key) || !this.base.has(key))) yield [key, value];
    }
  }
}

/** Canonical state only; callers stage this store until a complete snapshot is validated. */
export class SyncStore {
  private entities: Map<string, EntityRecord> | StagedMap<EntityRecord> = new Map();
  private presences = new Map<string, StateObject>();
  private metadata: StateObject = {};
  private tombstones: Map<string, bigint> | StagedMap<bigint> = new Map();
  private copyOnWrite = false;
  private owned = new Set<string>();
  private retainedBytes = 0;
  constructor(private readonly maxRetainedBytes = 32 * 1024 * 1024) {}
  revision = 0n;
  boundary = 0n;

  get state(): InstanceState {
    return structuredClone({ revision: this.revision,
      entities: new Map([...this.entities].map(([key, record]) => [key, record.value])),
      presences: this.presences, metadata: this.metadata });
  }
  entityRevision(entityId: string): bigint | undefined { return this.entities.get(entityId)?.value.revision; }

  /** Apply a synchronous batch atomically, copying only records it touches. */
  transaction<T>(apply: (staged: SyncStore) => T): T {
    if (!(this.entities instanceof Map) || !(this.tombstones instanceof Map)) throw new SyncError("INVALID_UPDATE");
    const staged = new SyncStore(this.maxRetainedBytes);
    const entities = new StagedMap(this.entities), tombstones = new StagedMap(this.tombstones);
    staged.entities = entities;
    staged.tombstones = tombstones;
    staged.presences = this.presences;
    staged.metadata = this.metadata;
    staged.retainedBytes = this.retainedBytes;
    staged.revision = this.revision;
    staged.boundary = this.boundary;
    staged.copyOnWrite = true;
    const result = apply(staged);
    entities.commit();
    tombstones.commit();
    this.retainedBytes = staged.retainedBytes;
    this.revision = staged.revision;
    return result;
  }

  fork(): SyncStore {
    const next = new SyncStore(this.maxRetainedBytes);
    next.retainedBytes = this.retainedBytes;
    next.entities = new Map(this.entities);
    next.presences = this.presences;
    next.metadata = this.metadata;
    next.tombstones = new Map(this.tombstones);
    next.revision = this.revision;
    next.boundary = this.boundary;
    next.copyOnWrite = true;
    return next;
  }

  static snapshot(bytes: Uint8Array, boundary: bigint, instanceId: string, maxRetainedBytes = 32 * 1024 * 1024): SyncStore {
    let parsed: StateObject;
    try { parsed = object(parseStateJson(new TextDecoder("utf-8", { fatal: true }).decode(bytes))); }
    catch (error) { throw error instanceof SyncError ? error : new SyncError("INVALID_SNAPSHOT"); }
    if (parsed.format !== "orbisync.snapshot.v1" || uint64(parsed.revision) !== boundary
      || object(parsed.instance).instance_id !== instanceId || !Array.isArray(parsed.entities) || !Array.isArray(parsed.users)) {
      throw new SyncError("INVALID_SNAPSHOT");
    }
    if (parsed.entities.length > 65_536 || parsed.users.length > 65_536) throw new SyncError("RESOURCE_LIMIT");
    const store = new SyncStore(maxRetainedBytes);
    store.revision = store.boundary = boundary;
    const { entities, users, ...metadata } = parsed;
    store.metadata = metadata;
    for (const raw of entities as StateValue[]) {
      const item = object(raw), entityId = id(item.entity_id);
      if (store.entities.has(entityId)) throw new SyncError("INVALID_SNAPSHOT");
      const record = store.ensure(entityId, boundary);
      record.value.revision = uint64(item.revision);
      if (typeof item.kind !== "string") throw new SyncError("INVALID_SNAPSHOT");
      record.value.kind = item.kind;
      record.fields.set("kind", boundary);
      for (const key of ["transform", "velocity", "animation", "presence"] as const) {
        if (item[key] != null) {
          let data = object(item[key]);
          if (key === "transform") {
            const position = object(data.position), rotation = object(data.rotation);
            data = { positionX: position.x!, positionY: position.y!, positionZ: position.z!,
              rotationX: rotation.x!, rotationY: rotation.y!, rotationZ: rotation.z!, rotationW: rotation.w!,
              ...(data.scale === undefined ? {} : { scale: data.scale }) };
            if (Object.entries(data).some(([field, value]) => field !== "scale" && (typeof value !== "number" || !Number.isFinite(value)))) throw new SyncError("INVALID_SNAPSHOT");
          }
          record.value[key] = data; record.fields.set(key, boundary);
        }
      }
      record.value.properties = properties(item.properties);
      for (const key of Object.keys(record.value.properties)) record.fields.set(`p:${key}`, boundary);
    }
    for (const raw of users as StateValue[]) {
      const presence = object(raw), presenceId = id(presence.presence_id);
      if (store.presences.has(presenceId)) throw new SyncError("INVALID_SNAPSHOT");
      store.presences.set(presenceId, presence);
    }
    store.retainedBytes = stateBytes(metadata, maxRetainedBytes) + stateBytes([...store.presences], maxRetainedBytes);
    store.checkBudget();
    return store;
  }

  private ensure(entityId: string, boundary: bigint): EntityRecord {
    let record = this.entities.get(entityId);
    if (record && this.copyOnWrite && !this.owned.has(entityId)) {
      record = structuredClone(record);
      this.entities.set(entityId, record);
    }
    this.owned.add(entityId);
    if (!record) {
      if (this.entities.size >= 65_536) throw new SyncError("RESOURCE_LIMIT");
      record = { value: { entityId, revision: 0n, instanceRevision: boundary, properties: Object.create(null) as StateObject }, fields: new Map(), birth: this.boundary, propertiesFloor: this.boundary, bytes: 0 };
      this.entities.set(entityId, record);
    }
    return record;
  }

  private set(record: EntityRecord, key: string, value: StateValue | undefined, boundary: bigint): boolean {
    const fieldRevision = record.fields.get(key) ?? record.birth;
    const previousRevision = key.startsWith("p:") && record.propertiesFloor > fieldRevision ? record.propertiesFloor : fieldRevision;
    if (boundary < previousRevision) return false;
    const target = key.startsWith("p:") ? record.value.properties : record.value as unknown as StateObject;
    const field = key.startsWith("p:") ? key.slice(2) : key;
    const previous = target[field];
    if (boundary === previousRevision && (record.fields.has(key) || (key.startsWith("p:") && record.propertiesFloor === boundary))) {
      if (!equalState(previous, value)) throw new SyncError("REVISION_CONFLICT");
      return false;
    }
    if (record.fields.size >= 1030 && !record.fields.has(key)) throw new SyncError("RESOURCE_LIMIT");
    record.fields.set(key, boundary);
    if (value === undefined) delete target[field]; else target[field] = structuredClone(value);
    return !equalState(previous, value);
  }

  applyDelta(delta: StateDelta): StateEvent[] {
    const boundary = uint64(delta.toRevision, "INVALID_UPDATE");
    if (uint64(delta.fromRevision, "INVALID_UPDATE") > boundary) throw new SyncError("INVALID_UPDATE");
    if (boundary <= this.boundary) return [];
    const events: StateEvent[] = [];
    for (const item of delta.entities) {
      const entityId = id(item.entityId, "INVALID_UPDATE"), entityRevision = uint64(item.revision, "INVALID_UPDATE");
      if (boundary <= (this.tombstones.get(entityId) ?? -1n)) continue;
      const record = this.ensure(entityId, boundary);
      if (boundary < record.birth) continue;
      let changed = false;
      for (const key of ["transform", "velocity", "animation", "presence"] as const) {
        const wire = item[key];
        if (wire !== undefined) {
          const { $typeName: _, $unknown: __, ...data } = wire;
          const state = data as StateObject;
          if (key === "transform" && record.value.transform?.scale !== undefined) state.scale = record.value.transform.scale;
          changed = this.set(record, key, state, boundary) || changed;
        }
      }
      if (item.properties !== undefined) {
        const incoming = properties(item.properties);
        for (const key of new Set([...Object.keys(record.value.properties), ...Object.keys(incoming)])) {
          changed = this.set(record, `p:${key}`, incoming[key], boundary) || changed;
        }
        record.propertiesFloor = boundary > record.propertiesFloor ? boundary : record.propertiesFloor;
      }
      if (boundary === record.value.instanceRevision && record.value.revision !== 0n && record.value.revision !== entityRevision) throw new SyncError("REVISION_CONFLICT");
      if (boundary >= record.value.instanceRevision) {
        record.value.revision = entityRevision;
        record.value.instanceRevision = boundary;
      }
      if (changed) events.push({ event: "entityUpdated", payload: structuredClone(record.value) });
    }
    this.revision = this.revision > boundary ? this.revision : boundary;
    this.checkBudget();
    return events;
  }

  applyCommand(command: EntityCommand): StateEvent[] {
    if (command.instanceRevision === undefined) throw new SyncError("UNSUPPORTED_SERVER");
    const boundary = uint64(command.instanceRevision, "INVALID_UPDATE");
    const entityId = id(command.entityId, "INVALID_UPDATE");
    if (!["spawn", "update", "delete"].includes(command.operation)) throw new SyncError("INVALID_UPDATE");
    if (boundary <= this.boundary || boundary <= (this.tombstones.get(entityId) ?? -1n)) return [];
    const revision = uint64(command.expectedRevision, "INVALID_UPDATE");
    this.revision = this.revision > boundary ? this.revision : boundary;
    if (command.operation === "delete") {
      if (revision !== boundary) throw new SyncError("INVALID_UPDATE");
      if (this.tombstones.size >= 4096 && !this.tombstones.has(entityId)) throw new SyncError("RESOURCE_LIMIT");
      this.tombstones.set(entityId, boundary);
      const existing = this.entities.get(entityId);
      if (!existing) { this.checkBudget(); return []; }
      if (existing.value.instanceRevision <= boundary) {
        this.retainedBytes -= existing.bytes;
        this.entities.delete(entityId);
        this.checkBudget();
        return [{ event: "entityDeleted", payload: { entityId } }];
      }
      this.clearOlderFields(this.ensure(entityId, boundary), boundary);
      this.checkBudget();
      return [];
    }
    const record = this.ensure(entityId, boundary);
    if (boundary < record.birth) return [];
    const args = object(command.arguments, "INVALID_UPDATE");
    let changed = false;
    if (command.operation === "spawn") {
      if (boundary > record.birth) {
        this.clearOlderFields(record, boundary);
        record.birth = boundary;
        record.propertiesFloor = record.propertiesFloor > boundary ? record.propertiesFloor : boundary;
      }
      if (typeof args.kind !== "string") throw new SyncError("INVALID_UPDATE");
      changed = this.set(record, "kind", args.kind.toLowerCase(), boundary);
      const transformArgs = args.transform !== null && typeof args.transform === "object" && !Array.isArray(args.transform) ? args.transform : args;
      if ([transformArgs.position_x, transformArgs.position_y, transformArgs.position_z].some(value => typeof value === "number")) {
        changed = this.set(record, "transform", {
          positionX: typeof transformArgs.position_x === "number" ? transformArgs.position_x : 0,
          positionY: typeof transformArgs.position_y === "number" ? transformArgs.position_y : 0,
          positionZ: typeof transformArgs.position_z === "number" ? transformArgs.position_z : 0,
          rotationX: 0, rotationY: 0, rotationZ: 0, rotationW: 1, scale: { x: 1, y: 1, z: 1 },
        }, boundary) || changed;
      }
      // Arbitrary spawn arguments are echoed, NOT persisted custom components.
    } else {
      const key = id(args.component_key ?? args.key ?? "entity.component", "INVALID_UPDATE");
      if (!key.includes(".") || key.startsWith("core.")) throw new SyncError("INVALID_UPDATE");
      changed = this.set(record, `p:${key}`, { encoding: "json", value: args }, boundary);
    }
    if (boundary === record.value.instanceRevision && record.value.revision !== 0n && record.value.revision !== revision) throw new SyncError("REVISION_CONFLICT");
    if (boundary >= record.value.instanceRevision) {
      record.value.instanceRevision = boundary;
      record.value.revision = revision;
    }
    this.checkBudget();
    return changed ? [{ event: command.operation === "spawn" ? "entitySpawned" : "entityUpdated", payload: structuredClone(record.value) }] : [];
  }

  private checkBudget(): void {
    for (const id of this.owned) {
      const record = this.entities.get(id);
      if (!record) continue;
      const size = stateBytes(record.value, this.maxRetainedBytes) + 64
        + [...record.fields.keys()].reduce((sum, key) => sum + 40 + key.length * 2, 0);
      this.retainedBytes += size - record.bytes;
      record.bytes = size;
    }
    // IDs are independently bounded to 256 UTF-16 units.
    if (this.retainedBytes + this.tombstones.size * 576 > this.maxRetainedBytes) throw new SyncError("RESOURCE_LIMIT");
  }

  private clearOlderFields(record: EntityRecord, boundary: bigint): void {
    for (const [key, stamp] of record.fields) {
      if (stamp <= boundary) {
        if (key.startsWith("p:")) delete record.value.properties[key.slice(2)];
        else delete (record.value as unknown as StateObject)[key];
        record.fields.delete(key);
      }
    }
  }
}
