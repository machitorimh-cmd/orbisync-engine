import { test, expect } from "@playwright/test";
import { Adventure, STAR_POSITIONS } from "../src/game";

test("移動・衝突・収集・点灯・リセットのゲームループ", () => {
    const g = new Adventure();
    expect(g.ignite()).toBe(false);
    g.reset();
    for (let i = 0; i < 600; i++) g.step(1 / 60, 0, 1, true);
    expect(Math.hypot(g.x, g.z)).toBeLessThan(11.25);
    g.x = 0;
    g.z = 3;
    for (let i = 0; i < 100; i++) g.step(1 / 60, 0, -1, false);
    expect(g.z).toBeGreaterThan(0.95);
    expect(g.ignite()).toBe(false);
    STAR_POSITIONS.forEach(([x, z]) => {
        g.x = x;
        g.z = z;
        expect(g.step(1 / 60, 0, 0, false)).toHaveLength(1);
        expect(g.step(1 / 60, 0, 0, false)).toHaveLength(0);
    });
    expect(g.ignite()).toBe(false);
    g.x = 0;
    g.z = 2;
    expect(g.ignite()).toBe(true);
    expect(g.ignite()).toBe(false);
    const elapsed = g.elapsed;
    g.step(1 / 60, 0, 0, false);
    expect(g.elapsed).toBe(elapsed);
    g.reset();
    expect(g.collected.size).toBe(0);
    expect(g.complete).toBe(false);
});

test("フレームレートと斜め移動で速度が変わらない", () => {
    const a = new Adventure(),
        b = new Adventure(),
        c = new Adventure();
    [a, b, c].forEach((g) => g.reset());
    for (let i = 0; i < 30; i++) a.step(1 / 30, 1, 0, false);
    for (let i = 0; i < 120; i++) b.step(1 / 120, 1, 0, false);
    for (let i = 0; i < 60; i++) c.step(1 / 60, 1, -1, false);
    expect(a.x).toBeCloseTo(b.x, 6);
    expect(Math.hypot(c.x, c.z - 7)).toBeCloseTo(a.x, 6);
    a.jump();
    a.step(1 / 60, 0, 0, false);
    expect(a.y).toBeGreaterThan(0);
    for (let i = 0; i < 100; i++) a.step(1 / 60, 0, 0, false);
    expect(a.y).toBe(0);
});

test("日本語の導入から実際に移動してかけらを拾う", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install({ time: new Date("2026-09-07T00:00:00Z") });
    await page.clock.pauseAt(new Date("2026-09-07T00:00:01Z"));
    // Exercise low-FPS input deterministically without depending on the CI GPU.
    await page.addInitScript(() => {
        window.requestAnimationFrame = (callback) =>
            window.setTimeout(() => callback(performance.now()), 50);
        window.cancelAnimationFrame = (id) => clearTimeout(id);
    });
    await page.goto("/");
    await expect(page.locator("#world canvas")).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("lang", "ja");
    await page.screenshot({ path: "test-results/title-desktop.png" });
    await page.getByRole("button", { name: "冒険をはじめる" }).click();
    await expect(page.locator("#hud")).toBeVisible();
    await page.keyboard.down("a");
    await page.clock.runFor(2300);
    await page.keyboard.up("a");
    await page.keyboard.down("w");
    await page.clock.runFor(1050);
    await page.keyboard.up("w");
    await expect(page.locator("#count")).toContainText("1");
    await page.getByRole("button", { name: "遊び方", exact: true }).click();
    const before = await page.locator("#timer").textContent();
    await page.clock.runFor(1100);
    expect(await page.locator("#timer").textContent()).toBe(before);
    await page.getByRole("button", { name: "わかった" }).click();
    await page.screenshot({ path: "test-results/game-desktop.png" });
    await page.getByRole("button", { name: "やり直す" }).click();
    await expect(page.locator("#count")).toHaveText("0 / 5");
    expect(errors).toEqual([]);
});

test("スマートフォンの導入とタッチ操作を表示", async ({ browser }) => {
    const context = await browser.newContext({
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
    });
    const page = await context.newPage();
    await page.goto("/");
    await page.screenshot({ path: "test-results/title-mobile.png" });
    await page.getByRole("button", { name: "冒険をはじめる" }).click();
    await expect(page.locator("#touch")).toBeVisible();
    expect(
        await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBe(390);
    await page.screenshot({ path: "test-results/game-mobile.png" });
    await context.close();
});

test("接続失敗を日本語で表示して再試行できる", async ({ page }) => {
    await page.route("http://localhost:8080/**", (route) =>
        route.fulfill({
            status: 401,
            headers: { "Access-Control-Allow-Origin": "*" },
            body: "{}",
        }),
    );
    await page.goto("/");
    await page.getByRole("button", { name: "友だちと同じ島へ" }).click();
    await page.locator("#login").fill("demo");
    await page.locator("#password").fill("not-a-real-password");
    await page.locator("#room").fill("00000000-0000-7000-8000-000000000001");
    await page.locator("#connect").click();
    await expect(page.locator("#connection-error")).toContainText(
        "ログインIDまたはパスワード",
    );
    await expect(page.locator("#connect")).toBeEnabled();
});
