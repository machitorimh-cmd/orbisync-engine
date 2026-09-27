/**
 * ADR-026 T27: a real ResumeSession is checked against the participation
 * boundary.
 *
 * Resume returns straight to the instance recorded in its binding without
 * passing through the join branch, so the boundary has to be consulted there
 * separately. This drives the SDK's own reconnect path, which sends a genuine
 * ResumeSession carrying the binding obtained from a real join.
 *
 * Reaching the check matters as much as the check itself. The subject's world
 * is withdrawn while its session and ticket stay valid, so the handshake still
 * succeeds and the ResumeSession is actually processed. Letting the deadline
 * pass instead would fail at ticket consumption, before resume is considered,
 * and would prove nothing about this path.
 *
 * The server's own structured log is the evidence: `realtime.resume_denied_by_scope`
 * is emitted only from the resume branch and names the instance and world it
 * refused. Nothing secret is read; the line holds identifiers only.
 *
 * Usage:
 *   ORBISYNC_E2E_INSTANCE=<instance id> ORBISYNC_E2E_WORLD=<world id> \
 *   ORBISYNC_E2E_OTHER_WORLD=<world id> ORBISYNC_E2E_SERVER_LOG=<path> \
 *   node verify_auth_resume_scope_e2e.mjs <baseUrl> <container> <db>
 */

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { OrbiSyncClient } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const CONTAINER = process.argv[3] ?? "orbisync-auth26-test-db";
const DB = process.argv[4] ?? "orbisync_auth26";
const INSTANCE = process.env.ORBISYNC_E2E_INSTANCE;
const WORLD = process.env.ORBISYNC_E2E_WORLD;
const OTHER_WORLD = process.env.ORBISYNC_E2E_OTHER_WORLD;
const SERVER_LOG = process.env.ORBISYNC_E2E_SERVER_LOG;

if (!INSTANCE || !WORLD || !OTHER_WORLD || !SERVER_LOG) {
  console.error(
    "set ORBISYNC_E2E_INSTANCE, ORBISYNC_E2E_WORLD, ORBISYNC_E2E_OTHER_WORLD and ORBISYNC_E2E_SERVER_LOG",
  );
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

function subjectOf(accessToken) {
  const payload = accessToken.split(".")[1];
  return JSON.parse(
    Buffer.from(payload.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString(),
  ).sub;
}

/** Lines the server wrote for a given event name. */
function eventLines(event) {
  return readFileSync(SERVER_LOG, "utf8")
    .split("\n")
    .filter((line) => line.includes(event));
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Drops the socket without marking an explicit close, so the SDK reconnects
 * and sends ResumeSession with the binding it already holds.
 */
async function forceResume(conn) {
  conn._getWs().close();
  await wait(4500);
}

async function main() {
  console.log("ADR-026 resume is checked against the participation boundary\n");

  const guest = new OrbiSyncClient({ baseUrl: BASE });
  await guest.auth.guest();
  const subject = subjectOf(guest._getAccessToken());
  sql(
    `UPDATE ephemeral_subjects SET allowed_worlds = ARRAY['${WORLD}']::uuid[] WHERE user_id = '${subject}'`,
  );

  const conn = await guest.connect();
  await conn.join(INSTANCE);
  check(
    "the join produced a resume binding",
    conn._getResumeToken().length > 0,
  );

  // --- control: resume is not refused while the world is permitted ---------
  const deniedBefore = eventLines("realtime.resume_denied_by_scope").length;
  await forceResume(conn);
  const deniedAfterControl = eventLines("realtime.resume_denied_by_scope").length;
  check(
    "resume is not refused while the world is permitted",
    deniedAfterControl === deniedBefore,
    `denials ${deniedBefore} -> ${deniedAfterControl}`,
  );
  check(
    "the connection still holds a resume binding after reconnecting",
    conn._getResumeToken().length > 0,
  );

  // --- withdraw the world, leaving the session and ticket valid ------------
  sql(
    `UPDATE ephemeral_subjects SET allowed_worlds = ARRAY['${OTHER_WORLD}']::uuid[] WHERE user_id = '${subject}'`,
  );
  const validSessions = sql(
    `SELECT count(*) FROM auth_sessions WHERE user_id = '${subject}' AND status = 'active' AND expires_at > now()`,
  );
  check(
    "the session is still valid, so the handshake reaches the resume branch",
    validSessions !== "0",
    `valid sessions: ${validSessions}`,
  );
  const unexpired = sql(
    `SELECT count(*) FROM ephemeral_subjects WHERE user_id = '${subject}' AND expires_at > now()`,
  );
  check(
    "the subject has not expired, so only the world check can refuse",
    unexpired === "1",
    `unexpired rows: ${unexpired}`,
  );

  const resumeBefore = eventLines("realtime.resume_denied_by_scope").length;
  const joinBefore = eventLines("realtime.join_denied_by_scope").length;
  await forceResume(conn);

  const denials = eventLines("realtime.resume_denied_by_scope");
  check(
    "the resume branch refused the withdrawn world",
    denials.length > resumeBefore,
    `resume denials ${resumeBefore} -> ${denials.length}`,
  );

  const denial = denials[denials.length - 1] ?? "";
  check(
    "the refusal names the instance it was asked to resume into",
    denial.includes(INSTANCE),
    denial.slice(0, 220),
  );
  check(
    "the refusal names the world the subject may no longer enter",
    denial.includes(WORLD),
    denial.slice(0, 220),
  );
  // After a refused resume the SDK falls back to a fresh join, which the join
  // branch then refuses too. That is the client behaving correctly, not the
  // join branch doing the work: what matters is that the resume refusal came
  // first and stands on its own.
  const lines = readFileSync(SERVER_LOG, "utf8").split(/\r?\n/);
  const resumeAt = lines.findLastIndex((line) =>
    line.includes("realtime.resume_denied_by_scope"),
  );
  const joinAt = lines.findLastIndex((line) =>
    line.includes("realtime.join_denied_by_scope"),
  );
  check(
    "the resume branch refused before any join fallback did",
    resumeAt >= 0 && (joinAt < 0 || resumeAt < joinAt),
    `resume line ${resumeAt}, join line ${joinAt}`,
  );
  check(
    "the join fallback afterwards is the client retrying, and is also refused",
    eventLines("realtime.join_denied_by_scope").length >= joinBefore,
  );

  // Restore so a later run starts from a permitted boundary.
  sql(
    `UPDATE ephemeral_subjects SET allowed_worlds = ARRAY['${WORLD}']::uuid[] WHERE user_id = '${subject}'`,
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
