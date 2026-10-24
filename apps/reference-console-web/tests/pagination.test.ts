import { it } from "node:test";
import assert from "node:assert/strict";
import type { OrbiSyncClient } from "../../../sdk/typescript/src/client.js";
import { ReferenceApi } from "../src/api.js";
import { CursorPages } from "../../shared/cursor_pages.mjs";

for (const resource of ["worlds", "instances"] as const) {
  it(`reference ${resource}: SDK-authenticated requests preserve opaque cursor and reach item 101`, async () => {
    const requests: string[] = [];
    const cursor = "opaque+cursor/with=&?";
    const client = { async fetch(path: string) {
      requests.push(path);
      const query = new URL(path, "https://example.test").searchParams;
      assert.equal(query.get("limit"), "100");
      assert.equal(query.get("cursor"), requests.length === 1 ? null : cursor);
      return new Response(JSON.stringify(query.has("cursor") ? { items: [{ id: "101" }] } : {
        items: Array.from({ length: 100 }, (_, i) => ({ id: String(i + 1) })), next_cursor: cursor,
      }));
    }} as unknown as OrbiSyncClient;
    const api = new ReferenceApi(client), pages = new CursorPages<{ id: string }>();
    const fetch = (cursor: string | null) => resource === "worlds" ? api.listWorlds(cursor) : api.listInstances(cursor);
    await pages.load(fetch);
    assert.equal(requests.length, 1);
    await pages.load(fetch);
    assert.equal(pages.items.length, 101);
    assert.equal(pages.items.at(-1)?.id, "101");
    assert.equal(new URL(requests[1]!, "https://example.test").pathname, `/v1/${resource}`);
  });
}

it("reference pagination keeps the failed cursor and permits an explicit retry", async () => {
  let fail = false;
  const client = { async fetch() { return fail ? new Response("{}", { status: 503 }) : new Response('{"items":[{"id":"1"}],"next_cursor":"next"}'); } } as unknown as OrbiSyncClient;
  const api = new ReferenceApi(client), pages = new CursorPages<{ id: string }>();
  await pages.load(cursor => api.listWorlds(cursor)); fail = true;
  await assert.rejects(pages.load(cursor => api.listWorlds(cursor)), /HTTP 503/);
  assert.equal(pages.nextCursor, "next"); assert.equal(pages.items.length, 1);
  fail = false; pages.reset(); await pages.load(cursor => api.listWorlds(cursor));
  assert.equal(pages.items.length, 1);
});
