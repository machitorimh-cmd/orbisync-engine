// Browser regression for the embedded admin, with an optional real private DB.
// This never attaches to an existing installation or modifies operator data.
const { chromium, expect } = require(process.env.PLAYWRIGHT_MODULE || '@playwright/test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { spawn } = require('node:child_process');
const assert = require('node:assert/strict');
const repository = path.resolve(__dirname, '..');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const assets = {
  '/': ['apps/admin-web/index.html', 'text/html'],
  '/admin.js': ['apps/admin-web/src/main.js', 'text/javascript'],
  '/admin-connection.js': ['apps/admin-web/src/local_connection.js', 'text/javascript'],
};

async function mocked(browser) {
  const context = await browser.newContext({locale: 'ja-JP'});
  const page = await context.newPage();
  await page.clock.install();
  let token = 'initial-secret-token', refreshes = 0, expire = false, revoke = false, forbidden = false, refreshOffline = false;
  let unauthorized = 0;
  let sequence = 0, created = 0;
  const worlds = [], instances = [], errors = [];
  const users = [
    {id: 'operator-id', login_id: 'operator', display_name: 'Operator', enabled: true},
    {id: 'member-id', login_id: 'member', display_name: 'Operator', enabled: true},
  ];
  let statusChanges = 0;
  page.on('pageerror', error => errors.push(error.message));
  await context.route('https://admin.test/**', async route => {
    const request = route.request(), p = new URL(request.url()).pathname, method = request.method();
    if (assets[p]) return route.fulfill({contentType: assets[p][1], body: fs.readFileSync(path.join(repository, assets[p][0]), 'utf8')});
    const json = (body, status = 200) => route.fulfill({status, headers: {'x-request-id': 'req_fixture'}, json: body});
    if (p === '/admin/status') return json({initialized: true, ready: true, engine: 'running'});
    if (p === '/api/v1/auth/login') return json({access_token: token, refresh_token: 'refresh-secret-token', expires_in: 900});
    if (p === '/api/v1/auth/refresh') {
      refreshes++;
      assert.equal(request.postDataJSON().refresh_token, 'refresh-secret-token');
      if (refreshOffline) return json({error: {code: 'SERVICE_UNAVAILABLE'}}, 503);
      if (revoke) return json({error: {code: 'AUTHENTICATION_REQUIRED'}}, 401);
      await wait(50); expire = false; token = 'renewed-secret-token';
      return json({access_token: token, refresh_token: 'refresh-secret-token', expires_in: 900});
    }
    if (expire || request.headers().authorization !== `Bearer ${token}`) {unauthorized++; return json({error: {code: 'AUTHENTICATION_REQUIRED', message: 'authentication required', request_id: 'req_expired'}}, 401);}
    if (p === '/api/v1/auth/me') return json(users[0]);
    if (p === '/api/v1/users') return json({items: users});
    if (p === '/api/v1/roles') return json({items: []});
    if (p === '/api/v1/users/operator-id/roles') return json({role_ids: []});
    const statusMatch = p.match(/^\/api\/v1\/users\/([^/]+)\/(disable|enable)$/);
    if (statusMatch) {
      assert.equal(statusMatch[1], 'member-id', 'the signed-in operator must not be disabled from its own management screen');
      statusChanges++;
      users[1].enabled = statusMatch[2] === 'enable';
      return json(users[1]);
    }
    if (p === '/api/v1/worlds' && method === 'GET') return json({items: worlds});
    if (p === '/api/v1/instances' && method === 'GET') return json({items: instances});
    if (p === '/api/v1/worlds' && method === 'POST') {
      if (forbidden) return json({error: {code: 'NOT_AUTHORIZED', message: 'permission required', request_id: 'req_forbidden'}}, 403);
      const world = {id: `world-${++sequence}`, name: request.postDataJSON().name, status: 'active'};
      created++; worlds.push(world); return json(world, 201);
    }
    if (p === '/api/v1/instances' && method === 'POST') {
      const instance = {id: `instance-${++sequence}`, world_id: request.postDataJSON().world_id, status: 'created'};
      instances.push(instance); return json(instance, 201);
    }
    const match = p.match(/^\/api\/v1\/instances\/([^/]+)\/(start|stop)$/);
    if (match) {
      const instance = instances.find(i => i.id === match[1]);
      assert.ok(instance);
      assert.ok(match[2] === 'start' ? instance.status === 'created' : ['created', 'running'].includes(instance.status));
      instance.status = match[2] === 'start' ? 'running' : 'stopped';
      return json(instance, 202);
    }
    throw Error(`Unexpected fixture route: ${method} ${p}`);
  });
  try {
    await page.goto('https://admin.test/#fixture-capability');
    await page.locator('#loginId').fill('operator');
    await page.locator('#loginPassword').fill('login-secret-password');
    await page.locator('#loginForm button').click();
    await expect(page.locator('#management')).toBeVisible();
    const ownRow = page.locator('#usersTable tbody tr').filter({hasText: 'operator-id'});
    const memberRow = page.locator('#usersTable tbody tr').filter({hasText: 'member-id'});
    await expect(ownRow).toContainText('現在ログイン中');
    await expect(ownRow.getByRole('button', {name: '無効にする', exact: true})).toHaveCount(0);
    await memberRow.getByRole('button', {name: '無効にする', exact: true}).click();
    await expect(memberRow).toContainText('無効');
    await expect(ownRow).toContainText('現在ログイン中');
    await expect(page.locator('#management')).toBeVisible();
    assert.equal(unauthorized, 0, 'disabling another user must preserve the administrator session');
    await memberRow.getByRole('button', {name: '有効にする', exact: true}).click();
    assert.equal(statusChanges, 2);
    await page.locator('#language').selectOption('en');
    await expect(ownRow).toContainText('Signed-in account');
    await expect(ownRow.getByRole('button', {name: 'Disable', exact: true})).toHaveCount(0);
    await page.locator('#language').selectOption('ja');
    await page.locator('[data-tab=worlds]').click();
    await page.locator('#worldName').fill('Unsaved administrator draft');
    // Keep an unattended screen open for three simulated days. Each large
    // clock jump also models timers delayed by a suspended/background tab.
    for (let period = 0; period < 6; period++) {
      const before = refreshes;
      await page.clock.fastForward(12 * 60 * 60 * 1000);
      await expect.poll(() => refreshes).toBe(before + 1);
      await expect(page.locator('#sessionContinuity')).toContainText('自動で継続');
      await expect(page.locator('#management')).toBeVisible();
      await expect(page.locator('#worldName')).toHaveValue('Unsaved administrator draft');
    }
    assert.equal(unauthorized, 0, 'routine time passage must renew before a protected request fails');
    const beforeOutage = refreshes;
    refreshOffline = true;
    await page.clock.fastForward(15 * 60 * 1000);
    await expect.poll(() => refreshes).toBe(beforeOutage + 1);
    await expect(page.locator('#sessionContinuity')).toContainText('接続を回復');
    await expect(page.locator('#management')).toBeVisible();
    await expect(page.locator('#worldName')).toHaveValue('Unsaved administrator draft');
    refreshOffline = false;
    await page.clock.fastForward(5000);
    await expect.poll(() => refreshes).toBe(beforeOutage + 2);
    await expect(page.locator('#sessionContinuity')).toContainText('自動で継続');
    // Simulate wake while timers have not run yet. A management operation must
    // await renewal and must not be sent with an expired access token.
    const beforeWake = refreshes;
    await page.clock.setSystemTime(new Date(await page.evaluate(() => Date.now()) + 3600000));
    await page.locator('#refreshWorlds').click();
    await expect.poll(() => refreshes).toBe(beforeWake + 1);
    await expect(page.locator('#sessionContinuity')).toContainText('自動で継続');
    assert.equal(unauthorized, 0);
    const beforeExpiry = refreshes;
    expire = true;
    await page.locator('[data-tab=worlds]').click();
    await expect.poll(() => refreshes).toBe(beforeExpiry + 1);
    await expect(page.locator('#management')).toBeVisible();
    await page.locator('#worldName').fill('Operation test');
    await page.locator('#worldForm button').click();
    await expect(page.locator('#worldsTable')).toContainText('Operation test');
    assert.equal(created, 1);
    await page.locator('#worldsTable button').click();
    await expect(page.locator('#instancesTable')).toContainText('作成済み');
    await page.locator('#instancesTable').getByRole('button', {name: '起動', exact: true}).click();
    await expect(page.locator('#instancesTable')).toContainText('稼働中');
    await expect(page.locator('#instancesTable').getByRole('button', {name: '起動', exact: true})).toHaveCount(0);
    await page.locator('#instancesTable').getByRole('button', {name: '停止', exact: true}).click();
    await expect(page.locator('#instancesTable')).toContainText('再起動できません');
    forbidden = true;
    await page.locator('#worldName').fill('Forbidden test');
    await page.locator('#worldForm button').click();
    await expect(page.locator('#message')).toContainText('HTTP 403');
    await expect(page.locator('#message')).toContainText('req_forbidden');
    await expect(page.locator('#operationRows')).toContainText('NOT_AUTHORIZED');
    const before = refreshes;
    expire = true; revoke = true;
    await page.locator('#refreshWorlds').click();
    await expect(page.locator('#message')).toContainText('管理者認証が無効');
    await expect(page.locator('#management')).toBeHidden();
    await expect(page.locator('#login')).toBeVisible();
    assert.equal(refreshes, before + 1);
    const logs = await page.locator('#operationLog').textContent();
    for (const secret of ['initial-secret-token', 'renewed-secret-token', 'refresh-secret-token', 'login-secret-password', 'fixture-capability']) assert.ok(!logs.includes(secret));
    assert.deepEqual(errors, []);
    console.log('PASS: self-disable prevention, other-user status changes without administrator logout, three simulated days signed in, proactive renewal, outage/draft preservation and retry, wake before timers, genuine revocation, lifecycle controls and secret-free logs');
  } finally {await context.close();}
}

async function real(browser, binary) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'orbisync-admin-operations-'));
  const install = path.join(root, 'installation');
  const environment = {...process.env};
  for (const key of Object.keys(environment)) if (key.startsWith('ORBISYNC_') || key === 'DATABASE_URL') delete environment[key];
  const stream = fs.openSync(path.join(root, 'launcher.log'), 'w');
  const child = spawn(binary, ['web-admin', '--data-dir', install, '--port', '0', '--no-browser'], {cwd: root, env: environment, windowsHide: true, stdio: ['ignore', stream, stream]});
    let control, context;
  const timeout = setTimeout(() => child.kill(), 180000);
  try {
    const link = path.join(install, 'launch-url.txt');
    for (let attempt = 0; !fs.existsSync(link); attempt++) {
      if (child.exitCode !== null || attempt > 150) throw Error('Fixture launcher failed; inspect '+root);
      await wait(100);
    }
    const launch = new URL(fs.readFileSync(link, 'utf8').trim());
    control = async (p, method = 'GET', body) => {
      const response = await fetch(launch.origin + p, {method, headers: {'x-orbisync-local': launch.hash.slice(1), 'Content-Type': 'application/json'}, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(60000)});
      assert.ok(response.ok, `Fixture control failed: ${method} ${p} HTTP ${response.status}`);
      return response;
    };
    context = await browser.newContext({locale: 'ja-JP'});
    const page = await context.newPage(), errors = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.goto(launch.href);
    await page.locator('#startLocalTrial').click();
    await expect(page.locator('#management')).toBeVisible({timeout: 120000});
    await expect(page.locator('#probeSummary')).toContainText('すべて成功', {timeout: 45000});
    await page.locator('#dismissCredential').click();
    // Shorten only this disposable fixture's engine TTL, then sign in again.
    // Verify actual renewal across the original token's real expiry, not just
    // a simulated browser clock or a synthetic unauthorized response.
    const config = path.join(install, 'orbisync.toml');
    assert.ok(!/^\[auth\]/m.test(fs.readFileSync(config, 'utf8')));
    fs.appendFileSync(config, '\n[auth]\naccess_token_ttl_seconds = 60\n');
    await control('/admin/restart', 'POST', {});
    await page.reload();
    const shortSession = page.waitForResponse(r => new URL(r.url()).pathname === '/admin/local-trial' && r.request().method() === 'POST');
    await page.locator('#startLocalTrial').click();
    const issued = await (await shortSession).json();
    assert.equal(issued.expires_in, 60);
    const issuedAt = Date.now();
    await expect(page.locator('#management')).toBeVisible();
    await expect(page.locator('#probeSummary')).toContainText('すべて成功', {timeout: 45000});
    let unintended401s = 0;
    const observeResponse = response => { if (response.status() === 401) unintended401s++; };
    page.on('response', observeResponse);
    await page.locator('[data-tab=worlds]').click();
    await page.locator('#worldName').fill('Draft retained across real expiry');
    await page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/auth/refresh' && r.status() === 200, {timeout: 65000});
    await expect(page.locator('#sessionContinuity')).toContainText('自動で継続');
    await wait(Math.max(0, issuedAt + 65000 - Date.now()));
    await page.locator('#refreshWorlds').click();
    await expect(page.locator('#management')).toBeVisible();
    await expect(page.locator('#worldName')).toHaveValue('Draft retained across real expiry');
    assert.equal(unintended401s, 0);
    page.off('response', observeResponse);
    await page.locator('[data-tab=worlds]').click();
    // Force just one expired-access response, then use the actual engine's
    // refresh endpoint and replay the operation through the real proxy/DB.
    let injectedExpiry = false;
    const expireOnce = async route => {
      if (!injectedExpiry && route.request().method() === 'POST') {
        injectedExpiry = true;
        return route.fulfill({status: 401, json: {error: {code: 'AUTHENTICATION_REQUIRED', request_id: 'req_fixture_expiry'}}});
      }
      await route.continue();
    };
    await page.route('**/api/v1/worlds', expireOnce);
    const renewed = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/auth/refresh');
    const name = 'Real admin operations '+Date.now();
    await page.locator('#worldName').fill(name);
    await page.locator('#worldForm button').click();
    assert.equal((await renewed).status(), 200);
    await page.unroute('**/api/v1/worlds', expireOnce);
    const worldRow = page.locator('#worldsTable tr').filter({hasText: name});
    await expect(worldRow).toBeVisible();
    await worldRow.getByRole('button', {name: 'インスタンスを作成', exact: true}).click();
    const instanceRow = page.locator('#instancesTable tr').filter({hasText: name});
    await expect(instanceRow).toContainText('作成済み');
    await instanceRow.getByRole('button', {name: '起動', exact: true}).click();
    await expect(instanceRow).toContainText('稼働中');
    await instanceRow.getByRole('button', {name: '停止', exact: true}).click();
    await expect(instanceRow).toContainText('停止済み', {timeout: 15000});
    await expect(instanceRow.getByRole('button', {name: '起動', exact: true})).toHaveCount(0);
    const stoppedId = (await instanceRow.getByRole('cell').first().textContent()).trim();
    const credentialsForJoin = JSON.parse(fs.readFileSync(path.join(install, 'local-credentials.json'), 'utf8'));
    const { OrbiSyncClient } = await import(pathToFileURL(path.join(repository, 'sdk/typescript/dist/client.js')).href);
    const engineSettings = await control('/admin/status').then(r => r.json());
    // The engine URL is read from this fixture's explicitly supplied settings,
    // never discovered from an unrelated operator installation.
    const match = fs.readFileSync(config, 'utf8').match(/bind\s*=\s*"([^"]+)"/);
    assert.ok(match && engineSettings.ready);
    const sdk = new OrbiSyncClient({baseUrl: 'http://' + match[1]});
    let connection;
    try {
      await sdk.auth.login({loginId: credentialsForJoin.login_id, password: credentialsForJoin.password});
      connection = await sdk.connect();
      await assert.rejects(connection.join(stoppedId), error => error.code === 'INSTANCE_NOT_RUNNING');
    } finally {
      if (connection) await connection.disconnect();
      await sdk.fetch('/v1/auth/logout', {method:'POST'}).catch(() => {});
    }
    await page.reload();
    const resumed = page.waitForResponse(r => new URL(r.url()).pathname === '/admin/local-trial' && r.request().method() === 'POST');
    await page.locator('#startLocalTrial').click();
    assert.equal((await resumed).status(), 200);
    await expect(page.locator('#management')).toBeVisible({timeout: 45000});
    await page.locator('[data-tab=worlds]').click();
    await expect(page.locator('#worldsTable')).toContainText(name);
    await page.locator('#stopEngine').click();
    await expect(page.locator('#localPhase')).toHaveText('停止済み', {timeout: 45000});
    const restarted = page.waitForResponse(r => new URL(r.url()).pathname === '/admin/local-trial' && r.request().method() === 'POST');
    await page.locator('#startLocalTrial').click();
    assert.equal((await restarted).status(), 200);
    await expect(page.locator('#localPhase')).toHaveText('準備完了', {timeout: 45000});
    await expect.poll(async () => (await (await control('/admin/status')).json()).operation.busy).toBe(false);
    await expect(page.locator('#management')).toBeVisible({timeout: 60000});
    await page.locator('[data-tab=worlds]').click();
    await expect(page.locator('#worldsTable')).toContainText(name);
    const adminLog = fs.readFileSync(path.join(install, 'admin.log'), 'utf8');
    const entries = adminLog.trim().split('\n').map(JSON.parse);
    assert.ok(entries.some(e => e.method === 'POST' && e.status === 201 && e.engine_request_id));
    const engineLog = fs.readFileSync(path.join(install, 'engine.log'), 'utf8');
    assert.ok(engineLog.includes('http.request_completed'));
    const credentials = JSON.parse(fs.readFileSync(path.join(install, 'local-credentials.json'), 'utf8'));
    assert.ok(!adminLog.includes(credentials.password) && !engineLog.includes(credentials.password));
    assert.ok(!adminLog.includes(launch.hash.slice(1)));
    assert.deepEqual(errors, []);
    console.log('PASS: real engine/DB: proactive refresh across real access expiry without 401 or lost draft; World/Instance lifecycle, 401 fallback, reload/resume, engine restart persistence and correlated logs');
    console.log('Fixture diagnostics: '+root);
  } finally {
    clearTimeout(timeout);
    if (context) await context.close();
    if (control) {
      for (let attempt = 0; attempt < 100; attempt++) {
        const status = await control('/admin/status').then(r => r.json()).catch(() => null);
        if (!status?.operation?.busy) break;
        await wait(100);
      }
      await control('/admin/stop', 'POST', {}).catch(() => {});
    }
    if (child.exitCode === null) {
      const exited = new Promise(resolve => child.once('exit', resolve)); child.kill(); await exited;
    }
    fs.closeSync(stream);
    // Preserve the private fixture logs for inspection; never delete operator data.
  }
}

(async () => {
  const browser = await chromium.launch({headless: true});
  try {
    await mocked(browser);
    if (process.argv.includes('--real')) {
      const binary = process.env.ORBISYNC_ADMIN_TEST_BINARY;
      assert.ok(binary && path.isAbsolute(binary), '--real requires an explicit ORBISYNC_ADMIN_TEST_BINARY');
      await real(browser, binary);
    }
  } finally {await browser.close();}
})().catch(error => {console.error(error); process.exitCode = 1;});
