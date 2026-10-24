import { readFile } from "node:fs/promises";
import { expect, test, type Download, type Page, type Route } from "@playwright/test";
import { diagnosticFixture } from "./diagnostics-fixture.js";

const OPERATOR = "018f0000-0000-7000-8000-000000000001";
const TARGET = "018f0000-0000-7000-8000-000000000002";
const CURRENT = "018f0000-0000-7000-8000-000000000101";
const OTHER = "018f0000-0000-7000-8000-000000000102";
const THIRD = "018f0000-0000-7000-8000-000000000103";
const FOREIGN = "018f0000-0000-7000-8000-000000000201";
const FOREIGN_SECOND = "018f0000-0000-7000-8000-000000000202";
const headers = {
  "Access-Control-Allow-Origin": "http://127.0.0.1:4174",
  "Access-Control-Allow-Headers": "authorization,content-type,idempotency-key,if-match,accept",
  "Access-Control-Allow-Methods": "GET,POST,PUT,PATCH,DELETE,OPTIONS",
};
async function json(route: Route, body: unknown, status = 200): Promise<void> {
  await route.fulfill({ status, headers, contentType: "application/json", body: JSON.stringify(body) });
}
async function empty(route: Route): Promise<void> { await route.fulfill({ status: 204, headers }); }
const pageOf = (items: unknown[], cursor: string | null = null) => ({ items, next_cursor: cursor });
type FeatureHandler = (route: Route, url: URL, method: string) => Promise<boolean>;

async function setup(page: Page, feature: FeatureHandler): Promise<string[]> {
  const failures: string[] = [];
  page.on("pageerror", error => failures.push(error.message));
  await page.route("http://127.0.0.1:8080/**", async route => {
    const request = route.request(), url = new URL(request.url()), method = request.method();
    if (method === "OPTIONS") return empty(route);
    if (url.pathname.startsWith("/health/")) return json(route, { status: url.pathname.endsWith("live") ? "live" : "ready", failures: [] });
    if (url.pathname === "/version") return json(route, { service: "orbisync", version: "features-test", protocol_major: 1 });
    if (url.pathname === "/v1/auth/login") return json(route, { access_token: "feature-access", refresh_token: "feature-refresh", token_type: "Bearer", expires_in: 3600 });
    expect(request.headers().authorization).toBe("Bearer feature-access");
    if (url.pathname === "/v1/auth/me") return json(route, { id: OPERATOR, login_id: "admin", display_name: "Feature Operator", enabled: true, revision: 1 });
    if (url.pathname === "/v1/auth/logout") return empty(route);
    if (await feature(route, url, method)) return;
    if (url.pathname === "/v1/admin/diagnostics") {
      const fixture = diagnosticFixture(); fixture.presence.observed_at = new Date().toISOString();
      return json(route, fixture);
    }
    if (["/v1/users", "/v1/roles", "/v1/worlds", "/v1/instances", "/v1/audit-events"].includes(url.pathname)) return json(route, pageOf([]));
    failures.push(`Unexpected ${method} ${url.pathname}`);
    return json(route, { error: { code: "RESOURCE_NOT_FOUND", message: "Unknown feature test path", request_id: "req_feature" } }, 404);
  });
  await page.goto("/");
  await page.locator("#loginId").fill("admin"); await page.locator("#password").fill("test-password");
  await page.getByRole("button", { name: "管理者としてログイン" }).click();
  await expect(page.locator("#protectedArea")).toBeVisible();
  await expect(page.locator("#diagnostics .diagnostic-grid")).toBeVisible();
  return failures;
}

function session(id: string, current = false, label: string | null = null) {
  return { session_id: id, current, device_label: label, status: "active", created_at: "2026-10-24T00:00:00Z", expires_at: "2026-10-25T00:00:00Z", revoked_at: null };
}

test("self sessions paginate, save advisory labels, revoke one/others and end current login", async ({ page }) => {
  let sessions = [session(CURRENT, true), session(OTHER), session(THIRD)];
  const mutations: string[] = [];
  const failures = await setup(page, async (route, url, method) => {
    if (!url.pathname.startsWith("/v1/auth/sessions")) return false;
    const path = url.pathname;
    if (method === "GET") {
      const next = url.searchParams.get("cursor");
      await json(route, next ? pageOf(sessions.slice(2)) : pageOf(sessions.slice(0, 2), sessions.length > 2 ? "self-next" : null));
    } else if (method === "PATCH") {
      expect(path).toBe(`/v1/auth/sessions/${OTHER}`);
      expect(route.request().postDataJSON()).toEqual({ device_label: "Office laptop" });
      sessions.find(item => item.session_id === OTHER)!.device_label = "Office laptop";
      mutations.push("label"); await empty(route);
    } else if (method === "POST") {
      expect(path).toBe("/v1/auth/sessions/revoke-others");
      sessions = sessions.filter(item => item.current); mutations.push("others");
      await json(route, { revoked_count: 1 });
    } else if (method === "DELETE") {
      const id = path.split("/").at(-1)!;
      sessions = sessions.filter(item => item.session_id !== id); mutations.push(id); await empty(route);
    } else throw new Error(`Unexpected session method ${method}`);
    return true;
  });
  await page.locator("#selfSessionsBtn").click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(2);
  await expect(page.locator("#sessionsTbody")).toContainText(`${CURRENT}（現在）`);
  await page.locator("#nextSessionsBtn").click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(3);
  await expect(page.locator("#nextSessionsBtn")).toBeHidden();
  const other = page.locator("#sessionsTbody tr").filter({ hasText: OTHER });
  await other.getByLabel("端末の表示名").fill("Office laptop");
  await other.getByRole("button", { name: "名前を保存" }).click();
  await expect(other.getByLabel("端末の表示名")).toHaveValue("Office laptop");
  await other.getByRole("button", { name: "失効", exact: true }).click();
  await expect(page.locator("#sessionsTbody")).not.toContainText(OTHER);
  await page.locator("#revokeOtherSessionsBtn").click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(1);
  await expect(page.locator("#sessionsTbody")).toContainText(CURRENT);
  await page.locator("#sessionsTbody tr").getByRole("button", { name: "失効", exact: true }).click();
  await expect(page.locator("#protectedArea")).toBeHidden();
  await expect(page.locator("#sessionUser")).toHaveText("未ログイン");
  expect(mutations).toEqual(["label", OTHER, "others", CURRENT]); expect(failures).toEqual([]);
});

test("admin session scope uses target-user paths and keeps self controls separate", async ({ page }) => {
  let sessions = [session(FOREIGN), session(FOREIGN_SECOND)]; const calls: string[] = [];
  const failures = await setup(page, async (route, url, method) => {
    if (url.pathname === "/v1/auth/sessions") { calls.push(`${method} self`); await json(route, pageOf([session(CURRENT, true)])); return true; }
    if (!url.pathname.startsWith(`/v1/users/${TARGET}/sessions`)) return false;
    calls.push(`${method} ${url.pathname}`);
    if (method === "GET") await json(route, pageOf(sessions));
    else if (method === "DELETE") {
      sessions = url.pathname.endsWith(FOREIGN) ? sessions.filter(item => item.session_id !== FOREIGN) : [];
      await empty(route);
    } else throw new Error(`Unexpected administrative session method ${method}`);
    return true;
  });
  await page.locator("#sessionTargetUser").fill(TARGET);
  await page.locator("#adminSessionsBtn").click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(2);
  await expect(page.locator("#sessionsStatus")).toContainText(TARGET);
  await expect(page.locator("#sessionsTbody input")).toHaveCount(0);
  await page.locator("#sessionsTbody tr").filter({ hasText: FOREIGN }).getByRole("button", { name: "失効", exact: true }).click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(1);
  await page.locator("#revokeAllUserSessionsBtn").click();
  await expect(page.locator("#sessionsTbody tr")).toHaveCount(0);
  await page.locator("#selfSessionsBtn").click();
  await expect(page.locator("#sessionsTbody")).toContainText(`${CURRENT}（現在）`);
  expect(calls).toContain(`DELETE /v1/users/${TARGET}/sessions/${FOREIGN}`);
  expect(calls).toContain(`DELETE /v1/users/${TARGET}/sessions`);
  expect(calls.at(-1)).toBe("GET self"); expect(failures).toEqual([]);
});

function audit(id: string, action: string) {
  return { audit_id: id, timestamp: "2026-10-24T12:00:00.123456Z", actor_type: "user", actor_id: OPERATOR, action, resource_type: "entity", resource_id: "=SUM(1,2)", result: "success", request_id: "req_feature", details: { safe_count: 1 } };
}
async function downloadedText(download: Download): Promise<string> {
  const path = await download.path(); expect(path).not.toBeNull(); return readFile(path!, "utf8");
}

test("compound audit search resets cursors and exports only the last fetched page as JSON/CSV", async ({ page }) => {
  const requests: URL[] = [], exports: URL[] = [];
  const csv = "audit_id,resource_id\r\n\"last\",\"'=SUM(1,2)\"\r\n";
  const failures = await setup(page, async (route, url) => {
    if (url.pathname === "/v1/audit-events") {
      requests.push(url);
      const filtered = url.searchParams.has("action"), next = url.searchParams.get("cursor");
      if (!filtered) await json(route, pageOf([audit("initial", "initial.action")], "initial-next"));
      else await json(route, pageOf([audit(next ? "last" : "first", "entity.updated")], next ? null : "filtered-next"));
      return true;
    }
    if (url.pathname !== "/v1/audit-events/export") return false;
    exports.push(url);
    if (url.searchParams.get("format") === "csv") await route.fulfill({ status: 200, headers, contentType: "text/csv; charset=utf-8", body: csv });
    else await json(route, pageOf([audit("last", "entity.updated")]));
    return true;
  });
  await expect(page.locator("#nextAuditBtn")).toBeVisible();
  await page.locator("#auditActionFilter").fill("entity.updated");
  await expect(page.locator("#nextAuditBtn")).toBeHidden();
  await expect(page.locator("#auditCount")).toHaveText("0 events");
  await expect(page.locator("#exportAuditJsonBtn")).toBeDisabled();
  await page.locator("#auditActorFilter").fill(OPERATOR);
  await page.locator("#auditResourceTypeFilter").fill("entity");
  await page.locator("#auditResourceIdFilter").fill("=SUM(1,2)");
  await page.locator("#auditRequestFilter").fill("req_feature");
  await page.locator("#auditFromFilter").fill("2026-10-24T01:00");
  await page.locator("#auditToFilter").fill("2026-10-24T23:00");
  const from = await page.locator("#auditFromFilter").evaluate(input => new Date((input as HTMLInputElement).value).toISOString());
  const to = await page.locator("#auditToFilter").evaluate(input => new Date((input as HTMLInputElement).value).toISOString());
  await page.locator("#applyAuditFilterBtn").click();
  await expect(page.locator("#auditCount")).toHaveText("1 events");
  const expected = { action: "entity.updated", actor_id: OPERATOR, resource_type: "entity", resource_id: "=SUM(1,2)", request_id: "req_feature", from, to };
  expect(Object.fromEntries(requests.at(-1)!.searchParams)).toEqual({ ...expected, limit: "100" });
  await page.locator("#nextAuditBtn").click();
  await expect(page.locator("#auditCount")).toHaveText("2 events");
  await expect(page.locator("#nextAuditBtn")).toBeHidden();
  const jsonDownload = page.waitForEvent("download"); await page.locator("#exportAuditJsonBtn").click();
  const jsonFile = await jsonDownload; expect(jsonFile.suggestedFilename()).toBe("audit-events.json");
  expect(JSON.parse(await downloadedText(jsonFile)).items.map((item: { audit_id: string }) => item.audit_id)).toEqual(["last"]);
  const csvDownload = page.waitForEvent("download"); await page.locator("#exportAuditCsvBtn").click();
  const csvFile = await csvDownload; expect(csvFile.suggestedFilename()).toBe("audit-events.csv");
  expect(await downloadedText(csvFile)).toBe(csv);
  expect(exports).toHaveLength(2);
  for (const url of exports) expect(Object.fromEntries(url.searchParams)).toEqual({ ...expected, limit: "100", cursor: "filtered-next", format: url.searchParams.get("format") });
  expect(failures).toEqual([]);
});

test("editing audit conditions invalidates a delayed old page before a fresh search", async ({ page }) => {
  let release: (() => void) | undefined; const requests: URL[] = [];
  const failures = await setup(page, async (route, url) => {
    if (url.pathname !== "/v1/audit-events") return false;
    requests.push(url);
    if (url.searchParams.get("cursor") === "initial-next") {
      await new Promise<void>(resolve => { release = resolve; });
      await json(route, pageOf([audit("stale", "stale.action")]));
    } else await json(route, pageOf([audit("current", url.searchParams.get("action") ?? "initial.action")], url.searchParams.has("action") ? null : "initial-next"));
    return true;
  });
  await page.locator("#nextAuditBtn").click();
  await expect.poll(() => Boolean(release)).toBe(true);
  await page.locator("#auditActionFilter").fill("fresh.action");
  const response = page.waitForResponse(response => new URL(response.url()).searchParams.get("cursor") === "initial-next");
  release!(); await response;
  await expect(page.locator("#auditCount")).toHaveText("0 events");
  await expect(page.locator("#auditTbody")).not.toContainText("stale.action");
  await expect(page.locator("#exportAuditCsvBtn")).toBeDisabled();
  await page.locator("#applyAuditFilterBtn").click();
  await expect(page.locator("#auditTbody")).toContainText("fresh.action");
  expect(requests.at(-1)!.searchParams.get("cursor")).toBeNull();
  expect(failures).toEqual([]);
});

test("external key reload sends one POST and displays generation/key IDs without file paths or material", async ({ page }) => {
  let release: (() => void) | undefined, calls = 0;
  const failures = await setup(page, async (route, url, method) => {
    if (url.pathname !== "/v1/auth/external/keys/reload") return false;
    expect(method).toBe("POST"); expect(route.request().postData()).toBeNull(); calls++;
    await new Promise<void>(resolve => { release = resolve; });
    await json(route, { generation: 3, key_count: 2, key_ids: ["old-key", "new-key"], ignored_file_path: "/private/operator.jwks", ignored_material: "private-jwk-material" });
    return true;
  });
  await expect(page.locator("#externalKeys input")).toHaveCount(0);
  await page.locator("#reloadExternalKeysBtn").evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
  await expect.poll(() => Boolean(release)).toBe(true); expect(calls).toBe(1);
  await expect(page.locator("#reloadExternalKeysBtn")).toBeDisabled();
  release!();
  await expect(page.locator("#externalKeysStatus")).toHaveText("鍵世代 3、2件: old-key, new-key");
  await expect(page.locator("#reloadExternalKeysBtn")).toBeEnabled();
  await expect(page.locator("body")).not.toContainText("/private/operator.jwks");
  await expect(page.locator("body")).not.toContainText("private-jwk-material");
  expect(failures).toEqual([]);
});
