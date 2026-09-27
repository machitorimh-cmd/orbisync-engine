import assert from "node:assert/strict";
import { test } from "node:test";
import { uuidv7 } from "./uuidv7.js";

test("uuidv7 creates canonical UUIDv7 identifiers", () => {
  const value = uuidv7();
  assert.match(value, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
});
