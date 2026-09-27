/**
 * ADR-026 T14 with the improvement-02 lock rule in force.
 *
 * The point of this run is the separation ADR-026 insists on: RBAC decides
 * whether a subject may touch another participant's entity at all, and the
 * pre-commit rule decides whether a *locked* note may be edited. So both
 * guests here hold `entity.update.any` — the permission that makes
 * collaborative editing possible — and the rule, not the authorizer, is what
 * keeps A's locked note safe from B.
 *
 * A signed HTTPS pre-commit extension is registered for this run, so the
 * decisions come from the real hook over a real TLS connection.
 *
 * Sequence: A creates and locks a note; B is refused update, unlock and
 * delete; A unlocks; B edits successfully.
 *
 * Usage:
 *   ORBISYNC_E2E_INSTANCE=<instance id> \
 *   node verify_auth_guest_lock_e2e.mjs <baseUrl> <container> <db>
 */

import { execFileSync } from "node:child_process";
import { OrbiSyncClient, uuidv7 } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const CONTAINER = process.argv[3] ?? "orbisync-auth26-test-db";
const DB = process.argv[4] ?? "orbisync_auth26";
const INSTANCE = process.env.ORBISYNC_E2E_INSTANCE;

if (!INSTANCE) {
  console.error("set ORBISYNC_E2E_INSTANCE");
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

function sql(statement) {
  return execFileSync(
    "docker",
    ["exec", CONTAINER, "psql", "-U", "orbisync", "-d", DB, "-tAc", statement],
    { encoding: "utf8" },
  ).trim();
}

const settle = () => new Promise((r) => setTimeout(r, 1200));

/** The component key the lock rule reads. */
const NOTE_COMPONENT = "com.orbisync.whiteboard.note";

/**
 * The note arguments, shaped exactly as the whiteboard sends them.
 *
 * Core replaces the component wholesale, so every command carries the complete
 * note rather than a patch.
 */
function noteArgs({ text, locked }) {
  return {
    component_key: NOTE_COMPONENT,
    kind: "object",
    visibility: "global",
    text,
    color: "#ffd166",
    locked: locked === true,
    position_x: 1,
    position_y: 1,
  };
}

/**
 * Reads the revision Core currently holds.
 *
 * The rule refuses a command whose expected revision does not match, so each
 * step reads the live value instead of assuming how many commits landed.
 */
function currentRevision(id) {
  return Number(sql(`SELECT revision FROM persistent_entities WHERE id = '${id}'`) || 0);
}

async function joinAsGuest() {
  const client = new OrbiSyncClient({ baseUrl: BASE });
  await client.auth.guest();
  const conn = await client.connect();
  const instance = await conn.join(INSTANCE);
  const errors = [];
  instance.on("error", (e) => errors.push(e));
  return { client, conn, instance, errors };
}

async function main() {
  console.log("ADR-026 T14 with the lock rule in force\n");

  const registration = sql(
    "SELECT status || ' ' || endpoint FROM extension_registrations WHERE name = 'whiteboard-lock'",
  );
  check(
    "a signed HTTPS pre-commit extension is registered",
    registration.startsWith("active https://"),
    registration || "no registration",
  );

  const permissions = sql(
    "SELECT string_agg(permission_name, ',' ORDER BY permission_name) FROM role_permissions rp " +
      "JOIN roles r ON r.id = rp.role_id WHERE r.name = 'E2E Visitor'",
  );
  check(
    "the guest role holds entity.update.any, so RBAC is not what refuses",
    permissions.includes("entity.update.any"),
    permissions,
  );

  const a = await joinAsGuest();
  const b = await joinAsGuest();
  check("two guests joined", true);

  // --- A creates a note ----------------------------------------------------
  const noteId = uuidv7();
  a.instance.sendEntityCommand({
    entityId: noteId,
    operation: "spawn",
    expectedRevision: 0,
    args: noteArgs({ text: "A's note", locked: false }),
  });
  await settle();
  check("A creates a note", a.errors.length === 0, JSON.stringify(a.errors[0] ?? {}).slice(0, 160));
  check("the note is persisted", currentRevision(noteId) > 0, `revision ${currentRevision(noteId)}`);

  // --- A locks it ----------------------------------------------------------
  a.errors.length = 0;
  a.instance.sendEntityCommand({
    entityId: noteId,
    operation: "update",
    expectedRevision: currentRevision(noteId),
    args: noteArgs({ text: "A's note", locked: true }),
  });
  await settle();
  check("A locks the note", a.errors.length === 0, JSON.stringify(a.errors[0] ?? {}).slice(0, 160));
  const lockedRevision = currentRevision(noteId);

  // --- B is refused while the note is locked -------------------------------
  // B holds entity.update.any, so the authorizer permits all three of these.
  // Only the lock rule stands between B and A's note.
  for (const [label, build] of [
    [
      "update",
      () => ({
        entityId: noteId,
        operation: "update",
        expectedRevision: currentRevision(noteId),
        args: noteArgs({ text: "B was here", locked: true }),
      }),
    ],
    [
      "unlock",
      () => ({
        entityId: noteId,
        operation: "update",
        expectedRevision: currentRevision(noteId),
        args: noteArgs({ text: "A's note", locked: false }),
      }),
    ],
    [
      "delete",
      () => ({
        entityId: noteId,
        operation: "delete",
        expectedRevision: currentRevision(noteId),
        args: {},
      }),
    ],
  ]) {
    b.errors.length = 0;
    b.instance.sendEntityCommand(build());
    await settle();
    const denied = b.errors.length > 0;
    check(
      `B's ${label} on A's locked note is refused`,
      denied,
      denied
        ? `${b.errors[0]?.code ?? "?"}: ${(b.errors[0]?.message ?? "").slice(0, 90)}`
        : "the command was accepted",
    );
    check(
      `B's ${label} left the note unchanged at revision ${lockedRevision}`,
      currentRevision(noteId) === lockedRevision,
      `revision is now ${currentRevision(noteId)}`,
    );
  }

  // --- A unlocks, then B may edit -----------------------------------------
  a.errors.length = 0;
  a.instance.sendEntityCommand({
    entityId: noteId,
    operation: "update",
    expectedRevision: currentRevision(noteId),
    args: noteArgs({ text: "A's note", locked: false }),
  });
  await settle();
  check("A unlocks the note", a.errors.length === 0, JSON.stringify(a.errors[0] ?? {}).slice(0, 160));

  b.errors.length = 0;
  const beforeEdit = currentRevision(noteId);
  b.instance.sendEntityCommand({
    entityId: noteId,
    operation: "update",
    expectedRevision: beforeEdit,
    args: noteArgs({ text: "B edited after unlock", locked: false }),
  });
  await settle();
  check(
    "B edits the unlocked note successfully",
    b.errors.length === 0,
    b.errors.length === 0
      ? ""
      : `${b.errors[0]?.code ?? "?"}: ${(b.errors[0]?.message ?? "").slice(0, 90)}`,
  );
  check(
    "B's accepted edit advanced the note",
    currentRevision(noteId) > beforeEdit,
    `revision ${beforeEdit} -> ${currentRevision(noteId)}`,
  );

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
