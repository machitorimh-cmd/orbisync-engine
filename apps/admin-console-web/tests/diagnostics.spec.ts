import { expect, test, type Page, type Route } from "@playwright/test";
import { diagnosticFixture } from "./diagnostics-fixture.js";

const headers = { "Access-Control-Allow-Origin": "http://127.0.0.1:4174", "Access-Control-Allow-Headers": "authorization,content-type,idempotency-key", "Access-Control-Allow-Methods": "GET,POST,OPTIONS" };
async function reply(route: Route, body: unknown, status = 200): Promise<void> {
  await route.fulfill({ status, headers, contentType: "application/json", body: JSON.stringify(body) });
}
async function setup(page: Page, diagnostic: (route: Route) => Promise<void>): Promise<void> {
  await page.route("http://127.0.0.1:8080/**", async route => {
    const path = new URL(route.request().url()).pathname;
    if (route.request().method() === "OPTIONS") { await route.fulfill({ status: 204, headers }); return; }
    if (path.startsWith("/health/")) { await reply(route, { status: path.endsWith("live") ? "live" : "ready", failures: [] }); return; }
    if (path === "/version") { await reply(route, { service: "orbisync", version: "test", protocol_major: 1 }); return; }
    if (path === "/v1/auth/login") { await reply(route, { access_token: "access", refresh_token: "refresh", token_type: "Bearer", expires_in: 900 }); return; }
    expect(route.request().headers().authorization).toBe("Bearer access");
    if (path === "/v1/auth/me") { await reply(route, { id: "operator", login_id: "admin", display_name: "Operator", enabled: true, revision: 1 }); return; }
    if (path === "/v1/auth/logout") { await route.fulfill({ status: 204, headers }); return; }
    if (path === "/v1/admin/diagnostics") { await diagnostic(route); return; }
    await reply(route, { items: [], next_cursor: null });
  });
  await page.goto("/");
  await page.locator("#loginId").fill("admin"); await page.locator("#password").fill("password");
  await page.getByRole("button", { name: "管理者としてログイン" }).click();
}

test("diagnostics separates normal grace from orphan, persistence lag, and denied access", async ({ page }, testInfo) => {
  let mode: "normal" | "anomaly" | "denied" = "normal";
  await setup(page, async route => {
    if (mode === "denied") { await reply(route, { error: { code: "FORBIDDEN", message: "denied" } }, 403); return; }
    const fixture = diagnosticFixture(); fixture.presence.observed_at = new Date().toISOString();
    if (mode === "anomaly") { fixture.presence.memberships_without_binding = 1; fixture.persistence.pending = 2; fixture.persistence.oldest_pending_seconds = 90; fixture.resume_rejections_total.revision_gap = 7; }
    await reply(route, fixture);
  });
  const panel = page.locator("#diagnostics");
  await expect(panel.locator(".diagnostic-healthy")).toBeVisible();
  await expect(panel).toContainText("猶予待ち（正常）");
  await expect(panel.locator(".diagnostic-warnings")).toHaveCount(0);
  mode = "anomaly"; await panel.getByRole("button", { name: "診断を更新" }).click();
  await expect(panel.locator(".diagnostic-warnings")).toContainText("所有記録のない Membership");
  await expect(panel.locator(".diagnostic-warnings")).toContainText("永続化待ち");
  await panel.screenshot({ path: testInfo.outputPath("diagnostics.png") });
  await panel.getByLabel("永続化待ちの注意秒数").fill("120"); await panel.getByLabel("永続化待ちの注意秒数").dispatchEvent("change");
  await expect(panel.locator(".diagnostic-warnings")).not.toContainText("永続化待ち");
  mode = "denied"; await panel.getByRole("button", { name: "診断を更新" }).click();
  await expect(panel).toContainText("診断を読む権限がありません");
  await expect(panel.locator(".diagnostic-grid")).toHaveCount(0);
});

test("logout invalidates a delayed diagnostics response", async ({ page }) => {
  let release: (() => void) | undefined;
  let delay = false;
  await setup(page, async route => {
    if (delay) await new Promise<void>(resolve => { release = resolve; });
    const fixture = diagnosticFixture(); fixture.presence.observed_at = new Date().toISOString();
    await reply(route, fixture);
  });
  await expect(page.locator(".diagnostic-grid")).toBeVisible();
  delay = true;
  await page.locator("#diagnostics").getByRole("button", { name: "診断を更新" }).click();
  await expect.poll(() => Boolean(release)).toBe(true);
  await page.locator("#logoutBtn").click();
  await expect(page.locator("#protectedArea")).toBeHidden();
  release!();
  await expect(page.locator(".diagnostic-grid")).toHaveCount(0);
  await expect(page.locator("#diagnostics")).not.toContainText("取得時刻:");
});
