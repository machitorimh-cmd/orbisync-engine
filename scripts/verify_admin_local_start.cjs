// Focused real-browser fixtures; no SDK build, server, database or downloads.
const { chromium, expect } = require(process.env.PLAYWRIGHT_MODULE || '@playwright/test');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
const assets = {
  '/': fs.readFileSync(path.join(root, 'apps/admin-web/index.html'), 'utf8'),
  '/admin.js': fs.readFileSync(path.join(root, 'apps/admin-web/src/main.js'), 'utf8'),
  '/admin-connection.js': fs.readFileSync(path.join(root, 'apps/admin-web/src/local_connection.js'), 'utf8'),
};
const secret = 'postgresql://operator:NEVER_RENDER_THIS@db/private?token=NEVER_RENDER_THIS';
const passed = () => ({ ok: true, checks: ['http', 'websocket', 'join', 'sync'].map(step => ({ step, status: 'passed' })) });
function gate() { let release; const promise = new Promise(resolve => { release = resolve; }); return { promise, release }; }
async function fixture(browser, initial = {}, options = {}) {
  const context = await browser.newContext({ locale: 'ja-JP' });
  if (options.storageBlocked) await context.addInitScript(() => {
    for (const name of ['sessionStorage', 'localStorage']) Object.defineProperty(window, name, { get() { throw new DOMException('Fixture storage blocked', 'SecurityError'); } });
  });
  const page = await context.newPage(); await page.clock.install();
  const state = { initialized: false, ready: false, existing: false, engine: 'not started', local_trial: false, managed_db: null,
    operation: { phase: 'idle', busy: false, error_code: null }, ...initial };
  const calls = [], pageErrors = [], consoleMessages = [], probes = [];
  let trialGate = null, trialFailure = null, credentialsSent = false, diagnostics = { checks: [{ code: 'engine', status: 'passed' }], actions: [] };
  page.on('pageerror', error => pageErrors.push(error.message));
  page.on('console', message => consoleMessages.push(message.text()));
  function ready() { Object.assign(state, { initialized: true, ready: true, engine: 'running', local_trial: true, managed_db: { state: 'running', port: 54329 }, operation: { phase: 'ready', busy: false, error_code: null } }); }
  await context.route('https://admin.test/**', async route => {
    const request = route.request(), pathname = new URL(request.url()).pathname;
    if (Object.hasOwn(assets, pathname)) return route.fulfill({ contentType: pathname.endsWith('.js') ? 'text/javascript' : 'text/html', body: assets[pathname] });
    const body = request.postDataJSON(); calls.push({ pathname, method: request.method(), body, authorization: request.headers().authorization ?? null });
    if (pathname === '/admin/status') return route.fulfill({ json: state });
    if (pathname === '/admin/local-trial') {
      assert.deepEqual(body, {}, 'local trial must not require a DB URL or password');
      if (trialGate) await trialGate.promise;
      if (trialFailure) return route.fulfill({ status: 400, json: { code: trialFailure, message: secret, stderr: secret } });
      ready(); const result = { ok: true, login_id: 'local-admin', access_token: 'fixture-access-token-NEVER_RENDER_THIS' };
      if (!credentialsSent) result.initial_password = 'local-password-shown-once'; credentialsSent = true;
      return route.fulfill({ json: result });
    }
    if (pathname === '/admin/start' || pathname === '/admin/restart' || pathname === '/admin/repair') {
      ready(); return route.fulfill({ json: { ok: true, message: secret } });
    }
    if (pathname === '/admin/stop') {
      Object.assign(state, { ready: false, engine: 'stopped', managed_db: { state: 'stopped', port: 54329 }, operation: { phase: 'stopped', busy: false, error_code: null } });
      return route.fulfill({ json: { ok: true, message: secret } });
    }
    if (pathname === '/admin/probe') {
      const next = probes.shift(); const result = typeof next === 'function' ? await next() : next || passed();
      return route.fulfill({ json: result });
    }
    if (pathname === '/admin/diagnostics') return route.fulfill({ json: diagnostics });
    if (pathname === '/admin/settings') return route.fulfill({ json: { values: { 'server.bind': '127.0.0.1:54328' }, manifest: { version: 1, rules: [] }, read_only: false } });
    if (pathname === '/api/v1/auth/me') return route.fulfill({ json: { login_id: 'local-admin', display_name: 'Trial Admin' } });
    if (['/api/v1/users', '/api/v1/roles'].includes(pathname)) return route.fulfill({ json: { items: [], next_cursor: null } });
    pageErrors.push(`Unexpected request ${pathname}`); return route.fulfill({ status: 500, json: { code: 'unknown', message: secret } });
  });
  await page.goto('https://admin.test/#fixture-launch-capability');
  await expect(page.locator('#readiness')).not.toHaveText('接続中…');
  return { context, page, state, calls, probes, ready, pageErrors,
    setTrialGate(value) { trialGate = value; }, failTrial(code) { trialFailure = code; }, setDiagnostics(value) { diagnostics = value; },
    async noSecrets() {
      const text = await page.locator('body').textContent();
      assert.ok(!text.includes('NEVER_RENDER_THIS'), 'raw errors, credentials and tokens must not enter the DOM');
      assert.ok(!consoleMessages.some(line => line.includes('NEVER_RENDER_THIS')), 'secrets must not enter console logs');
      const stored = await page.evaluate(() => { try { return JSON.stringify([Object.entries(localStorage), Object.entries(sessionStorage)]); } catch { return ''; } });
      assert.ok(!stored.includes('fixture-access-token') && !stored.includes('local-password-shown-once'));
      assert.deepEqual(pageErrors, []);
    },
  };
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const f = await fixture(browser), page = f.page, start = gate(); f.setTrialGate(start);
    await expect(page.locator('#manualSetup')).not.toHaveAttribute('open', '');
    await page.locator('#startLocalTrial').click();
    await expect(page.locator('#startLocalTrial')).toBeDisabled();
    await page.evaluate(() => document.getElementById('startLocalTrial').dispatchEvent(new MouseEvent('click', { bubbles: true })));
    assert.equal(f.calls.filter(call => call.pathname === '/admin/local-trial').length, 1, 'duplicate startup must be coalesced');
    f.state.operation = { phase: 'db_preparing', busy: true, error_code: null };
    await page.clock.fastForward(5000);
    await expect(page.locator('#localPhase')).toHaveText('ローカルDBを準備中…');
    await expect(page.locator('#localDatabaseState')).toHaveText('確認中…');
    f.state.operation.phase = 'initializing'; f.state.managed_db = { state: 'running', port: 54329 };
    await page.clock.fastForward(5000);
    await expect(page.locator('#localSetupState')).toHaveText('確認中…');
    f.state.initialized = true; f.state.operation.phase = 'engine_starting';
    await page.clock.fastForward(5000);
    await expect(page.locator('#localSetupState')).toHaveText('成功');
    await expect(page.locator('#localEngineState')).toHaveText('確認中…');
    start.release();
    await expect(page.locator('#management')).toBeVisible();
    await expect(page.locator('#signedIn')).toHaveText('ログイン中: Trial Admin (local-admin)');
    await expect(page.locator('#credentialTitle')).toHaveText('初期管理者パスワード');
    await expect(page.locator('#credentialValue')).toHaveText('local-password-shown-once');
    await expect(page.locator('#probeSummary')).toContainText('すべて成功');
    for (const step of ['Http', 'Websocket', 'Join', 'Sync']) await expect(page.locator(`#probe${step}State`)).toHaveText('成功');
    assert.equal(f.calls.filter(call => call.pathname === '/admin/probe').length, 1);
    await page.locator('#dismissCredential').click(); await f.noSecrets();
    await page.locator('[data-tab=config]').click();
    await expect(page.locator('#replacementDb')).toBeDisabled();
    await expect(page.locator('#managedDatabaseHint')).toContainText('専用のデータベース');

    await page.locator('#stopEngine').click();
    await expect(page.locator('#localPhase')).toHaveText('停止済み');
    await expect(page.locator('#startEngine')).toBeVisible();
    await expect(page.locator('#probeSummary')).toHaveText('エンジンを起動すると接続をテストできます。');
    await page.locator('#startEngine').click();
    await expect(page.locator('#probeSummary')).toContainText('すべて成功');
    await page.locator('#restartEngine').click();
    await expect(page.locator('#probeSummary')).toContainText('すべて成功');
    assert.equal(f.calls.filter(call => call.pathname === '/admin/local-trial').length, 1, 'start/restart cannot reinitialize a database');

    f.probes.push({ ok: false, message: secret, checks: [{ step: 'http', status: 'passed' }, { step: 'websocket', status: 'failed', code: 'engine_port_in_use', detail: secret }, { step: 'join', status: 'skipped' }, { step: 'sync', status: 'skipped' }] });
    await page.locator('#runProbe').click();
    await expect(page.locator('#probeHttpState')).toHaveText('成功');
    await expect(page.locator('#probeWebsocketState')).toHaveText('失敗');
    await expect(page.locator('#probeJoinState')).toHaveText('未実行');
    await expect(page.locator('#retryProbe')).toBeVisible();
    await f.noSecrets();
    await page.locator('#language').selectOption('en');
    await expect(page.locator('#probeWebsocketState')).toHaveText('Failed');
    await expect(page.locator('#probeSummary')).toContainText('did not complete');

    f.setDiagnostics({ checks: [{ code: 'engine_port_in_use', status: 'failed', detail: secret }], actions: [{ action: 'choose_engine_port' }, { action: 'retry' }, { action: 'delete_database' }], message: secret });
    await page.locator('#runDiagnostics').click();
    await expect(page.locator('#chooseEnginePort')).toBeVisible();
    await expect(page.locator('#diagnosticSummary')).toContainText('engine port is in use');
    await page.locator('#chooseEnginePort').click();
    await expect(page.locator('#probeSummary')).toContainText('Connection confirmed');
    assert.deepEqual(f.calls.find(call => call.pathname === '/admin/repair').body, { action: 'choose_engine_port' });
    f.setDiagnostics({ checks: [{ code: 'port_in_use', status: 'failed' }], actions: [{ action: 'choose_database_port' }] });
    await page.locator('#runDiagnostics').click(); await expect(page.locator('#chooseDatabasePort')).toBeVisible();
    await page.locator('#chooseDatabasePort').click(); await expect(page.locator('#probeSummary')).toContainText('Connection confirmed');
    assert.deepEqual(f.calls.filter(call => call.pathname === '/admin/repair').at(-1).body, { action: 'choose_database_port' });

    f.probes.push({ ok: true, checks: [{ step: 'http', status: 'passed' }], message: secret });
    await page.locator('#runProbe').click();
    await expect(page.locator('#probeSummary')).toContainText('did not complete');
    await expect(page.locator('#probeSyncState')).toHaveText('Not reached');
    f.setDiagnostics({ checks: [{ code: 'operation_in_progress', status: 'skipped' }], actions: [] });
    await page.locator('#runDiagnostics').click();
    await expect(page.locator('#diagnosticSummary')).toContainText('operation is running');
    f.setDiagnostics({ checks: [{ code: 'toString', status: 'failed', message: secret }], actions: [] });
    await page.locator('#runDiagnostics').click();
    await expect(page.locator('#diagnosticSummary')).toHaveText('The operation could not finish. Check setup, then retry.');

    const delayed = gate(); f.probes.push(async () => { await delayed.promise; return passed(); });
    await page.locator('#runProbe').click(); await expect(page.locator('#probeSummary')).toHaveText('Testing the connection…');
    await expect(page.locator('#stopEngine')).toBeDisabled();
    Object.assign(f.state, { ready: false, engine: 'stopped', managed_db: { state: 'stopped', port: 54329 } });
    await page.clock.fastForward(5000); await expect(page.locator('#localPhase')).toHaveText('Stopped');
    const oldResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/admin/probe');
    delayed.release();
    await oldResponse; await page.clock.runFor(20);
    await expect(page.locator('#probeSummary')).toHaveText('Start the engine to test a connection.');
    await expect(page.locator('#probeHttpState')).toHaveText('Not tested');
    // Retry an already signed-in trial, refresh explicitly, and poll once.
    // None of these capability-only operations may expose the engine token.
    await page.locator('#startLocalTrial').click();
    await expect(page.locator('#probeSummary')).toContainText('Connection confirmed');
    await page.locator('#refreshStatus').click(); await page.clock.fastForward(5000);
    const capabilityOnly = ['/admin/status', '/admin/start', '/admin/stop', '/admin/restart', '/admin/diagnostics', '/admin/repair', '/admin/local-trial'];
    for (const pathname of capabilityOnly) {
      const requests = f.calls.filter(call => call.pathname === pathname);
      assert.ok(requests.length > 0, `Missing header coverage for ${pathname}`);
      assert.ok(requests.every(call => call.authorization === null), `Capability-only ${pathname} must never carry Authorization`);
    }
    const authenticated = f.calls.filter(call => call.pathname.startsWith('/api/v1/') || call.pathname === '/admin/settings' || call.pathname === '/admin/probe');
    assert.ok(authenticated.length > 0);
    assert.ok(authenticated.every(call => call.authorization === 'Bearer fixture-access-token-NEVER_RENDER_THIS'), 'Authenticated management and signed-in probes must retain their identity');
    await f.noSecrets(); await f.context.close();

    // A browser reload can resume the same saved installation without asking
    // for credentials that were intentionally shown only once.
    const resumed = await fixture(browser); await resumed.page.locator('#startLocalTrial').click();
    await expect(resumed.page.locator('#management')).toBeVisible(); await resumed.page.locator('#dismissCredential').click();
    await resumed.page.reload(); await expect(resumed.page.locator('#startLocalTrial')).toHaveText('ローカル試用を再開');
    await resumed.page.locator('#startLocalTrial').click(); await expect(resumed.page.locator('#management')).toBeVisible();
    await expect(resumed.page.locator('#credential')).toBeHidden(); await resumed.noSecrets(); await resumed.context.close();

    const missing = await fixture(browser); missing.failTrial('runtime_missing');
    await missing.page.locator('#startLocalTrial').click();
    await expect(missing.page.locator('#localProblem')).toContainText('同梱のデータベース実行環境が見つかりません');
    await expect(missing.page.locator('#retryLocalTrial')).toBeVisible();
    await missing.page.locator('#diagnosticManualSetup').click();
    await expect(missing.page.locator('#databaseUrl')).toBeVisible();
    await missing.noSecrets(); await missing.context.close();

    const existing = await fixture(browser, { initialized: true, existing: true, ready: true, engine: 'running' });
    await expect(existing.page.locator('#localTrial')).toBeHidden();
    await expect(existing.page.locator('#runProbe')).toBeDisabled();
    await expect(existing.page.locator('#probeSummary')).toContainText('既存のデータベースにテスト用データは作成しません');
    assert.equal(existing.calls.filter(call => call.pathname === '/admin/probe').length, 0);
    assert.equal(existing.calls.filter(call => call.pathname === '/admin/local-trial').length, 0);
    await existing.noSecrets(); await existing.context.close();
    const blocked = await fixture(browser, {}, { storageBlocked: true });
    await expect(blocked.page.locator('#startLocalTrial')).toBeVisible(); await blocked.noSecrets(); await blocked.context.close();
    console.log('PASS: one-click local startup, stages, auto sign-in/probe, stop/start/restart, partial failures, explicit port repairs, stale-response fencing, manual fallback, existing installations and secret-safe bilingual rendering.');
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
