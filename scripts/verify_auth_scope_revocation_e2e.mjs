/**
 * ADR-026 T26/T27: writes and resume stop the moment a subject loses its
 * standing, with no pre-commit extension registered.
 *
 * This is the case the design review caught. The pre-commit hook re-resolves
 * permissions, but only when an active extension holds the matching
 * capability, and it never sees transform updates at all. So a check that
 * lived there would not run in a default deployment — which is the one this
 * script uses. Nothing is registered here.
 *
 * The subject's standing is withdrawn in the database mid-session, the way an
 * operator changing configuration or a deadline passing would, and the same
 * connection is then used to write.
 *
 * Usage:
 *   node verify_auth_scope_revocation_e2e.mjs <baseUrl> <containerName> <dbName>
 */

import { execFileSync } from "node:child_process";
import { OrbiSyncClient, uuidv7 } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const CONTAINER = process.argv[3] ?? "orbisync-auth26-test-db";
const DB = process.argv[4] ?? "orbisync_auth26";
const INSTANCE = process.env.ORBISYNC_E2E_INSTANCE;

if (!INSTANCE) {
  console.error("set ORBISYNC_E2E_INSTANCE to the permitted instance id");
  process.exit(2);
}

let passed = 0;
let failed = 0;
const failures = [];

function check(name, condition, detail = "") {
  if (condition) {
    passed += 1;
    console.log(`  PASS  ${name}`);
  } else {
    failed += 1;
    failures.push(`${name}${detail ? ` — ${detail}` : ""}`);
    console.log(`  FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

/** Runs SQL against the isolated verification database. */
function sql(statement) {
  return execFileSync(
    "docker",
    ["exec", CONTAINER, "psql", "-U", "orbisync", "-d", DB, "-tAc", statement],
    { encoding: "utf8" },
  ).trim();
}

function subjectOf(accessToken) {
  const payload = accessToken.split(".")[1];
  return JSON.parse(
    Buffer.from(payload.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString(),
  ).sub;
}

const settle = () => new Promise((r) => setTimeout(r, 900));

async function main() {
  console.log(`ADR-026 scope revocation, no pre-commit extension registered\n`);

  // Clear any registration left behind by an earlier run: this scenario is
  // specifically about the deployment where no hook is in play.
  sql("DELETE FROM extension_registrations");
  const registrations = sql(
    "SELECT count(*) FROM extension_registrations WHERE status = 'active'",
  );
  check(
    "no active extension is registered, so the hook path is not in play",
    registrations === "0",
    `active registrations: ${registrations}`,
  );

  const guest = new OrbiSyncClient({ baseUrl: BASE });
  await guest.auth.guest();
  const subject = subjectOf(guest._getAccessToken());
  const conn = await guest.connect();
  const instance = await conn.join(INSTANCE);

  const errors = [];
  instance.on("error", (e) => errors.push(e));

  // A transform while the subject still has standing, to establish the
  // baseline: this same call must be refused after the withdrawal.
  const entityId = uuidv7();
  instance.sendEntityCommand({
    entityId,
    operation: "spawn",
    expectedRevision: 0,
    args: { kind: "object", visibility: "global", transform: { position_x: 1, position_y: 0, position_z: 1 } },
  });
  await settle();
  check("a spawn is accepted while the subject has standing", errors.length === 0, JSON.stringify(errors[0] ?? {}).slice(0, 140));

  instance.sendTransform({
    entityId,
    position: { x: 2, y: 0, z: 2 },
    rotation: { x: 0, y: 0, z: 0, w: 1 },
    expectedRevision: 1,
  });
  await settle();
  check("a transform is accepted while the subject has standing", errors.length === 0, JSON.stringify(errors[0] ?? {}).slice(0, 140));

  // Withdraw the subject's standing on the same connection: its deadline is
  // moved into the past, exactly as time passing would do.
  // Both timestamps move back: the table requires expires_at > created_at, so
  // backdating only the deadline would be rejected by the schema.
  sql(
    `UPDATE ephemeral_subjects SET created_at = now() - interval '2 hours', ` +
      `expires_at = now() - interval '1 hour' WHERE user_id = '${subject}'`,
  );
  const stillActive = sql(
    `SELECT count(*) FROM auth_sessions WHERE user_id = '${subject}' AND status = 'active'`,
  );
  check(
    "the revocation pass has not run, so only the deadline check can refuse",
    stillActive !== "0",
    `active sessions: ${stillActive}`,
  );

  errors.length = 0;
  instance.sendEntityCommand({
    entityId,
    operation: "update",
    expectedRevision: 2,
    args: { components: { "app.note": "aGVsbG8=" } },
  });
  await settle();
  check(
    "an entity command is refused once the subject has expired",
    errors.length > 0,
    errors.length === 0 ? "the command was accepted" : JSON.stringify(errors[0]).slice(0, 140),
  );

  errors.length = 0;
  instance.sendTransform({
    entityId,
    position: { x: 9, y: 0, z: 9 },
    rotation: { x: 0, y: 0, z: 0, w: 1 },
  });
  await settle();
  // The transform branch never reaches the pre-commit arm, so this is the
  // case that would have been missed entirely.
  check(
    "a transform is refused once the subject has expired",
    errors.length > 0,
    errors.length === 0 ? "the transform was accepted" : JSON.stringify(errors[0]).slice(0, 140),
  );

  // T27: a fresh connection must not get back in either, and the refusal has
  // to happen at join rather than being deferred.
  let rejoinError = null;
  try {
    const secondConn = await guest.connect();
    await secondConn.join(INSTANCE);
  } catch (e) {
    rejoinError = e instanceof Error ? e.message : String(e);
  }
  check("an expired subject cannot join again", rejoinError !== null, rejoinError ?? "join succeeded");

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) {
    console.log("\nfailures:");
    for (const f of failures) console.log(`  - ${f}`);
    process.exit(1);
  }
  process.exit(0);
}

main().catch((e) => {
  console.error("verification aborted:", e);
  process.exit(2);
});
