import { it } from "node:test";
import assert from "node:assert/strict";
import { OrbiSyncAdminApi, SessionChangedError } from "../src/api.js";

const tokens = (name: string) => Response.json({ access_token: `${name}-access`, refresh_token: `${name}-refresh` });
const user = { id: "admin", login_id: "admin", display_name: "Admin", enabled: true, revision: 1 };
const authorization = (init?: RequestInit) => new Headers(init?.headers).get("Authorization");
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

it("a delayed login cannot send its credentials to a newly selected server", async () => {
  const original = globalThis.fetch, login = deferred<Response>();
  const requests: string[] = [];
  globalThis.fetch = (async input => { requests.push(String(input)); return login.promise; }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server-a.example");
    const pending = assert.rejects(api.login("admin-a", "password-a"), SessionChangedError);
    api.setBaseUrl("https://server-b.example");
    login.resolve(tokens("a"));
    await pending;
    assert.deepEqual(requests, ["https://server-a.example/v1/auth/login"]);
    assert.equal(api.isAuthenticated(), false);
  } finally { globalThis.fetch = original; }
});

it("reverse login responses cannot replace or clear the latest session", async () => {
  const original = globalThis.fetch, old = deferred<Response>();
  const seen: Array<string | null> = [];
  let logins = 0;
  globalThis.fetch = (async (input, init) => {
    if (String(input).endsWith("/login")) return ++logins === 1 ? old.promise : tokens("new");
    seen.push(authorization(init));
    return Response.json(user);
  }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server.example");
    const pending = assert.rejects(api.login("old", "password"), SessionChangedError);
    await api.login("new", "password");
    old.resolve(tokens("old"));
    await pending;
    await api.currentUser();
    assert.deepEqual(seen, ["Bearer new-access", "Bearer new-access"]);
  } finally { globalThis.fetch = original; }
});

for (const status of [200, 401]) {
  it(`a refresh response (${status}) cannot revive logout or retry against another server`, async () => {
    const original = globalThis.fetch, refresh = deferred<Response>(), started = deferred<void>();
    const requests: string[] = [];
    globalThis.fetch = (async input => {
      const url = String(input); requests.push(url);
      if (url.endsWith("/login")) return tokens("old");
      if (url.endsWith("/me")) return Response.json(user);
      if (url.endsWith("/logout")) return new Response(null, { status: 204 });
      if (url.endsWith("/refresh")) { started.resolve(); return refresh.promise; }
      return new Response(null, { status: 401 });
    }) as typeof fetch;
    try {
      const api = new OrbiSyncAdminApi("https://server-a.example");
      await api.login("admin", "password");
      const pending = assert.rejects(api.listUsers(), SessionChangedError);
      await started.promise;
      await api.logout();
      api.setBaseUrl("https://server-b.example");
      refresh.resolve(status === 200 ? tokens("late") : new Response(null, { status }));
      await pending;
      assert.equal(api.isAuthenticated(), false);
      assert.ok(requests.every(url => url.startsWith("https://server-a.example/")));
    } finally { globalThis.fetch = original; }
  });
}

it("an obsolete failed refresh cannot erase a new login", async () => {
  const original = globalThis.fetch, refresh = deferred<Response>(), started = deferred<void>();
  let logins = 0;
  globalThis.fetch = (async (input, init) => {
    const url = String(input);
    if (url.endsWith("/login")) return tokens(++logins === 1 ? "old" : "new");
    if (url.endsWith("/me")) {
      assert.equal(authorization(init), logins === 1 ? "Bearer old-access" : "Bearer new-access");
      return Response.json(user);
    }
    if (url.endsWith("/refresh")) { started.resolve(); return refresh.promise; }
    return new Response(null, { status: 401 });
  }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server.example");
    await api.login("old", "password");
    const pending = assert.rejects(api.listUsers(), SessionChangedError);
    await started.promise;
    await api.login("new", "password");
    refresh.resolve(new Response(null, { status: 401 }));
    await pending;
    assert.equal(api.isAuthenticated(), true);
    await api.currentUser();
  } finally { globalThis.fetch = original; }
});

it("concurrent expired requests share one refresh and retry with its token", async () => {
  const original = globalThis.fetch;
  let refreshes = 0;
  globalThis.fetch = (async (input, init) => {
    const url = String(input);
    if (url.endsWith("/login")) return tokens("old");
    if (url.endsWith("/me")) return Response.json(user);
    if (url.endsWith("/refresh")) { refreshes++; return tokens("renewed"); }
    return authorization(init) === "Bearer renewed-access"
      ? Response.json({ items: [] }) : new Response(null, { status: 401 });
  }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server.example");
    await api.login("admin", "password");
    await Promise.all([api.listUsers(), api.listRoles()]);
    assert.equal(refreshes, 1);
  } finally { globalThis.fetch = original; }
});

it("logout can renew an expired access token to revoke the server session", async () => {
  const original = globalThis.fetch;
  const logoutTokens: Array<string | null> = [];
  globalThis.fetch = (async (input, init) => {
    const url = String(input);
    if (url.endsWith("/login")) return tokens("old");
    if (url.endsWith("/me")) return Response.json(user);
    if (url.endsWith("/refresh")) return tokens("renewed");
    logoutTokens.push(authorization(init));
    return new Response(null, { status: authorization(init) === "Bearer renewed-access" ? 204 : 401 });
  }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server.example");
    await api.login("admin", "password");
    await api.logout();
    assert.deepEqual(logoutTokens, ["Bearer old-access", "Bearer renewed-access"]);
    assert.equal(api.isAuthenticated(), false);
  } finally { globalThis.fetch = original; }
});

it("logout and delayed response bodies cannot clear or expose a newer session", async () => {
  const original = globalThis.fetch, logout = deferred<Response>(), body = deferred<string>(), started = deferred<void>();
  let deferBody = false;
  globalThis.fetch = (async input => {
    const url = String(input);
    if (url.endsWith("/login")) return tokens("current");
    if (url.endsWith("/logout")) return logout.promise;
    const response = Response.json(user);
    if (deferBody) { response.text = () => { started.resolve(); return body.promise; }; }
    return response;
  }) as typeof fetch;
  try {
    const api = new OrbiSyncAdminApi("https://server.example");
    await api.login("first", "password");
    deferBody = true;
    const oldUser = assert.rejects(api.currentUser(), SessionChangedError);
    await started.promise;
    const leaving = assert.rejects(api.logout(), SessionChangedError);
    deferBody = false;
    await api.login("second", "password");
    logout.resolve(new Response(null, { status: 204 }));
    body.resolve(JSON.stringify(user));
    await Promise.all([oldUser, leaving]);
    assert.equal(api.isAuthenticated(), true);
  } finally { globalThis.fetch = original; }
});
