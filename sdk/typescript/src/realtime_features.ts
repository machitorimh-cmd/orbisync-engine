import { SyncError, stateBytes } from "./sync_state.js";

export const DIRECTED_EVENTS_FEATURE = "orbisync.directed-events.v1";
export const ACTIVITY_FEATURE = "orbisync.activity.v1";
export type RealtimeJsonValue = null | boolean | string | number | readonly RealtimeJsonValue[] | RealtimeJsonObject;
export type RealtimeJsonObject = { readonly [key: string]: RealtimeJsonValue };

export function canonicalId(value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value)) throw new SyncError("INVALID_UPDATE");
  return value;
}
export function activityKey(value: string): string {
  if (typeof value !== "string" || value.length > 128 || !/^[a-z0-9_-]+(?:\.[a-z0-9_-]+)+$/.test(value)
    || /^(core|orbisync|system|admin)\./.test(value)) throw new SyncError("INVALID_UPDATE");
  return value;
}
export function customEventType(value: string): string {
  if (typeof value !== "string" || value.length > 128 || !/^custom\.[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*$/.test(value)) throw new SyncError("INVALID_UPDATE");
  return value;
}

/** Validate before copying: JSON serialization must never invoke a caller's toJSON. */
export function ownJson(value: unknown, maxBytes: number): RealtimeJsonObject {
  stateBytes(value, Math.max(maxBytes * 16, 65_536), false);
  const validate = (item: unknown): void => {
    if (item && typeof item === "object") {
      if (!Array.isArray(item) && ![Object.prototype, null].includes(Object.getPrototypeOf(item))) throw new SyncError("INVALID_UPDATE");
      for (const child of Object.values(item)) validate(child);
    }
  };
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new SyncError("INVALID_UPDATE");
  validate(value);
  const encoded = JSON.stringify(value);
  if (new TextEncoder().encode(encoded).byteLength > maxBytes) throw new SyncError("RESOURCE_LIMIT");
  const freeze = (item: unknown): void => {
    if (item && typeof item === "object") { for (const child of Object.values(item)) freeze(child); Object.freeze(item); }
  };
  const result = JSON.parse(encoded) as RealtimeJsonObject;
  freeze(result);
  return result;
}
