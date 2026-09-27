/**
 * ADR-026 T15/T25: what a temporary subject's session is worth over time.
 *
 * Covers the three ways a session stops being usable — the deadline passing,
 * an explicit logout, and a refresh token being presented twice — and checks
 * each against both the REST surface and realtime ticket issuance, since a
 * subject that can still mint a ticket can still reach the world.
 *
 * Also checks the expiry a subject is *issued* with. The deadline has to bind
 * the first token pair, not only later rotations: a first refresh token minted
 * at the global lifetime would outlive the subject it belongs to even once
 * rotation clamps everything after it.
 *
 * Usage:
 *   node verify_auth_lifecycle_e2e.mjs <baseUrl> <container> <db>
 */

import { execFileSync } from "node:child_process";
import { OrbiSyncClient } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const CONTAINER = process.argv[3] ?? "orbisync-auth26-test-db";
const DB = process.argv[4] ?? "orbisync_auth26";

/** The deployment's configured values, for the clamping comparison. */
const GUEST_TTL_SECONDS = Number(process.env.ORBISYNC_E2E_GUEST_TTL ?? 3600);
const GLOBAL_REFRESH_TTL_SECONDS = Number(process.env.ORBISYNC_E2E_REFRESH_TTL ?? 2_592_000);

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

/** Raw POST so the test sees the status rather than an SDK exception. */
async function post(path, body, token) {
  return fetch(`${BASE}${path}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

async function newGuest() {
  const res = await post("/v1/auth/guest", {});
  const body = await res.json();
  return {
    accessToken: body.access_token,
    refreshToken: body.refresh_token,
    expiresIn: body.expires_in,
    subject: subjectOf(body.access_token),
  };
}

async function main() {
  console.log("ADR-026 session lifetime for a temporary subject\n");

  // --- T25: the deadline binds the first token pair ------------------------
  console.log("the subject is issued inside its own deadline");
  const first = await newGuest();
  const sessionTtl = Number(
    sql(
      `SELECT round(EXTRACT(EPOCH FROM (expires_at - created_at))) FROM auth_sessions ` +
        `WHERE user_id = '${first.subject}'`,
    ),
  );
  const refreshTtl = Number(
    sql(
      `SELECT round(EXTRACT(EPOCH FROM (r.expires_at - r.issued_at))) FROM refresh_tokens r ` +
        `JOIN auth_sessions s ON s.id = r.session_id WHERE s.user_id = '${first.subject}'`,
    ),
  );
  const deadlineTtl = Number(
    sql(
      `SELECT round(EXTRACT(EPOCH FROM (expires_at - created_at))) FROM ephemeral_subjects ` +
        `WHERE user_id = '${first.subject}'`,
    ),
  );
  check(
    "the first session is clamped to the subject deadline",
    sessionTtl <= GUEST_TTL_SECONDS && sessionTtl < GLOBAL_REFRESH_TTL_SECONDS,
    `session ${sessionTtl}s, deadline ${GUEST_TTL_SECONDS}s, global ${GLOBAL_REFRESH_TTL_SECONDS}s`,
  );
  check(
    "the first refresh token is clamped to the subject deadline",
    refreshTtl <= GUEST_TTL_SECONDS && refreshTtl < GLOBAL_REFRESH_TTL_SECONDS,
    `refresh ${refreshTtl}s`,
  );
  check("the deadline itself matches the configured lifetime", deadlineTtl === GUEST_TTL_SECONDS, `${deadlineTtl}s`);
  check(
    "the access token lifetime does not exceed the deadline",
    first.expiresIn <= GUEST_TTL_SECONDS,
    `${first.expiresIn}s`,
  );

  // --- T15: refresh token reuse -------------------------------------------
  console.log("\na refresh token cannot be used twice");
  const reuse = await newGuest();
  const rotated = await post("/v1/auth/refresh", { refresh_token: reuse.refreshToken });
  check("the first refresh succeeds", rotated.status === 200, `status ${rotated.status}`);
  const replayed = await post("/v1/auth/refresh", { refresh_token: reuse.refreshToken });
  check("presenting the same token again is refused", replayed.status === 401, `status ${replayed.status}`);
  const revoked = sql(
    `SELECT count(*) FROM auth_sessions WHERE user_id = '${reuse.subject}' AND status = 'revoked'`,
  );
  check("reuse revokes the session it belonged to", revoked !== "0", `revoked sessions: ${revoked}`);
  const ticketAfterReuse = await post("/v1/realtime/tickets", undefined, reuse.accessToken);
  check(
    "no realtime ticket is issued after reuse is detected",
    ticketAfterReuse.status === 401,
    `status ${ticketAfterReuse.status}`,
  );

  // --- T15: logout ---------------------------------------------------------
  console.log("\nlogout ends the session everywhere");
  const out = await newGuest();
  const ticketBefore = await post("/v1/realtime/tickets", undefined, out.accessToken);
  check("a ticket is issued while the session is live", ticketBefore.status === 200, `status ${ticketBefore.status}`);
  const logout = await post("/v1/auth/logout", undefined, out.accessToken);
  check("logout is accepted", logout.status === 204 || logout.status === 200, `status ${logout.status}`);
  const ticketAfter = await post("/v1/realtime/tickets", undefined, out.accessToken);
  check("no ticket is issued after logout", ticketAfter.status === 401, `status ${ticketAfter.status}`);
  const meAfter = await fetch(`${BASE}/v1/auth/me`, {
    headers: { Authorization: `Bearer ${out.accessToken}` },
  });
  check("REST refuses the token after logout", meAfter.status === 401, `status ${meAfter.status}`);
  const refreshAfter = await post("/v1/auth/refresh", { refresh_token: out.refreshToken });
  check("refresh is refused after logout", refreshAfter.status === 401, `status ${refreshAfter.status}`);

  // --- T15: the deadline passing ------------------------------------------
  console.log("\nthe deadline ends the session without any cleanup running");
  const expired = await newGuest();
  const ticketWhileLive = await post("/v1/realtime/tickets", undefined, expired.accessToken);
  check("a ticket is issued before the deadline", ticketWhileLive.status === 200, `status ${ticketWhileLive.status}`);
  // Move the subject and its session into the past together. The revocation
  // pass is not invoked, so anything that still refuses is doing so from the
  // stored deadline rather than from a status flag.
  sql(
    `UPDATE ephemeral_subjects SET created_at = now() - interval '2 hours', ` +
      `expires_at = now() - interval '1 hour' WHERE user_id = '${expired.subject}'`,
  );
  sql(
    `UPDATE auth_sessions SET expires_at = now() - interval '1 hour' WHERE user_id = '${expired.subject}'`,
  );
  const stillActive = sql(
    `SELECT count(*) FROM auth_sessions WHERE user_id = '${expired.subject}' AND status = 'active'`,
  );
  check("the session row is still marked active", stillActive !== "0", `active rows: ${stillActive}`);
  const rolesStillGranted = sql(
    `SELECT count(*) FROM user_roles WHERE user_id = '${expired.subject}'`,
  );
  check(
    "the roles are still granted, so no cleanup has run",
    rolesStillGranted !== "0",
    `role rows: ${rolesStillGranted}`,
  );

  const expiredTicket = await post("/v1/realtime/tickets", undefined, expired.accessToken);
  check("no ticket is issued past the deadline", expiredTicket.status === 401, `status ${expiredTicket.status}`);
  const expiredMe = await fetch(`${BASE}/v1/auth/me`, {
    headers: { Authorization: `Bearer ${expired.accessToken}` },
  });
  check("REST refuses the token past the deadline", expiredMe.status === 401, `status ${expiredMe.status}`);
  const expiredRefresh = await post("/v1/auth/refresh", { refresh_token: expired.refreshToken });
  check("refresh is refused past the deadline", expiredRefresh.status === 401, `status ${expiredRefresh.status}`);

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
