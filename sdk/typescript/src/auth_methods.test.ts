/**
 * Tests for the ADR-026 authentication methods.
 *
 * These drive the client against a stub HTTP server rather than a real one, so
 * they assert the contract the SDK depends on: every method stores the same
 * token pair, which is what lets the connection path stay method-agnostic.
 */

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import { AuthMethodDisabledError, OrbiSyncClient } from "./client.js";

type Handler = (
  method: string,
  url: string,
  body: string,
) => { status: number; json: unknown };

/** Starts a stub server and returns its base URL plus a close function. */
async function withServer(
  handler: Handler,
  run: (baseUrl: string) => Promise<void>,
): Promise<void> {
  const server: Server = createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => {
      body += String(chunk);
    });
    req.on("end", () => {
      const result = handler(req.method ?? "GET", req.url ?? "/", body);
      res.writeHead(result.status, { "content-type": "application/json" });
      res.end(JSON.stringify(result.json));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("no address");
  try {
    await run(`http://127.0.0.1:${address.port}`);
  } finally {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
}

const TOKENS = {
  access_token: "access-1",
  refresh_token: "refresh-1",
  token_type: "Bearer",
  expires_in: 900,
};

describe("ADR-026 authentication methods", () => {
  it("guest, name-only and external all store the same token pair", async () => {
    const seen: Array<{ url: string; body: string }> = [];
    await withServer(
      (_method, url, body) => {
        seen.push({ url, body });
        return { status: 200, json: TOKENS };
      },
      async (baseUrl) => {
        for (const authenticate of [
          (c: OrbiSyncClient) => c.auth.guest(),
          (c: OrbiSyncClient) => c.auth.nameOnly({ displayName: "Ada" }),
          (c: OrbiSyncClient) => c.auth.external({ token: "signed.jwt.value" }),
        ]) {
          const client = new OrbiSyncClient({ baseUrl });
          await authenticate(client);
          // Every method leaves the client ready to fetch a ticket and
          // connect; nothing downstream needs to know which one ran.
          assert.equal(client._getAccessToken(), "access-1");
        }
      },
    );
    assert.deepEqual(
      seen.map((entry) => entry.url),
      ["/v1/auth/guest", "/v1/auth/name", "/v1/auth/external"],
    );
    // The guest request carries no fields at all: the server decides the
    // identifier, the display name and the roles.
    assert.equal(seen[0]?.body, "{}");
    assert.equal(seen[1]?.body, JSON.stringify({ display_name: "Ada" }));
    assert.equal(seen[2]?.body, JSON.stringify({ token: "signed.jwt.value" }));
  });

  it("a disabled method raises a distinct error", async () => {
    await withServer(
      () => ({
        status: 403,
        json: { error: { code: "AUTH_METHOD_DISABLED", message: "not enabled" } },
      }),
      async (baseUrl) => {
        const client = new OrbiSyncClient({ baseUrl });
        // Distinguishable from a credential failure, so a caller can fall back
        // to another method instead of prompting for a password.
        await assert.rejects(() => client.auth.guest(), AuthMethodDisabledError);
        await assert.rejects(
          () => client.auth.nameOnly({ displayName: "Ada" }),
          AuthMethodDisabledError,
        );
        await assert.rejects(
          () => client.auth.external({ token: "t" }),
          AuthMethodDisabledError,
        );
      },
    );
  });

  it("a rejected display name is not reported as a disabled method", async () => {
    await withServer(
      () => ({
        status: 400,
        json: { error: { code: "INVALID_REQUEST", message: "display_name must not be blank" } },
      }),
      async (baseUrl) => {
        const client = new OrbiSyncClient({ baseUrl });
        await assert.rejects(
          () => client.auth.nameOnly({ displayName: "   " }),
          (error: unknown) => {
            assert.ok(error instanceof Error);
            assert.ok(!(error instanceof AuthMethodDisabledError));
            return true;
          },
        );
      },
    );
  });

  it("discovery returns the accepted methods and ignores unknown names", async () => {
    await withServer(
      () => ({
        status: 200,
        // A server newer than this SDK may name a method it does not know;
        // that must not break discovery for the methods it does know.
        json: { methods: ["local", "guest", "quantum_handshake"] },
      }),
      async (baseUrl) => {
        const client = new OrbiSyncClient({ baseUrl });
        assert.deepEqual(await client.auth.methods(), ["local", "guest"]);
      },
    );
  });

  it("login still works unchanged", async () => {
    let seenBody = "";
    await withServer(
      (_method, _url, body) => {
        seenBody = body;
        return { status: 200, json: TOKENS };
      },
      async (baseUrl) => {
        const client = new OrbiSyncClient({ baseUrl });
        await client.auth.login({ loginId: "ada", password: "secret" });
        assert.equal(client._getAccessToken(), "access-1");
      },
    );
    assert.equal(seenBody, JSON.stringify({ login_id: "ada", password: "secret" }));
  });
});
