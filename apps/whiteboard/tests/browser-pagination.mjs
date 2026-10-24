import { it } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';
import { createServer } from 'vite';

// API interception keeps this UI regression independent of a running Core/DB.
for (const reference of [false, true]) it(`${reference ? 'reference console' : 'whiteboard'} UI reaches World/Instance 101 with bounded retryable pages`, async () => {
  const root = fileURLToPath(new URL(reference ? '../../reference-console-web/' : '../', import.meta.url));
  const server = await createServer({ root, server: { host: '127.0.0.1', port: 0 } });
  await server.listen();
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage(); const errors = [], calls = [];
    page.on('pageerror', error => errors.push(error.message));
    let worldNext = 0;
    await page.route('http://api.test/**', async route => {
      const request = route.request(), url = new URL(request.url());
      const headers = { 'access-control-allow-origin': '*', 'access-control-allow-headers': '*', 'access-control-allow-methods': '*' };
      const reply = (body, status = 200) => route.fulfill({ status, headers, contentType: 'application/json', body: JSON.stringify(body) });
      if (request.method() === 'OPTIONS') return reply({});
      if (url.pathname === '/v1/auth/login') return reply({ access_token: 'test-access', refresh_token: 'test-refresh', expires_in: 3600 });
      if (url.pathname === '/v1/auth/me') return reply({ id: 'user', login_id: 'admin', display_name: 'Test', enabled: true, revision: 1 });
      if (url.pathname === '/v1/auth/methods') return reply({ methods: ['local', 'guest', 'name_only'] });
      if (!['/v1/worlds', '/v1/instances'].includes(url.pathname)) return reply({}, 404);
      calls.push(url.pathname + url.search);
      assert.equal(url.searchParams.get('limit'), '100');
      const next = url.searchParams.get('cursor');
      if (url.pathname === '/v1/worlds') {
        if (next) { assert.equal(next, 'world+cursor/='); if (++worldNext === 1) return reply({}, 503); }
        return reply(next ? { items: [{ id: 'world-101', name: 'World 101', status: 'active', revision: 1 }] } : {
          items: Array.from({ length: 100 }, (_, i) => ({ id: `world-${i + 1}`, name: `World ${i + 1}`, status: 'active', revision: 1 })), next_cursor: 'world+cursor/=',
        });
      }
      if (next) assert.equal(next, 'instance+cursor/=');
      return reply(next ? { items: [{ id: 'instance-101', world_id: 'world-101', status: 'running', revision: 1 }] } : {
        items: Array.from({ length: 100 }, (_, i) => ({ id: `instance-${i + 1}`, world_id: 'world-1', status: 'running', revision: 1 })), next_cursor: 'instance+cursor/=',
      });
    });
    await page.goto(server.resolvedUrls.local[0]);
    const selector = reference ? '#worldSelect' : '#worlds';
    const moreWorlds = reference ? '#moreWorldsBtn' : '#moreWorlds';
    const moreInstances = reference ? '#moreInstancesBtn' : '#moreInstances';
    const instances = reference ? '#instanceSelect' : '#instances';
    await page.locator(reference ? '#serverUrl' : '#server').fill('http://api.test');
    await page.locator('#password').fill('test-password');
    await page.locator(reference ? '#loginBtn' : '#login').click();
    if (!reference) { await expect(page.locator('#connection')).toHaveText('認証済み'); await page.locator('#load').click(); }
    await expect(page.locator(`${selector} option[value="world-100"]`)).toHaveCount(1);
    assert.equal(calls.filter(path => path.startsWith('/v1/worlds')).length, 1);
    // Failed next page preserves the existing list and cursor for an explicit retry.
    await page.locator(moreWorlds).evaluate(button => { button.click(); button.click(); });
    await expect(page.locator(moreWorlds)).toBeEnabled();
    assert.equal(worldNext, 1);
    await expect(page.locator(`${selector} option[value="world-100"]`)).toHaveCount(1);
    await page.locator(moreWorlds).click();
    await expect(page.locator(`${selector} option[value="world-101"]`)).toHaveCount(1);
    await expect(page.locator(moreWorlds)).toBeDisabled();
    await page.locator(selector).selectOption('world-101');
    await expect(page.locator(moreInstances)).toBeEnabled();
    await page.locator(moreInstances).evaluate(button => { button.click(); button.click(); });
    await expect(page.locator(`${instances} option[value="instance-101"]`)).toHaveCount(1);
    assert.equal(calls.filter(path => path.startsWith('/v1/instances') && path.includes('cursor=')).length, 1);
    await expect(page.locator(moreInstances)).toBeDisabled();
    // Changing the API origin immediately discards old results and cursors.
    await page.locator(reference ? '#serverUrl' : '#server').fill('http://other-api.test');
    await expect(page.locator(`${instances} option[value="instance-101"]`)).toHaveCount(0);
    await expect(page.locator(moreInstances)).toBeDisabled();
    await expect(page.locator(moreWorlds)).toBeDisabled();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); await server.close(); }
});
