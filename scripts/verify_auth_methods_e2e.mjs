/**
 * ADR-026 end-to-end verification against a running server.
 *
 * Drives the real HTTP and WebSocket paths through the SDK: guest session ->
 * realtime ticket -> WebSocket join -> entity command -> ownership. The server
 * runs with allow_stub_bearer and allow_stub_ticket both false, so nothing
 * here depends on a bypass.
 *
 * Run it against a server configured with guest and name-only enabled:
 *
 *   cd sdk/typescript && npm ci
 *   node --import tsx ../../scripts/verify_auth_methods_e2e.mjs http://127.0.0.1:18099
 *
 * The SDK import resolves through sdk/typescript, so run it from there or
 * point the import at the built package.
 */

import { OrbiSyncClient, AuthMethodDisabledError, uuidv7 } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const PERMITTED_INSTANCE = "01a0abd0-aef0-7165-8603-c16fc87c003a";
const PERMITTED_INSTANCE_2 = "01a0abd0-b6c1-78cc-ae95-094cfbfc8b88";
const OFF_LIMITS_INSTANCE = "01a0abd0-b2d9-79a5-81a8-9b7b8278bf6e";

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

/** Decodes the `sub` claim so the test can compare server-issued identities. */
function subjectOf(accessToken) {
  const payload = accessToken.split(".")[1];
  const json = Buffer.from(payload.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString();
  return JSON.parse(json).sub;
}

async function guestClient() {
  const client = new OrbiSyncClient({ baseUrl: BASE });
  await client.auth.guest();
  return client;
}

/** Joins and resolves with the instance, or rejects with the server's reason. */
async function joinAs(client, instanceId) {
  const conn = await client.connect();
  const instance = await conn.join(instanceId);
  return { conn, instance };
}

async function main() {
  console.log(`ADR-026 end-to-end verification against ${BASE}\n`);

  // --- T3 / mode discovery -------------------------------------------------
  console.log("mode discovery");
  const anon = new OrbiSyncClient({ baseUrl: BASE });
  const methods = await anon.auth.methods();
  check("discovery lists the enabled methods", methods.includes("guest") && methods.includes("local"), methods.join(","));
  check("discovery omits the method that is off", !methods.includes("external"), methods.join(","));

  // --- T12: guest -> ticket -> WS join -> entity command -------------------
  console.log("\nT12 guest reaches the world through the real ticket and socket");
  const guest = await guestClient();
  const guestSubject = subjectOf(guest._getAccessToken());
  check("a guest receives a session", typeof guestSubject === "string" && guestSubject.length > 0);

  const { conn: conn1, instance: inst1 } = await joinAs(guest, PERMITTED_INSTANCE);
  check("a guest joins a permitted world over a real socket", true);

  // Entity ids must be UUIDv7; the SDK exposes the same generator the app uses.
  const noteId = uuidv7();
  const spawnErrors = [];
  inst1.on("error", (e) => spawnErrors.push(e));
  inst1.sendEntityCommand({
    entityId: noteId,
    operation: "spawn",
    expectedRevision: 0,
    args: {
      kind: "object",
      visibility: "global",
      transform: { position_x: 1, position_y: 0, position_z: 1 },
    },
  });
  await new Promise((r) => setTimeout(r, 900));
  check("a guest's entity command is accepted", spawnErrors.length === 0, JSON.stringify(spawnErrors.slice(0, 1)));

  // --- T9: the participation boundary over the real join -------------------
  console.log("\nT9 the boundary decides on the world, not the instance id");
  const boundaryGuest = await guestClient();
  let offLimitsError = null;
  try {
    await joinAs(boundaryGuest, OFF_LIMITS_INSTANCE);
  } catch (e) {
    offLimitsError = e instanceof Error ? e.message : String(e);
  }
  check("a world outside the boundary is refused", offLimitsError !== null, offLimitsError ?? "join unexpectedly succeeded");

  const newInstanceGuest = await guestClient();
  let newInstanceOk = true;
  let newInstanceError = null;
  try {
    await joinAs(newInstanceGuest, PERMITTED_INSTANCE_2);
  } catch (e) {
    newInstanceOk = false;
    newInstanceError = e instanceof Error ? e.message : String(e);
  }
  // An instance created after the configuration was written is still inside
  // the permitted world; a boundary keyed on instance ids would reject it.
  check("a new instance in a permitted world is accepted", newInstanceOk, newInstanceError ?? "");

  // --- T13: reconnecting keeps the same subject ----------------------------
  console.log("\nT13 closing the socket does not end the session");
  await conn1.close?.();
  await new Promise((r) => setTimeout(r, 300));
  const reconnected = await guest.connect();
  const reInstance = await reconnected.join(PERMITTED_INSTANCE);
  check("the same session reconnects", true);
  check(
    "the subject is unchanged after reconnecting",
    subjectOf(guest._getAccessToken()) === guestSubject,
  );

  // --- T14: a fresh guest is a different subject ---------------------------
  console.log("\nT14 a fresh guest is a different subject");
  const otherGuest = await guestClient();
  const otherSubject = subjectOf(otherGuest._getAccessToken());
  check("a fresh guest receives a different identity", otherSubject !== guestSubject);

  const { instance: otherInstance } = await joinAs(otherGuest, PERMITTED_INSTANCE);
  const foreignErrors = [];
  otherInstance.on("error", (e) => foreignErrors.push(e));
  // The other guest's role holds entity.update.own but not entity.update.any,
  // so updating an entity it does not own must be refused by the authorizer.
  otherInstance.sendEntityCommand({
    entityId: noteId,
    operation: "update",
    expectedRevision: 1,
    args: { components: { "app.note": Buffer.from("hijacked").toString("base64") } },
  });
  await new Promise((r) => setTimeout(r, 900));
  check(
    "another guest cannot update an entity it does not own",
    foreignErrors.length > 0,
    foreignErrors.length === 0 ? "the command was accepted" : JSON.stringify(foreignErrors[0]).slice(0, 120),
  );

  // --- T10: the admin API stays closed to a guest --------------------------
  console.log("\nT10 a guest cannot reach the administration API");
  const adminAttempts = [
    ["POST", "/v1/users", JSON.stringify({ login_id: "sneaky", display_name: "Sneaky" })],
    ["GET", "/v1/audit-events", null],
    // A well-formed body, so the request reaches the authorization check rather
    // than being turned away for its shape.
    ["POST", "/v1/worlds", JSON.stringify({ name: "Guest Land", default_spawn: { position: { x: 0, y: 0, z: 0 }, rotation: { x: 0, y: 0, z: 0, w: 1 } }, capacity: 4 })],
  ];
  for (const [method, path, body] of adminAttempts) {
    const res = await fetch(`${BASE}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${guest._getAccessToken()}`,
        ...(body ? { "Content-Type": "application/json" } : {}),
      },
      ...(body ? { body } : {}),
    });
    check(`${method} ${path} is refused for a guest`, res.status === 403 || res.status === 404, `status ${res.status}`);
  }

  // --- forged fields -------------------------------------------------------
  console.log("\nT8 a request cannot claim its own roles");
  const forged = await fetch(`${BASE}/v1/auth/guest`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ role: "Administrator", user_id: "00000000-0000-7000-8000-000000000001" }),
  });
  check("a body claiming a role is refused", forged.status === 400, `status ${forged.status}`);

  // --- name-only -----------------------------------------------------------
  console.log("\nT6/T7 name-only participation");
  const a = new OrbiSyncClient({ baseUrl: BASE });
  const b = new OrbiSyncClient({ baseUrl: BASE });
  await a.auth.nameOnly({ displayName: "Ada" });
  await b.auth.nameOnly({ displayName: "Ada" });
  check("two visitors with the same name are different subjects", subjectOf(a._getAccessToken()) !== subjectOf(b._getAccessToken()));

  const blank = await fetch(`${BASE}/v1/auth/name`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ display_name: "   " }),
  });
  check("a blank display name is refused", blank.status === 400, `status ${blank.status}`);

  // --- external is off -----------------------------------------------------
  console.log("\nT2 a method that is off answers as such");
  const ext = new OrbiSyncClient({ baseUrl: BASE });
  let disabled = false;
  try {
    await ext.auth.external({ token: "whatever" });
  } catch (e) {
    disabled = e instanceof AuthMethodDisabledError;
  }
  check("external is reported as not enabled", disabled);

  // --- /v1/auth/me for a guest --------------------------------------------
  console.log("\nguest profile");
  const me = await fetch(`${BASE}/v1/auth/me`, {
    headers: { Authorization: `Bearer ${guest._getAccessToken()}` },
  });
  const meBody = await me.json().catch(() => ({}));
  check("a guest can read its own profile", me.status === 200, `status ${me.status}`);
  check(
    "the guest login id uses the reserved prefix",
    typeof meBody.login_id === "string" && meBody.login_id.startsWith("guest:"),
    JSON.stringify(meBody).slice(0, 120),
  );

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) {
    console.log("\nfailures:");
    for (const f of failures) console.log(`  - ${f}`);
    process.exit(1);
  }
}

main().catch((e) => {
  console.error("verification aborted:", e);
  process.exit(2);
});
