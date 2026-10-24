/** One bounded, explicit page at a time. Reset invalidates in-flight responses. */
export class CursorPages {
  items = [];
  nextCursor = null;
  busy = false;
  started = false;
  #version = 0;
  #flight = null;
  #seen = new Set();
  constructor(maxItems = 10_000) { this.maxItems = maxItems; }
  reset() {
    this.#version++;
    this.items = []; this.nextCursor = null; this.busy = false; this.started = false;
    this.#flight = null; this.#seen.clear();
  }
  upsert(item) {
    const merged = new Map(this.items.map(item => [item.id, item]));
    merged.set(item.id, item);
    if (merged.size > this.maxItems) throw new Error("一覧の保持上限です。条件を絞ってください");
    this.items = [...merged.values()];
  }
  load(fetchPage) {
    if (this.#flight) return this.#flight;
    if (this.started && !this.nextCursor) return Promise.resolve(false);
    if (this.#seen.size >= 1000) return Promise.reject(new Error("ページ数の保持上限です。一覧を更新してください"));
    const version = this.#version, cursor = this.nextCursor;
    this.busy = true;
    const flight = Promise.resolve().then(() => version === this.#version ? fetchPage(cursor) : null).then(page => {
      if (version !== this.#version) return false;
      if (!page || !Array.isArray(page.items) || page.items.length > 100
        || page.items.some(item => !item || typeof item.id !== "string" || !item.id || item.id.length > 256)) throw new Error("一覧応答が正しくありません");
      const next = page.next_cursor ?? null;
      if (next !== null && (typeof next !== "string" || !next || next.length > 4096)) throw new Error("pagination cursorが正しくありません");
      if (next && (next === cursor || this.#seen.has(next))) throw new Error("pagination cursorが循環しました");
      const merged = new Map(this.items.map(item => [item.id, item]));
      for (const item of page.items) merged.set(item.id, item);
      if (merged.size > this.maxItems) throw new Error("一覧の保持上限です。条件を絞ってください");
      this.items = [...merged.values()]; this.nextCursor = next; this.started = true;
      if (next) this.#seen.add(next);
      return true;
    }).catch(error => { if (version !== this.#version) return false; throw error; }).finally(() => {
      if (version === this.#version) { this.busy = false; this.#flight = null; }
    });
    this.#flight = flight;
    return flight;
  }
}
/** Exactly one bounded page; opaque cursors are encoded without interpretation. */
export function pagePath(resource, cursor = null) {
  if (resource !== "worlds" && resource !== "instances") throw new Error("Invalid resource");
  const query = new URLSearchParams({ limit: "100" });
  if (cursor !== null) query.set("cursor", cursor);
  return `/v1/${resource}?${query}`;
}
