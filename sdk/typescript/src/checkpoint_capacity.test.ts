import assert from "node:assert/strict";
import { test } from "node:test";
import { checkpointCapacityPhase } from "./checkpoint_capacity.js";

test("checkpoint capacity distinguishes admission retry from completed replay", () => {
  assert.equal(checkpointCapacityPhase({ code: "CHECKPOINT_CAPACITY", retryable: true }), "not_admitted");
  assert.equal(checkpointCapacityPhase({ code: "CHECKPOINT_CAPACITY", retryable: false }), "completed");
  assert.equal(checkpointCapacityPhase({ code: "PERSISTENCE_UNAVAILABLE", retryable: true }), undefined);
});
