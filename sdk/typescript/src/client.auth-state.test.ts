import { it } from "node:test";
import assert from "node:assert/strict";
import { OrbiSyncClient, RefreshFailedError } from "./client.js";

it("refreshes at 80 percent of the actual token lifetime", async () => {
  const originalFetch = globalThis.fetch;
  const originalNow = Date.now;
  let now = 1_000_000;
  let requests = 0;
  Date.now = () => now;
  globalThis.fetch = (async () => {
    requests++;
    return Response.json({ access_token: "renewed", refresh_token: "next", expires_in: 60 });
  }) as typeof fetch;
  try {
    const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
    await client.auth.login({ loginId: "test", password: "test" });
    requests = 0;
    await client.ensureValidAccessToken();
    assert.equal(requests, 0, "a newly issued short-lived token must not refresh immediately");
    now += 47_999;
    await client.ensureValidAccessToken();
    assert.equal(requests, 0);
    now++;
    await client.ensureValidAccessToken();
    assert.equal(requests, 1);
    now += 47_999;
    await client.ensureValidAccessToken();
    assert.equal(requests, 1, "the refreshed token starts its own lifetime");
    now++;
    await client.ensureValidAccessToken();
    assert.equal(requests, 2);
  } finally { Date.now = originalNow; globalThis.fetch = originalFetch; }
});

for (const status of [200, 401]) {
  it(`an old refresh response (${status}) cannot overwrite a newer login`, async () => {
    const original = globalThis.fetch;
    const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
    client._setTokens("old", "old-refresh", 60);
    const errors: unknown[] = [];
    client.on("error", error => errors.push(error));
    let finish!: (response: Response) => void;
    globalThis.fetch = (async input => String(input).endsWith("/refresh")
      ? new Promise<Response>(resolve => { finish = resolve; })
      : Response.json({ access_token: "new-login", refresh_token: "new-refresh", expires_in: 60 })) as typeof fetch;
    try {
      const refreshing = client.refreshAccessToken();
      // Observe the rejection immediately to avoid an unhandled rejection.
      const result = Promise.allSettled([refreshing]);
      await client.auth.login({ loginId: "new", password: "test" });
      finish(status === 200 ? Response.json({ access_token: "old-renewed", expires_in: 60 }) : new Response("", { status }));
      await result;
      assert.equal(client._getAccessToken(), "new-login");
      assert.equal(client.getAuthState(), "Authenticated");
      assert.deepEqual(errors, []);
    } finally { globalThis.fetch = original; }
  });
}

it("the latest login wins when login responses arrive in reverse order", async () => {
  const original = globalThis.fetch;
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  const responses: Array<(response: Response) => void> = [];
  globalThis.fetch = (() => new Promise<Response>(resolve => responses.push(resolve))) as typeof fetch;
  try {
    const oldLogin = Promise.allSettled([client.auth.login({ loginId: "old", password: "test" })]);
    const newLogin = client.auth.login({ loginId: "new", password: "test" });
    responses[1](Response.json({ access_token: "new", expires_in: 60 }));
    await newLogin;
    responses[0](Response.json({ access_token: "old", expires_in: 60 }));
    await oldLogin;
    assert.equal(client._getAccessToken(), "new");
    assert.equal(client.getAuthState(), "Authenticated");
  } finally { globalThis.fetch = original; }
});

it("a connection ticket from an earlier login cannot create a connection after a new login", async () => {
  const original = globalThis.fetch;
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  client._setTokens("old", "old-refresh", 60);
  let finishTicket!: (value: string) => void;
  let ticketStarted!: () => void;
  const started = new Promise<void>(resolve => { ticketStarted = resolve; });
  client.fetchRealtimeTicket = () => { ticketStarted(); return new Promise(resolve => { finishTicket = resolve; }); };
  globalThis.fetch = (async () => Response.json({ access_token: "new", refresh_token: "new-refresh", expires_in: 60 })) as typeof fetch;
  try {
    const connecting = client.connect();
    const rejected = assert.rejects(connecting, /authentication operation superseded/);
    await started;
    await client.auth.login({ loginId: "new", password: "test" });
    finishTicket("old-ticket");
    await rejected;
    assert.equal(client.getAuthState(), "Authenticated");
  } finally { globalThis.fetch = original; }
});

it("refresh rejection clears authentication, emits one typed error and is shared by concurrent callers", async () => {
  const original = globalThis.fetch;
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  client._setTokens("expired", "refresh", -1);
  const errors: unknown[] = [];
  const states: unknown[] = [];
  client.on("error", value => errors.push(value));
  client.on("authStateChanged", value => states.push(value));
  let requests = 0;
  globalThis.fetch = (async () => { requests++; return new Response("", { status: 401 }); }) as typeof fetch;
  try {
    const results = await Promise.allSettled([client.refreshAccessToken(), client.refreshAccessToken()]);
    assert.equal(requests, 1);
    assert.ok(results.every(result => result.status === "rejected" && result.reason instanceof RefreshFailedError));
    assert.equal(errors.length, 1);
    assert.deepEqual(states, ["Unauthenticated"]);
    assert.equal(client.getAuthState(), "Unauthenticated");
    await assert.rejects(client.connect(), /not authenticated/);
  } finally { globalThis.fetch = original; }
});

it("login exposes authentication transitions and removed listeners stop receiving notifications", async () => {
  const original = globalThis.fetch;
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  const states: unknown[] = [];
  const handler = (state: unknown) => states.push(state);
  client.on("authStateChanged", handler);
  globalThis.fetch = (async () => Response.json({ access_token: "access", refresh_token: "refresh", expires_in: 60 })) as typeof fetch;
  try {
    await client.auth.login({ loginId: "test", password: "test" });
    assert.deepEqual(states, ["Authenticating", "Authenticated"]);
    client.off("authStateChanged", handler);
    globalThis.fetch = (async () => new Response("", { status: 401 })) as typeof fetch;
    await assert.rejects(client.auth.login({ loginId: "test", password: "bad" }));
    assert.equal(client.getAuthState(), "Unauthenticated");
    assert.equal(states.length, 2);
  } finally { globalThis.fetch = original; }
});

it("ticket refresh failures preserve the typed error used to stop resume retries", async () => {
  const original = globalThis.fetch;
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  client._setTokens("revoked", "refresh", 60);
  const calls: string[] = [];
  globalThis.fetch = (async input => { calls.push(String(input)); return new Response("", { status: 401 }); }) as typeof fetch;
  try {
    await assert.rejects(client.fetchRealtimeTicket(), RefreshFailedError);
    assert.deepEqual(calls.map(url => new URL(url).pathname), ["/v1/realtime/tickets", "/v1/auth/refresh"]);
    assert.equal(client.getAuthState(), "Unauthenticated");
  } finally { globalThis.fetch = original; }
});
