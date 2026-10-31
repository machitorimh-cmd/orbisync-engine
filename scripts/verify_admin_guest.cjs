// Creates only a private disposable installation. Never attaches to operator DBs.
// Requires a built binary with PostgreSQL; --http-only needs no SDK dependencies.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {spawn, spawnSync} = require('node:child_process');
const {pathToFileURL} = require('node:url');
const assert = require('node:assert/strict');
const repository = path.resolve(__dirname, '..');
const httpOnly = process.argv.includes('--http-only');
// Use the same Node WebSocket implementation as the SDK's live-runtime suite.
if (!httpOnly) globalThis.WebSocket = require(path.join(repository, 'sdk/typescript/node_modules/ws')).WebSocket;
function uuid7() {
  const bytes = require('node:crypto').randomBytes(16); bytes.writeUIntBE(Date.now(), 0, 6);
  bytes[6] = (bytes[6] & 15) | 112; bytes[8] = (bytes[8] & 63) | 128;
  const h = bytes.toString('hex'); return `${h.slice(0,8)}-${h.slice(8,12)}-${h.slice(12,16)}-${h.slice(16,20)}-${h.slice(20)}`;
}
const binary = process.env.ORBISYNC_ADMIN_TEST_BINARY;
assert.ok(binary && path.isAbsolute(binary), 'Set ORBISYNC_ADMIN_TEST_BINARY to a built server');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'orbisync-guest-settings-'));
const install = path.join(root, 'installation');
const env = {...process.env};
for (const key of Object.keys(env)) if (key.startsWith('ORBISYNC_') || key === 'DATABASE_URL') delete env[key];
const log = fs.openSync(path.join(root, 'launcher.log'), 'w');
const child = spawn(binary, ['web-admin', '--data-dir', install, '--port', '0', '--no-browser'], {cwd: root, env, windowsHide: true, stdio: ['ignore', log, log]});
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
let launch, token, sdk, connection;
async function call(route, method = 'GET', body, expected = 200, bearer = token) {
  const response = await fetch(launch.origin + route, {method, headers: {'x-orbisync-local': launch.hash.slice(1), 'Content-Type': 'application/json', ...(bearer ? {Authorization: 'Bearer ' + bearer} : {})}, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(route === '/admin/local-trial' ? 120000 : 45000)})
    .catch(error => {throw new Error(`${method} ${route}: ${error.name}`);});
  assert.equal(response.status, expected, `${method} ${route} returned HTTP ${response.status}`);
  return response.status === 204 ? null : response.json();
}
async function closeGuest() {
  if (connection) await connection.disconnect();
  if (sdk) await sdk.fetch('/v1/auth/logout', {method: 'POST'}).catch(() => {});
  connection = null; sdk = null;
}
(async () => {
  try {
    const link = path.join(install, 'launch-url.txt');
    for (let n = 0; !fs.existsSync(link); n++) {assert.ok(n < 150 && child.exitCode === null, 'Fixture did not start: ' + root); await wait(100);}
    launch = new URL(fs.readFileSync(link, 'utf8').trim());
    const trial = await call('/admin/local-trial', 'POST', {}); token = trial.access_token;
    const {createUuidV7, OrbiSyncClient} = httpOnly ? {createUuidV7: uuid7} : await import(pathToFileURL(path.join(repository, 'sdk/typescript/dist/client.js')).href);
    // Ordinary management writes require v7 idempotency keys.
    async function api(route, body, expected = 201) {
      const response = await fetch(launch.origin + '/api/v1/' + route, {method: 'POST', headers: {'x-orbisync-local': launch.hash.slice(1), Authorization: 'Bearer ' + token, 'Content-Type': 'application/json', 'Idempotency-Key': createUuidV7()}, body: JSON.stringify(body), signal: AbortSignal.timeout(15000)});
      assert.equal(response.status, expected, route + ' failed'); return response.json();
    }
    const world = await api('worlds', {name: 'Guest allowed', capacity: 8});
    const denied = await api('worlds', {name: 'Guest denied', capacity: 8});
    const allowedInstance = await api('instances', {world_id: world.id});
    const deniedInstance = await api('instances', {world_id: denied.id});
    await api('instances/' + allowedInstance.id + '/start', {}, 202);
    await api('instances/' + deniedInstance.id + '/start', {}, 202);
    const configPath = path.join(install, 'orbisync.toml');
    const engineUrl = 'http://' + fs.readFileSync(configPath, 'utf8').match(/bind\s*=\s*"([^"]+)"/)[1];
    let settings = await call('/admin/settings'); assert.equal(settings.guest.enabled, false);
    const initialConfig = fs.readFileSync(configPath, 'utf8');
    const edit = {values: settings.values, manifest: settings.manifest, guest: {enabled: true, allowed_worlds: []}};
    await call('/admin/settings', 'POST', edit, 400);
    edit.guest.allowed_worlds = [createUuidV7()]; await call('/admin/settings', 'POST', edit, 400);
    edit.guest.allowed_worlds = [allowedInstance.id]; await call('/admin/settings', 'POST', edit, 400);
    assert.equal(fs.readFileSync(configPath, 'utf8'), initialConfig, 'Rejected settings changed the file');
    edit.guest.allowed_worlds = [world.id]; await call('/admin/settings', 'POST', edit);
    settings = await call('/admin/settings');
    assert.equal(settings.guest.enabled, true); assert.deepEqual(settings.guest.allowed_worlds, [world.id]);
    const roles = await call('/api/v1/roles'); const guestRole = roles.items.find(r => r.name === 'Guest');
    assert.deepEqual(guestRole.permissions, ['world.instance.read'], 'Automatic role must grant only room participation');
    assert.equal((await fetch(engineUrl + '/v1/auth/guest', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: '{}'})).status, 403, 'Saved config applied before restart');
    await call('/admin/restart', 'POST', {});
    if (!httpOnly) {
      sdk = new OrbiSyncClient({baseUrl: engineUrl}); await sdk.auth.guest(); connection = await sdk.connect();
      await assert.rejects(connection.join(deniedInstance.id), error => error.code === 'NOT_AUTHORIZED');
      await closeGuest();
      sdk = new OrbiSyncClient({baseUrl: engineUrl}); await sdk.auth.guest(); connection = await sdk.connect();
      const instance = await connection.join(allowedInstance.id); await instance.ready();
    }
    assert.equal((await call('/api/v1/auth/me')).enabled, true, 'Guest Join must not replace the administrator');
    const guestLogin = await fetch(engineUrl + '/v1/auth/guest', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: '{}'});
    assert.equal(guestLogin.status, 200);
    const guestToken = (await guestLogin.json()).access_token;
    await call('/admin/settings', 'GET', undefined, 403, guestToken);
    await call('/admin/settings', 'POST', edit, 403, guestToken);
    await fetch(engineUrl + '/v1/auth/logout', {method: 'POST', headers: {Authorization: 'Bearer ' + guestToken}});
    await closeGuest();
    const beforeDisable = fs.readFileSync(configPath, 'utf8');
    edit.guest.enabled = false; await call('/admin/settings', 'POST', edit);
    settings = await call('/admin/settings'); assert.equal(settings.guest.enabled, false); assert.deepEqual(settings.guest.allowed_worlds, [world.id]);
    assert.ok(beforeDisable.includes('[auth]'));
    await call('/admin/restart', 'POST', {});
    const methods = await (await fetch(engineUrl + '/v1/auth/methods')).json();
    assert.ok(methods.methods.includes('local')); assert.ok(!methods.methods.includes('guest'));
    assert.equal((await fetch(engineUrl + '/v1/auth/guest', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: '{}'})).status, 403);
    assert.equal((await call('/api/v1/auth/me')).enabled, true);
    await call('/api/v1/auth/logout', 'POST', undefined, 204); token = null;
    console.log('PASS: guest enable/disable through admin settings, automatic join-only role, unknown/Instance IDs rejected without saving, restart persistence, real guest authentication, administrator preserved, guest settings RBAC and session cleanup' + (httpOnly ? '' : ', WebSocket/Join/initial sync and unselected World denied'));
  } finally {
    await closeGuest().catch(() => {});
    if (launch) {
      // A client timeout does not cancel an in-progress initialization. Let the
      // owned operation settle before asking the launcher to stop its database.
      for (let n = 0; n < 240; n++) {
        const status = await call('/admin/status').catch(() => null);
        if (!status?.operation?.busy) break;
        await wait(250);
      }
      if (token) await call('/api/v1/auth/logout', 'POST', undefined, 204).catch(() => {});
      await call('/admin/stop', 'POST', {}).catch(() => {});
    }
    if (child.exitCode === null) {const exited = new Promise(resolve => child.once('exit', resolve)); child.kill(); await exited;}
    const data = path.join(install, 'local-postgres/data');
    if (fs.existsSync(path.join(data, 'postmaster.pid'))) {
      assert.ok(fs.realpathSync(data).startsWith(fs.realpathSync(root) + path.sep), 'Cleanup escaped the owned fixture');
      const pgctl = path.join(path.dirname(binary), 'runtime/postgres/bin', process.platform === 'win32' ? 'pg_ctl.exe' : 'pg_ctl');
      const stopped = spawnSync(pgctl, ['stop', '-D', data, '-m', 'fast', '-w', '-t', '30'], {windowsHide: true, stdio: 'pipe', timeout: 35000});
      assert.equal(stopped.status, 0, 'Could not stop the owned fixture database');
    }
    fs.closeSync(log);
  }
})().catch(error => {console.error(error.message + '\nPrivate fixture logs: ' + root); process.exitCode = 1;});
