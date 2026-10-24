import { it } from "node:test";
import assert from "node:assert/strict";
import { CursorPages } from "../../shared/cursor_pages.mjs";
import { resourcePage } from "./resources.js";

for (const resource of ["worlds", "instances"] as const) {
  it(`whiteboard ${resource}: SDK fetch passes opaque cursors and reaches item 101`, async () => {
    const requests: string[] = [], opaque = "opaque+cursor/with=&?";
    const client = { async fetch(path: string | URL | Request) {
      requests.push(String(path)); const query = new URL(String(path), "https://example.test").searchParams;
      assert.equal(query.get("limit"), "100"); assert.equal(query.get("cursor"), requests.length === 1 ? null : opaque);
      return new Response(JSON.stringify(query.has("cursor") ? { items: [{ id: "101" }] } : {
        items: Array.from({ length: 100 }, (_, i) => ({ id: String(i + 1) })), next_cursor: opaque,
      }));
    }};
    const pages = new CursorPages<{ id: string }>();
    await pages.load(cursor => resourcePage(client, resource, cursor)); assert.equal(requests.length, 1);
    await pages.load(cursor => resourcePage(client, resource, cursor)); assert.equal(pages.items.length, 101);
    assert.equal(pages.items.at(-1)?.id, "101");
  });
}

it("whiteboard page errors preserve items/cursor for explicit retry", async () => {
  const pages = new CursorPages<{ id: string }>();
  await pages.load(async () => ({ items: [{ id: "1" }], next_cursor: "next" }));
  await assert.rejects(pages.load(cursor => resourcePage({ fetch: async () => new Response("{}", { status: 403 }) }, "instances", cursor)), /HTTP 403/);
  assert.equal(pages.nextCursor, "next"); assert.equal(pages.items.length, 1);
  await pages.load(cursor => resourcePage({ fetch: async () => new Response('{"items":[{"id":"101"}]}') }, "instances", cursor));
  assert.equal(pages.items.length, 2);
});
