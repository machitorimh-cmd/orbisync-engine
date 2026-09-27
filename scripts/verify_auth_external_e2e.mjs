/**
 * ADR-026 external authentication, end to end against a running server.
 *
 * Tokens are signed here by a local issuer and verified by the production
 * adapter — there is no test-only verifier in this path. The server reads its
 * keys from a static JWKS file on disk, which is what makes the whole thing
 * runnable offline.
 *
 * Proves the accepting side as well as the rejecting side: a valid token
 * yields a session that reaches a realtime ticket, a WebSocket join and an
 * entity command, the same (issuer, subject) pair resolves to the same user
 * every time, and a second issuer using that subject string does not.
 *
 * Usage:
 *   ORBISYNC_E2E_INSTANCE=<instance id> \
 *   node verify_auth_external_e2e.mjs <baseUrl> <privateKeyPem> <container> <db>
 */

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { createPrivateKey, generateKeyPairSync, sign as cryptoSign } from "node:crypto";
import { OrbiSyncClient, uuidv7 } from "@orbisync/client";

const BASE = process.argv[2] ?? "http://127.0.0.1:18099";
const KEY_PATH = process.argv[3];
const CONTAINER = process.argv[4] ?? "orbisync-auth26-test-db";
const DB = process.argv[5] ?? "orbisync_auth26";
const INSTANCE = process.env.ORBISYNC_E2E_INSTANCE;

const ISSUER = "https://idp.e2e.test";
const AUDIENCE = "orbisync-api";

if (!KEY_PATH || !INSTANCE) {
  console.error("usage: <baseUrl> <privateKeyPem> [container] [db]; ORBISYNC_E2E_INSTANCE required");
  process.exit(2);
}

const signingKey = createPrivateKey(readFileSync(KEY_PATH));

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

const b64 = (buf) => Buffer.from(buf).toString("base64url");

/** Mints a signed token. `key` defaults to the issuer the server trusts. */
function mint(claims, { kid = "e2e-key-1", alg = "EdDSA", key = signingKey } = {}) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64(JSON.stringify({ alg, typ: "JWT", kid }));
  const payload = b64(
    JSON.stringify({ iss: ISSUER, aud: AUDIENCE, iat: now, nbf: now - 60, exp: now + 3600, ...claims }),
  );
  const signingInput = `${header}.${payload}`;
  const signature = cryptoSign(null, Buffer.from(signingInput), key);
  return `${signingInput}.${b64(signature)}`;
}

async function externalLogin(token) {
  const client = new OrbiSyncClient({ baseUrl: BASE });
  await client.auth.external({ token });
  return client;
}

function subjectOf(accessToken) {
  const payload = accessToken.split(".")[1];
  return JSON.parse(
    Buffer.from(payload.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString(),
  ).sub;
}

const settle = () => new Promise((r) => setTimeout(r, 900));

async function main() {
  console.log(`ADR-026 external authentication against ${BASE}\n`);

  // --- the accepting side --------------------------------------------------
  console.log("a valid token reaches the world");
  const upstreamSubject = `upstream-${Date.now()}`;
  const client = await externalLogin(mint({ sub: upstreamSubject }));
  const userId = subjectOf(client._getAccessToken());
  check("a signed token yields a session", typeof userId === "string" && userId.length > 0);

  const conn = await client.connect();
  const instance = await conn.join(INSTANCE);
  check("an external subject joins over a real ticket and socket", true);

  const errors = [];
  instance.on("error", (e) => errors.push(e));
  const entityId = uuidv7();
  instance.sendEntityCommand({
    entityId,
    operation: "spawn",
    expectedRevision: 0,
    args: { kind: "object", visibility: "global", transform: { position_x: 3, position_y: 0, position_z: 3 } },
  });
  await settle();
  check("an external subject's entity command is accepted", errors.length === 0, JSON.stringify(errors[0] ?? {}).slice(0, 140));

  const owned = sql(
    `SELECT count(*) FROM persistent_entities WHERE owner_id = '${userId}'`,
  );
  check("the entity is owned by the external subject", owned === "1", `owned rows: ${owned}`);

  // --- identity mapping ----------------------------------------------------
  console.log("\nthe issuer and subject pair is the identity");
  const again = await externalLogin(mint({ sub: upstreamSubject }));
  check(
    "the same pair resolves to the same user",
    subjectOf(again._getAccessToken()) === userId,
  );

  const mapped = sql(
    `SELECT count(*) FROM external_identities WHERE issuer = '${ISSUER}' AND subject = '${upstreamSubject}'`,
  );
  check("exactly one mapping row exists for the pair", mapped === "1", `rows: ${mapped}`);

  const kindRow = sql(`SELECT kind FROM users WHERE id = '${userId}'`);
  check("the user is stored as an external subject", kindRow === "external", kindRow);

  const credentials = sql(`SELECT count(*) FROM user_credentials WHERE user_id = '${userId}'`);
  check("an external subject holds no password credential", credentials === "0", credentials);

  const ephemeral = sql(`SELECT count(*) FROM ephemeral_subjects WHERE user_id = '${userId}'`);
  check("an external subject is not treated as temporary", ephemeral === "0", ephemeral);

  const other = await externalLogin(mint({ sub: `other-${Date.now()}` }));
  check("a different subject is a different user", subjectOf(other._getAccessToken()) !== userId);

  // --- self-asserted claims are ignored ------------------------------------
  console.log("\nself-asserted claims do not decide anything");
  const boastful = await externalLogin(
    mint({ sub: upstreamSubject, email: "e2eadmin@example.org", role: "Administrator", groups: ["admins"] }),
  );
  check(
    "an issuer asserting an address and a role gains nothing",
    subjectOf(boastful._getAccessToken()) === userId,
  );
  const adminAttempt = await fetch(`${BASE}/v1/users`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${boastful._getAccessToken()}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ login_id: "claimed", display_name: "Claimed" }),
  });
  check("the asserted role grants no administration access", adminAttempt.status === 403, `status ${adminAttempt.status}`);

  // A local account must not be reachable by assertion. The token below names
  // that account in every self-asserted field an issuer could try.
  const victim = process.env.ORBISYNC_E2E_LOCAL_LOGIN ?? "localvictim";
  const victimId = sql(`SELECT id FROM users WHERE login_id = '${victim}'`);
  check("a local account exists to defend", victimId.length > 0, victimId);
  const impersonator = await externalLogin(
    mint({ sub: victim, email: `${victim}@example.org`, preferred_username: victim, login_id: victim }),
  );
  const impersonatorId = subjectOf(impersonator._getAccessToken());
  check(
    "an issuer naming a local account does not become that account",
    impersonatorId !== victimId,
    `local=${victimId} external=${impersonatorId}`,
  );
  const victimKind = sql(`SELECT kind FROM users WHERE id = '${victimId}'`);
  check("the local account is untouched", victimKind === "account", victimKind);
  const victimCreds = sql(
    `SELECT count(*) FROM user_credentials WHERE user_id = '${victimId}'`,
  );
  check("the local account keeps its credential", victimCreds === "1", victimCreds);
  const victimMapping = sql(
    `SELECT count(*) FROM external_identities WHERE user_id = '${victimId}'`,
  );
  check("no external identity is attached to the local account", victimMapping === "0", victimMapping);

  // --- the rejecting side --------------------------------------------------
  console.log("\ntokens that must not be accepted");
  const foreign = generateKeyPairSync("ed25519").privateKey;
  const cases = [
    ["a token signed by another key", mint({ sub: "intruder" }, { key: foreign })],
    ["an unknown key id", mint({ sub: "intruder" }, { kid: "rotated-away" })],
    ["a wrong issuer", mint({ sub: "intruder", iss: "https://evil.test" })],
    ["a wrong audience", mint({ sub: "intruder", aud: "someone-else" })],
    ["an expired token", mint({ sub: "intruder", exp: Math.floor(Date.now() / 1000) - 600 })],
    ["an empty subject", mint({ sub: "   " })],
    ["a malformed token", "not.a.token"],
  ];
  for (const [name, token] of cases) {
    const res = await fetch(`${BASE}/v1/auth/external`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    check(`${name} is refused`, res.status === 401, `status ${res.status}`);
  }

  // A tampered payload keeps the original signature, which is the forgery a
  // verifier has to catch.
  const original = mint({ sub: upstreamSubject });
  const [h, , s] = original.split(".");
  const tampered = `${h}.${b64(JSON.stringify({ iss: ISSUER, aud: AUDIENCE, sub: "someone-else", exp: Math.floor(Date.now() / 1000) + 3600, nbf: 0, iat: 0 }))}.${s}`;
  const tamperedRes = await fetch(`${BASE}/v1/auth/external`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token: tampered }),
  });
  check("a tampered payload is refused", tamperedRes.status === 401, `status ${tamperedRes.status}`);

  // --- local login still works --------------------------------------------
  console.log("\nlocal login is unaffected");
  const methods = await new OrbiSyncClient({ baseUrl: BASE }).auth.methods();
  check("every configured method is reported", methods.length === 4, methods.join(","));

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
