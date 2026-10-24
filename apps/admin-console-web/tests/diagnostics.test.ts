import { it } from "node:test";
import assert from "node:assert/strict";
import { diagnosticWarnings } from "../src/diagnostics.js";

import { diagnosticNow as now, diagnosticFixture as fixture } from "./diagnostics-fixture.js";

it("normal grace retention produces no Presence anomaly", () => {
  assert.deepEqual(diagnosticWarnings(fixture(), undefined, now), []);
});
it("orphan membership and overlapping owner are separate anomalies", () => {
  const snapshot = fixture(); snapshot.presence.memberships_without_binding = 1; snapshot.presence.connection_ownership_mismatch = 1;
  const warnings = diagnosticWarnings(snapshot, undefined, now);
  assert.equal(warnings.length, 2); assert.match(warnings[0], /所有記録のない/); assert.match(warnings[1], /参加済み接続/);
});
it("stale samples and old persistence are visible with configurable thresholds", () => {
  const snapshot = fixture(); snapshot.persistence.pending = 1; snapshot.persistence.oldest_pending_seconds = 90;
  assert.equal(diagnosticWarnings(snapshot, { pendingAgeSeconds: 120, staleSeconds: 45 }, now).length, 0);
  snapshot.presence.observed_at = new Date(now - 60_000).toISOString();
  assert.equal(diagnosticWarnings(snapshot, { pendingAgeSeconds: 60, staleSeconds: 45 }, now).length, 2);
  snapshot.presence.observed_at = null;
  assert.match(diagnosticWarnings(snapshot, undefined, now)[0], /観測/);
});
