// UI-owned copy only. Server diagnostics, names, IDs, permissions and JSON stay raw.
const japanese = {
  'OrbiSync · Engine administration': 'OrbiSync · エンジン管理',
  'OrbiSync / operator console': 'OrbiSync / 管理コンソール',
  'Engine administration': 'エンジン管理',
  'Set up once. Manage your engine here.': '初期設定から日々のエンジン管理まで。',
  'Connecting…': '接続中…',
  'One-time temporary password': '一度だけ表示される仮パスワード',
  'Save it securely, then change it after signing in.': '安全に保存し、ログイン後に変更してください。',
  'I have saved it': '保存しました',
  '01 / FIRST START': '01 / 初期設定',
  'Connect your database': 'データベースに接続',
  'Use a new, empty PostgreSQL database. Initialization applies the engine migrations, creates keys automatically, and creates your first administrator.': '新しい空の PostgreSQL データベースを使用してください。初期化ではエンジンのマイグレーションを適用し、鍵と最初の管理者を自動作成します。',
  'PostgreSQL connection URL': 'PostgreSQL 接続 URL',
  'Engine address': 'エンジンのアドレス',
  'Environment': '環境',
  'Production — operator password corpus': '本番環境 — 管理者が用意したパスワードコーパス',
  'Local development — generated placeholder corpus': 'ローカル開発 — 生成した仮のコーパス',
  'Password corpus file (on the server)': 'パスワードコーパスファイル（サーバー上）',
  'Absolute path to 10,000 distinct passwords': '重複のない 10,000 件のパスワードを含むファイルの絶対パス',
  'Local development only. The generated placeholder corpus blocks no real common passwords. Replace it with an operator-approved corpus before real users log in.': 'ローカル開発専用です。生成した仮のコーパスでは、実際によく使われるパスワードを拒否できません。実ユーザーがログインする前に、管理者が承認したコーパスに置き換えてください。',
  'Administrator login': '管理者ログイン',
  'Administrator display name': '管理者の表示名',
  'Check connection': '接続を確認',
  'Initialize this database': 'このデータベースを初期化',
  '02 / ENGINE': '02 / エンジン',
  'Start your engine': 'エンジンを起動',
  'Configuration saved.': '設定を保存しました。',
  'Start engine': 'エンジンを起動',
  'Refresh status': '状態を更新',
  'Restart required: close this launcher with Ctrl+C, run the same command again, and click Start engine. Saved changes have not been applied to the running engine.': '再起動が必要です。Ctrl+C でこのランチャーを終了し、同じコマンドを再実行して「エンジンを起動」を押してください。保存した変更は稼働中のエンジンには未適用です。',
  '03 / SIGN IN': '03 / ログイン',
  'Login': 'ログイン ID',
  'Password': 'パスワード',
  'Sign in': 'ログイン',
  'New administrator? Sign in with your temporary password, then use Change password below.': '初めてログインする管理者は、仮パスワードでログイン後、下の「パスワードを変更」を使用してください。',
  'Sign out': 'ログアウト',
  'Change password': 'パスワードを変更',
  'Current password': '現在のパスワード',
  'New password': '新しいパスワード',
  'Use at least 12 characters. Password policy is checked by the engine. After changing your password, sign in again.': '12 文字以上で入力してください。エンジンがパスワードポリシーを検証します。変更後は再度ログインしてください。',
  'Administration sections': '管理メニュー',
  'Users': 'ユーザー',
  'Roles': 'ロール',
  'Worlds & instances': 'ワールドとインスタンス',
  'Configuration': '設定',
  'Refresh': '更新',
  'Display name': '表示名',
  'Create user': 'ユーザーを作成',
  'Load more users': 'ユーザーをさらに表示',
  'Assign roles to a user': 'ユーザーにロールを割り当て',
  'User': 'ユーザー',
  "Saving replaces the user's full role set. Select a user to load their current roles.": '保存すると、そのユーザーのロール全体を置き換えます。ユーザーを選ぶと現在のロールを読み込みます。',
  'Save role assignments': 'ロールの割り当てを保存',
  'Role name': 'ロール名',
  'Description': '説明',
  'Permissions (one per line)': '権限（1 行に 1 件）',
  'Create role': 'ロールを作成',
  'Load more roles': 'ロールをさらに表示',
  'World name': 'ワールド名',
  'Capacity': '定員',
  'Create world': 'ワールドを作成',
  'Load more worlds': 'ワールドをさらに表示',
  'Instances': 'インスタンス',
  'Load more instances': 'インスタンスをさらに表示',
  'Reload saved settings': '保存済みの設定を再読込',
  'Saved values apply on restart. Database credentials and generated keys are never displayed. Administrator role-assignment permission is required.': '保存した値は再起動時に適用されます。データベース認証情報と生成した鍵は表示しません。管理者のロール割り当て権限が必要です。',
  'Replace database connection URL (blank keeps current)': 'データベース接続 URL を変更（空欄なら現在の値を維持）',
  'External input rules': '外部入力ルール',
  "Version 1 JSON manifest. Bind an existing world to a rule, component key and HTTPS service. signing_secret_ref names an existing environment secret; never enter the secret itself here. Validation uses the engine's input-rule validator.": 'バージョン 1 の JSON マニフェストです。既存ワールドにルール、コンポーネントキー、HTTPS サービスを関連付けます。signing_secret_ref は既存の環境シークレット名です。秘密の値自体は入力しないでください。エンジンの入力ルール検証を使用します。',
  'Manifest': 'マニフェスト',
  'Validate & save for restart': '検証して保存（再起動で適用）',
  'Local administration · Existing engine authorization and validation apply to every operation.': 'ローカル管理 · すべての操作に既存エンジンの認可と検証が適用されます。',
  'Engine ready': 'エンジン準備完了',
  'Engine not ready': 'エンジン準備未完了',
  'Setup required': '初期設定が必要です',
  'Engine: ': 'エンジン: ',
  ' · Existing installation; configuration is read-only.': ' · 既存のインストールのため、設定は読み取り専用です。',
  'Signed in as ': 'ログイン中: ',
  'Signed in. If this is a temporary password, change it now.': 'ログインしました。仮パスワードの場合は、すぐに変更してください。',
  'Signed out.': 'ログアウトしました。',
  'Password changed. Sign in with your new password.': 'パスワードを変更しました。新しいパスワードでログインしてください。',
  'No records yet.': 'データはまだありません。',
  'Login / ID': 'ログイン / ID',
  'Name': '名前',
  'Status': '状態',
  'Actions': '操作',
  'Enabled': '有効',
  'Disabled': '無効',
  'Disable': '無効にする',
  'Enable': '有効にする',
  'Reset password': 'パスワードをリセット',
  'Role': 'ロール',
  'Permissions': '権限',
  'Role assignments saved.': 'ロールの割り当てを保存しました。',
  'Role created.': 'ロールを作成しました。',
  'World / ID': 'ワールド / ID',
  'Create instance': 'インスタンスを作成',
  'Instance': 'インスタンス',
  'World': 'ワールド',
  'Start': '起動',
  'Stop': '停止',
  'World created.': 'ワールドを作成しました。',
  'Database pool limit': 'データベース接続プール上限',
  'Database acquire timeout (seconds)': 'データベース接続取得タイムアウト（秒）',
  'Readiness timeout (seconds)': '準備確認タイムアウト（秒）',
  'Realtime connection limit': 'リアルタイム接続上限',
  'Default world capacity': 'ワールドの既定定員',
  'Managed by the existing installation.': '既存のインストールで管理されています。',
  'Existing configuration is read-only. Use its original operator workflow for changes.': '既存の設定は読み取り専用です。変更には従来の管理手順を使用してください。',
  'Open the private administration URL printed by the web-admin command. This page needs the launch capability in its URL fragment.': 'web-admin コマンドが出力した専用の管理 URL を開いてください。このページには URL フラグメント内の起動用アクセス情報が必要です。',
  'Launcher disconnected': 'ランチャーとの接続が切れました',
  'stopped; inspect server diagnostic and restart launcher': '停止済み。サーバーの診断情報を確認し、ランチャーを再起動してください',
  'Server response: ': 'サーバーからの応答: ',
  'Request failed': 'リクエストに失敗しました',
  'Connection successful. Database is empty and eligible for initialization.': '接続できました。データベースは空で、初期化できます。',
  'Initialized. Save the one-time password, start the engine, then sign in and change it.': '初期化しました。一度だけ表示されるパスワードを保存し、エンジンを起動してログイン後に変更してください。',
  'Engine start requested. Readiness will confirm when it accepts requests.': 'エンジンの起動を要求しました。リクエストを受け付けるようになると準備完了と表示されます。',
  'Saved. Restart the launcher and click Start engine to apply. Running engine is unchanged.': '保存しました。ランチャーを再起動して「エンジンを起動」を押すと適用されます。稼働中のエンジンは変更されていません。',
  'Open the private URL printed by this launcher, or sign in with the required role.': 'このランチャーが出力した専用 URL を開くか、必要なロールでログインしてください。',
  'Enter a value.': '値を入力してください。',
  'Enter a number.': '数値を入力してください。',
  'Use at least {min} characters.': '{min} 文字以上で入力してください。',
  'Enter a value of at least {min}.': '{min} 以上の値を入力してください。',
  'Enter a value no greater than {max}.': '{max} 以下の値を入力してください。',
  'Enter a valid value.': '有効な値を入力してください。',
  'Invalid JSON manifest.': 'JSON マニフェストが不正です。',
  'not started': '未起動', 'running': '稼働中', 'stopped': '停止済み',
  'starting': '起動中', 'stopping': '停止中', 'created': '作成済み',
  'active': '有効', 'archived': 'アーカイブ済み',
};

let language;
try { language = localStorage.getItem('orbisync-admin-language'); } catch { /* Storage can be blocked. */ }
if (!['ja', 'en'].includes(language)) language = /^ja(?:-|$)/i.test(navigator.language || '') ? 'ja' : 'en';
const bindings = new Map();
const t = key => language === 'ja' ? japanese[key] ?? key : key;
// Bind only UI-owned text. Never traverse subsequently loaded API data.
function localized(node, render) {
  const update = typeof render === 'function' ? render : () => t(render);
  bindings.set(node, update);
  node.textContent = update();
  return node;
}
function uiText(key) { return localized(document.createTextNode(''), key); }
function validation(input) {
  input.setCustomValidity('');
  const v = input.validity;
  const key = v.badInput ? 'Enter a number.' : v.valueMissing ? 'Enter a value.' : v.tooShort ? 'Use at least {min} characters.' : v.rangeUnderflow ? 'Enter a value of at least {min}.' : v.rangeOverflow ? 'Enter a value no greater than {max}.' : !v.valid ? 'Enter a valid value.' : '';
  input.setCustomValidity(t(key).replace('{min}', v.tooShort ? input.minLength : input.min).replace('{max}', input.max));
}
function applyLanguage() {
  document.documentElement.lang = language;
  for (const [node, render] of bindings) {
    if (!node.isConnected) bindings.delete(node);
    else node.textContent = render();
  }
  document.querySelectorAll('[data-language-attribute]').forEach(el => {
    const attribute = el.dataset.languageAttribute;
    el.setAttribute(attribute, t(el.dataset.languageSource));
  });
  document.querySelectorAll('input,select,textarea').forEach(input => {
    if (input.validity.customError) validation(input);
  });
}
function initializeLanguage() {
  const walker = document.createTreeWalker(document.documentElement, NodeFilter.SHOW_TEXT);
  const nodes = [];
  while (walker.nextNode()) {
    const node = walker.currentNode;
    if (!node.parentElement.closest('script,style') && Object.hasOwn(japanese, node.textContent.trim())) nodes.push(node);
  }
  for (const node of nodes) {
    const text = node.textContent, key = text.trim();
    localized(node, () => text.replace(key, t(key)));
  }
  for (const el of document.querySelectorAll('[placeholder],[aria-label]')) {
    const attribute = el.hasAttribute('placeholder') ? 'placeholder' : 'aria-label';
    const source = el.getAttribute(attribute);
    if (Object.hasOwn(japanese, source)) { el.dataset.languageAttribute = attribute; el.dataset.languageSource = source; }
  }
  const control = document.getElementById('language');
  control.value = language;
  control.addEventListener('change', () => {
    language = control.value;
    try { localStorage.setItem('orbisync-admin-language', language); } catch { /* Keep the choice for this page. */ }
    applyLanguage();
  });
  document.addEventListener('invalid', event => validation(event.target), true);
  document.addEventListener('input', event => { if (event.target.setCustomValidity) event.target.setCustomValidity(''); });
  applyLanguage();
}

// Plain browser module embedded in the Rust executable. No consumer build step.
const $ = id => document.getElementById(id);
const capability = location.hash.slice(1) || sessionStorage.getItem('orbisync-local') || '';
if (capability) sessionStorage.setItem('orbisync-local', capability);
history.replaceState(null, '', location.pathname);
let accessToken = null;
let lastStatus = null;
let assignedIds = [];
const pages = {};
const records = { users: [], roles: [], worlds: [], instances: [] };
function notice(message, error = false, raw = true) { $('message').hidden = false; localized($('message'), () => raw ? t('Server response: ') + message : t(message)); $('message').classList.toggle('error', error); }
function errorNotice(error) {
  notice(error.message, true);
  if (error.uiRender) localized($('message'), error.uiRender);
}
function parseManifest() {
  try { return JSON.parse($('manifest').value); }
  catch (error) { error.uiRender = () => `${t('Invalid JSON manifest.')}\n${error.message}`; throw error; }
}
function uiNotice(message, error = false) { notice(message, error, false); }
function credential(value) { $('credentialValue').textContent = value; $('credential').hidden = false; }
function clearCredential() { $('credentialValue').textContent = ''; $('credential').hidden = true; }
// API idempotency keys require UUIDv7, not crypto.randomUUID() (UUIDv4).
function uuid7() {
  const bytes = crypto.getRandomValues(new Uint8Array(16)); let time = BigInt(Date.now());
  for (let i = 5; i >= 0; i--) { bytes[i] = Number(time & 255n); time >>= 8n; }
  bytes[6] = (bytes[6] & 15) | 112; bytes[8] = (bytes[8] & 63) | 128;
  const h = Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('');
  return `${h.slice(0,8)}-${h.slice(8,12)}-${h.slice(12,16)}-${h.slice(16,20)}-${h.slice(20)}`;
}
async function request(path, method = 'GET', body, extra = {}) {
  const headers = { 'x-orbisync-local': capability, ...extra };
  if (accessToken) headers.Authorization = `Bearer ${accessToken}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (method !== 'GET') headers['Idempotency-Key'] = uuid7();
  const response = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), cache: 'no-store' });
  const text = await response.text(); let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* Error bodies may be plain text. */ }
  if (!response.ok) {
    const raw = data?.error?.message || data?.message;
    if (raw) throw new Error(raw);
    const error = new Error(text);
    error.uiRender = () => `${t('Request failed')} (${response.status}). ${response.status === 403 ? t('Open the private URL printed by this launcher, or sign in with the required role.') : ''}${text ? `\n${text}` : ''}`;
    throw error;
  }
  return data;
}
const api = (path, method, body, headers) => request(`/api/v1/${path}`, method, body, headers);
function action(id, handler, event = 'click') {
  $(id).addEventListener(event, async e => {
    e.preventDefault(); const buttons = [...$(id).querySelectorAll('button'), ...($(id).tagName === 'BUTTON' ? [$(id)] : [])];
    buttons.forEach(b => b.disabled = true);
    try { await handler(e); } catch (error) { errorNotice(error); }
    finally { buttons.forEach(b => b.disabled = false); }
  });
}
function setupBody() { return {database_url: $('databaseUrl').value, engine_bind: $('engineBind').value, local_development: document.querySelector('input[name=environment]:checked').value === 'development', corpus_path: $('corpusPath').value, login_id: $('adminLogin').value, display_name: $('adminName').value}; }
async function refreshStatus() {
  lastStatus = await request('/admin/status');
  $('setup').hidden = lastStatus.initialized; $('engine').hidden = !lastStatus.initialized;
  $('login').hidden = !lastStatus.initialized || !!accessToken;
  localized($('readiness'), lastStatus.ready ? 'Engine ready' : lastStatus.initialized ? 'Engine not ready' : 'Setup required');
  localized($('engineStatus'), () => `${t('Engine: ')}${t(lastStatus.engine)}${lastStatus.existing ? t(' · Existing installation; configuration is read-only.') : ''}`);
  $('startEngine').hidden = lastStatus.engine !== 'not started';
  $('restartNotice').hidden = !lastStatus.restart_required;
}
action('refreshStatus', refreshStatus);
action('dismissCredential', clearCredential);
$('environment').addEventListener('change', () => {const dev = document.querySelector('input[name=environment]:checked').value === 'development'; $('corpusLabel').hidden = dev; $('devWarning').hidden = !dev;});
action('checkConnection', async () => uiNotice((await request('/admin/check', 'POST', setupBody())).message));
action('setupForm', async () => {
  const result = await request('/admin/initialize', 'POST', setupBody());
  $('loginId').value = $('adminLogin').value; $('databaseUrl').value = '';
  credential(result.temporary_password); uiNotice(result.message); await refreshStatus();
}, 'submit');
action('startEngine', async () => { uiNotice((await request('/admin/start', 'POST', {})).message); await refreshStatus(); });
function clearSession() {
  accessToken = null; $('session').hidden = true; $('management').hidden = true; $('login').hidden = false;
  $('currentPassword').value = ''; $('newPassword').value = ''; clearCredential();
  for (const name of Object.keys(records)) { records[name] = []; pages[name] = null; $(`${name}Table`).replaceChildren(); }
  $('roleChoices').replaceChildren(); $('assignUser').replaceChildren(); $('manifest').value = ''; $('configFields').replaceChildren();
}
action('loginForm', async () => {
  const result = await api('auth/login', 'POST', {login_id: $('loginId').value, password: $('loginPassword').value});
  accessToken = result.access_token; $('loginPassword').value = '';
  const me = await api('auth/me'); localized($('signedIn'), () => `${t('Signed in as ')}${me.display_name} (${me.login_id})`);
  $('session').hidden = false; $('management').hidden = false; $('login').hidden = true; $('passwordDetails').open = true;
  uiNotice('Signed in. If this is a temporary password, change it now.');
  try { await loadUsers(); await loadRoles(); } catch (error) { errorNotice(error); }
}, 'submit');
action('logout', async () => { try {await api('auth/logout', 'POST');} finally {clearSession(); uiNotice('Signed out.');} });
action('passwordForm', async () => {await api('auth/change-password', 'POST', {current_password: $('currentPassword').value, new_password: $('newPassword').value}); clearSession(); uiNotice('Password changed. Sign in with your new password.');}, 'submit');
function table(target, columns, rows) {
  const root = $(target); root.replaceChildren();
  if (!rows.length) {const p = document.createElement('p'); p.className = 'empty'; localized(p, 'No records yet.'); root.append(p); return;}
  const t = document.createElement('table'), head = t.createTHead().insertRow(), body = t.createTBody();
  for (const label of columns) {const th = document.createElement('th'); localized(th, label); head.append(th);}
  for (const cells of rows) {const row = body.insertRow(); for (const value of cells) {const cell = row.insertCell(); if (value instanceof Node) cell.append(value); else cell.textContent = String(value ?? '');}}
  root.append(t);
}
function buttons(specs) {const box = document.createElement('div'); for (const [title, fn] of specs) {const b = document.createElement('button'); localized(b, title); b.className = 'secondary'; b.addEventListener('click', async () => {b.disabled = true; try {await fn();} catch (error) {errorNotice(error);} finally {b.disabled = false;}}); box.append(b);} return box;}
async function page(name, more = false) {
  const result = await api(`${name}?limit=50${more && pages[name] ? `&cursor=${encodeURIComponent(pages[name])}` : ''}`);
  records[name] = more ? [...records[name], ...result.items] : result.items; pages[name] = result.next_cursor;
  $(`more${name[0].toUpperCase()}${name.slice(1)}`).hidden = !result.next_cursor;
  return records[name];
}
async function loadUsers(more = false) {
  const users = await page('users', more);
  table('usersTable', ['Login / ID', 'Name', 'Status', 'Actions'], users.map(u => [ `${u.login_id}\n${u.id}`, u.display_name, uiText(u.enabled ? 'Enabled' : 'Disabled'), buttons([[u.enabled ? 'Disable' : 'Enable', async () => {await api(`users/${u.id}/${u.enabled ? 'disable' : 'enable'}`, 'POST'); await loadUsers();}], ['Reset password', async () => {const result = await api(`users/${u.id}/reset-password`, 'POST'); credential(result.temporary_password);} ]]) ]));
  const selected = $('assignUser').value; $('assignUser').replaceChildren();
  for (const u of users) {const opt = new Option(`${u.login_id} — ${u.display_name}`, u.id); $('assignUser').add(opt);}
  if (users.some(u => u.id === selected)) $('assignUser').value = selected;
  if (records.roles.length) await loadAssignments();
}
action('refreshUsers', () => loadUsers()); action('moreUsers', () => loadUsers(true));
action('userForm', async () => {const result = await api('users', 'POST', {login_id: $('newLogin').value, display_name: $('newName').value}, {Accept: 'application/vnd.orbisync.user-credential+json'}); credential(result.temporary_password); $('userForm').reset(); await loadUsers();}, 'submit');
async function loadRoles(more = false) {
  const roles = await page('roles', more);
  table('rolesTable', ['Role', 'Description', 'Permissions'], roles.map(r => [r.name, r.description, r.permissions.join('\n')]));
  $('roleChoices').replaceChildren();
  for (const r of roles) {const label = document.createElement('label'), check = document.createElement('input'); check.type = 'checkbox'; check.value = r.id; label.append(check, ` ${r.name}`); $('roleChoices').append(label);}
  await loadAssignments();
}
async function loadAssignments() {
  if (!$('assignUser').value) return;
  const user = $('assignUser').value;
  const save = $('assignForm').querySelector('button'); save.disabled = true;
  const result = await api(`users/${user}/roles`);
  if ($('assignUser').value !== user) return;
  assignedIds = result.role_ids || (result.roles || []).map(r => r.id);
  $('roleChoices').querySelectorAll('input').forEach(c => c.checked = assignedIds.includes(c.value));
  save.disabled = false;
}
$('assignUser').addEventListener('change', () => loadAssignments().catch(error => errorNotice(error)));
action('assignForm', async () => {const visible = [...$('roleChoices').querySelectorAll('input')].map(c => c.value); const role_ids = [...assignedIds.filter(id => !visible.includes(id)), ...[...$('roleChoices').querySelectorAll('input:checked')].map(c => c.value)]; await api(`users/${$('assignUser').value}/roles`, 'PUT', {role_ids}); uiNotice('Role assignments saved.');}, 'submit');
action('refreshRoles', () => loadRoles()); action('moreRoles', () => loadRoles(true));
action('roleForm', async () => {await api('roles', 'POST', {name: $('roleName').value, description: $('roleDescription').value || null, permissions: $('rolePermissions').value.split(/\r?\n/).map(v => v.trim()).filter(Boolean)}); $('roleForm').reset(); await loadRoles(); uiNotice('Role created.');}, 'submit');
async function loadWorlds(more = false) {const worlds = await page('worlds', more); table('worldsTable', ['World / ID', 'Status', 'Actions'], worlds.map(w => [`${w.name}\n${w.id}`, uiText(w.status), buttons([['Create instance', async () => {await api('instances', 'POST', {world_id: w.id}); await loadInstances();}]])]));}
async function loadInstances(more = false) {const instances = await page('instances', more); table('instancesTable', ['Instance', 'World', 'Status', 'Actions'], instances.map(i => [i.id, records.worlds.find(w => w.id === i.world_id)?.name || i.world_id, uiText(i.status), buttons(['start', 'stop'].map(op => [op === 'start' ? 'Start' : 'Stop', async () => {await api(`instances/${i.id}/${op}`, 'POST'); await loadInstances();}]))]));}
action('refreshWorlds', async () => {await loadWorlds(); await loadInstances();}); action('moreWorlds', () => loadWorlds(true)); action('moreInstances', () => loadInstances(true));
action('worldForm', async () => {await api('worlds', 'POST', {name: $('worldName').value, capacity: Number($('worldCapacity').value)}); $('worldName').value = ''; await loadWorlds(); uiNotice('World created.');}, 'submit');
const labels = {'server.bind':'Engine address', 'database.max_connections':'Database pool limit', 'database.acquire_timeout_seconds':'Database acquire timeout (seconds)', 'database.readiness_timeout_seconds':'Readiness timeout (seconds)', 'realtime.max_connections':'Realtime connection limit', 'world.default_capacity':'Default world capacity'};
async function loadSettings() {
  const result = await request('/admin/settings'); $('configFields').replaceChildren();
  for (const [key, value] of Object.entries(result.values)) {const label = document.createElement('label'), input = document.createElement('input'); label.append(uiText(labels[key] || key)); input.value = value; input.dataset.key = key; input.disabled = result.read_only; label.append(input); $('configFields').append(label);}
  $('manifest').value = result.manifest ? JSON.stringify(result.manifest, null, 2) : '';
  $('manifest').placeholder = t('Managed by the existing installation.');
  $('manifest').dataset.languageAttribute = 'placeholder'; $('manifest').dataset.languageSource = 'Managed by the existing installation.';
  for (const id of ['manifest', 'replacementDb', 'saveSettings']) $(id).disabled = result.read_only;
  if (result.read_only) uiNotice('Existing configuration is read-only. Use its original operator workflow for changes.');
}
action('refreshSettings', loadSettings);
action('settingsForm', async () => {const values = Object.fromEntries([...$('configFields').querySelectorAll('input')].map(i => [i.dataset.key, i.value])); const result = await request('/admin/settings', 'POST', {values, database_url: $('replacementDb').value, manifest: parseManifest()}); $('replacementDb').value = ''; uiNotice(result.message); await refreshStatus();}, 'submit');
for (const button of document.querySelectorAll('[data-tab]')) button.addEventListener('click', async () => {
  const name = button.dataset.tab;
  for (const b of document.querySelectorAll('[data-tab]')) {b.setAttribute('aria-selected', String(b === button)); $(b.dataset.tab).hidden = b !== button;}
  try {if (name === 'worlds') {await loadWorlds(); await loadInstances();} else if (name === 'config') await loadSettings(); else if (name === 'roles') await loadRoles(); else await loadUsers();} catch (error) {errorNotice(error);}
});
initializeLanguage();
if (!capability) uiNotice('Open the private administration URL printed by the web-admin command. This page needs the launch capability in its URL fragment.', true);
else {refreshStatus().catch(error => errorNotice(error)); setInterval(() => refreshStatus().catch(() => { localized($('readiness'), 'Launcher disconnected'); }), 5000);}
