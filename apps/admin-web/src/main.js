import { createLocalConnection } from '/admin-connection.js';

// UI-owned copy only. Legacy manual diagnostics, names, IDs, permissions and JSON stay raw.
const japanese = {
  'LOCAL START': 'このPCで起動',
  'Try on this PC': 'このPCで試す',
  'Continue local trial': 'ローカル試用を再開',
  'Prepare a private local PostgreSQL database, start the engine, and check a real connection. No SDK build or protocol generation is needed.': '専用のローカル PostgreSQL を準備し、エンジンを起動して実際の接続を確認します。SDK のビルドや通信コードの生成は不要です。',
  'The complete server distribution includes PostgreSQL. Trial data is kept when stopped. This mode is for local testing.': '完全なサーバー配布物には PostgreSQL が含まれています。停止しても試用データは残ります。このモードはローカルでの試用向けです。',
  'Not started': '未起動', 'Ready': '準備完了', 'Stopped': '停止済み',
  'Waiting': '待機中', 'Passed': '成功', 'Failed': '失敗', 'Not reached': '未実行', 'Checking…': '確認中…', 'Not tested': '未確認',
  'Database': 'データベース', 'Setup': '初期設定', 'Engine': 'エンジン', 'Connection test': '接続テスト',
  'Local startup progress': 'ローカル起動の進み具合',
  'Retry local startup': 'ローカル起動を再試行',
  'Use my own database': '自分のデータベースを使う',
  'Use your own PostgreSQL database': 'PostgreSQL を手動で設定する',
  'Stop engine and local database': 'エンジンとローカルDBを停止',
  'Stop engine': 'エンジンを停止', 'Restart engine': 'エンジンを再起動',
  'CONNECTION CHECK': '接続確認', 'Test your connection': '接続を試す',
  'Test connection': '接続テスト', 'Check setup': '構成を診断',
  'This check uses real authentication, a WebSocket connection, room entry, and initial state synchronization. It does not publish credentials or create a game project.': '実際の認証、WebSocket 接続、入室、初期状態の同期を確認します。認証情報の公開やゲームプロジェクトの作成は行いません。',
  'Start the engine to test a connection.': 'エンジンを起動すると接続をテストできます。',
  'The full connection test is available in local trial. Existing databases are not seeded.': '接続テストはローカル試用環境で利用できます。既存のデータベースにテスト用データは作成しません。',
  'Sign in before testing a connection.': '接続テストの前にログインしてください。',
  'Ready to test a real connection.': '実際の接続をテストできます。',
  'Connection check results': '接続テストの結果',
  'HTTP authentication': 'HTTP 認証', 'WebSocket': 'WebSocket', 'Room entry': '入室', 'Initial synchronization': '初期同期',
  'Retry connection test': '接続テストを再試行', 'Check again': 'もう一度確認',
  'Choose an available engine port': '空いているエンジンポートを選ぶ',
  'Choose another database port': '別のデータベースポートを選ぶ',
  'Preparing the local database…': 'ローカルDBを準備中…', 'Starting the database…': 'データベースを起動中…',
  'Applying initial setup…': '初期設定を適用中…', 'Starting the engine…': 'エンジンを起動中…',
  'Stopping safely…': '安全に停止中…', 'Restarting…': '再起動中…', 'Testing the connection…': '接続テスト中…',
  'Startup needs attention': '起動に必要な確認があります',
  'The bundled database runtime is missing. Extract the complete server distribution, then check again, or use your own database.': '同梱のデータベース実行環境が見つかりません。完全なサーバー配布物を展開して再確認するか、自分のデータベースを使用してください。',
  'Use the database runtime supplied with this installation. Existing database files were preserved.': 'この環境に対応する同梱データベース実行環境を使用してください。既存のデータは保持されています。',
  'The private installation folder cannot be used. Check its permissions, then try again.': '専用の保存フォルダーを利用できません。アクセス権を確認し、もう一度試してください。',
  'Another launcher is using this installation. Close that launcher, then try again.': '別のランチャーがこの環境を使用中です。そちらを終了してから、もう一度試してください。',
  'Database files need operator recovery. They were preserved; do not initialize over them.': 'データベースには管理者による復旧が必要です。データは保持されています。既存ファイルの上に初期化しないでください。',
  'The database identity could not be verified. No unrelated process was stopped. Check the installation before retrying.': 'データベースの識別情報を確認できません。無関係なプロセスは停止していません。環境を確認してから再試行してください。',
  'The database port is in use. Choose another database port; other applications will not be stopped.': 'データベースのポートが使用中です。別のポートを選んでください。他のアプリは停止しません。',
  'The database could not be initialized. Check the bundled runtime and folder permissions, then retry.': 'データベースを初期化できません。同梱の実行環境とフォルダーのアクセス権を確認して、再試行してください。',
  'The database could not start. Check the bundled runtime and folder permissions, then retry.': 'データベースを起動できません。同梱の実行環境とフォルダーのアクセス権を確認して、再試行してください。',
  'The database did not become ready. Check local resources, then retry.': 'データベースの準備が時間内に完了しませんでした。PC の空きリソースを確認して、再試行してください。',
  'Local database preparation failed. Its files were kept. Check setup, then retry.': 'ローカルDBの準備に失敗しました。ファイルは保持されています。構成を診断してから再試行してください。',
  'The database did not stop cleanly. Check setup before trying again.': 'データベースを正常に停止できませんでした。構成を診断してから、もう一度試してください。',
  'The database is unavailable. Retry local startup or check your database settings.': 'データベースに接続できません。ローカル起動を再試行するか、接続設定を確認してください。',
  'The engine port is in use. Choose an available port; other applications will not be stopped.': 'エンジンのポートが使用中です。空いているポートを選んでください。他のアプリは停止しません。',
  'Check the saved settings, then retry startup.': '保存済みの設定を確認し、起動を再試行してください。',
  'The database needs attention. Check setup before retrying.': 'データベースの確認が必要です。構成を診断してから再試行してください。',
  'The password corpus is missing. Check the saved password policy before retrying.': 'パスワードコーパスが見つかりません。保存されたパスワードポリシーを確認してから再試行してください。',
  'The password corpus is invalid. Check the saved password policy before retrying.': 'パスワードコーパスが不正です。保存されたパスワードポリシーを確認してから再試行してください。',
  'The saved signing key could not be loaded. Check the private installation before retrying.': '保存された署名鍵を読み込めません。専用の保存環境を確認してから再試行してください。',
  'Complete initialization before starting the engine.': 'エンジンを起動する前に初期設定を完了してください。',
  'The engine could not start. Check setup, then retry startup.': 'エンジンを起動できませんでした。構成を診断して、起動を再試行してください。',
  'The engine did not become ready. Check setup, then retry startup.': 'エンジンの準備が時間内に完了しませんでした。構成を診断して、起動を再試行してください。',
  'The engine is still stopping. Check its status before trying again.': 'エンジンの停止が続いています。状態を確認してから、もう一度試してください。',
  'The local trial credentials changed. Sign in with the current administrator password.': 'ローカル試用環境の認証情報が変更されています。現在の管理者パスワードでログインしてください。',
  'The local trial credentials are unavailable. Sign in with your saved administrator password.': 'ローカル試用環境の認証情報を利用できません。保存した管理者パスワードでログインしてください。',
  'Another local operation is running. Wait for it to finish, then check status.': '別のローカル操作を実行中です。完了を待ってから状態を確認してください。',
  'Local trial uses its dedicated database. Use the database port repair action to change its port.': 'ローカル試用環境では専用のデータベースを使用します。ポートの変更にはデータベースポートの復旧操作を使用してください。',
  'Open the private launcher link and sign in with an administrator account.': '専用の起動リンクを開き、管理者アカウントでログインしてください。',
  'The operation could not finish. Check setup, then retry.': '処理を完了できませんでした。構成を診断してから再試行してください。',
  'Connection confirmed: HTTP, WebSocket, room entry, and initial synchronization passed.': '接続できました。HTTP、WebSocket、入室、初期同期はすべて成功です。',
  'The connection test did not complete. Check setup, then try again.': '接続テストを完了できませんでした。構成を診断して、もう一度試してください。',
  'Setup checks passed.': '構成の診断は成功しました。',
  'Setup checks did not complete. Check again.': '構成の診断が完了していません。もう一度確認してください。',
  'Initial administrator password': '初期管理者パスワード',
  'Save it securely. This local administrator password is shown only once.': '安全に保存してください。このローカル管理者パスワードは一度だけ表示します。',
  'Signed in to your local trial.': 'ローカル試用環境にログインしました。',
  'Saved changes are waiting. Click Restart engine to apply them.': '変更を保存しました。「エンジンを再起動」を押すと適用されます。',
  'Saved. Restart the engine to apply changes.': '保存しました。エンジンを再起動すると変更が適用されます。',
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
  'Guest participation': 'ゲスト参加',
  'Allow guest participation': 'ゲスト参加を許可する',
  'Guests can join only the Worlds selected below. A join-only Guest role is prepared automatically when needed.': 'ゲストは選択したWorldだけに参加できます。初回は参加用のGuestロールを自動で準備します。',
  'After saving, restart the engine. In your app, leave and Join again to use the new guest settings.': '保存後にエンジンを再起動してください。アプリで退出して再度Joinすると、新しいゲスト設定が反映されます。',
  'Load more Worlds': '追加のWorldを読み込む',
  'Create an active World before enabling guest participation.': 'ゲスト参加を有効にする前に、参加先のWorldを作成してください。',
  'Select at least one World for guest participation.': 'ゲストが参加できるWorldを1つ以上選択してください。',
  'Saved. Restart the engine, then leave and Join again in your app to apply guest settings.': '保存しました。エンジンを再起動し、アプリで退出して再度Joinするとゲスト設定が反映されます。',
  'Signed-in account. Disable it from another administrator account.': '現在ログイン中のアカウントです。無効化は別の管理者でログインして行ってください。',
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
  'Operation log': '操作ログ',
  'Time': '時刻', 'Request': 'リクエスト', 'Result': '結果', 'Request ID': 'リクエストID',
  'No operations yet.': '操作はまだありません。',
  'Your administrator sign-in is no longer valid. Sign in again.': '管理者認証が無効になりました。再ログインしてください。',
  'Keeping you signed in automatically.': 'ログインは自動で継続します。',
  'Maintaining your sign-in…': 'ログインを継続中…',
  'Reconnecting. Your sign-in and unsaved input are kept; operations resume when the engine is reachable.': '接続を回復しています。ログインと入力内容は保持しています。エンジンに接続できると操作を再開できます。',
  'Instance created.': 'インスタンスを作成しました。',
  'Instance started.': 'インスタンスを起動しました。',
  'Instance stop requested. Waiting for participants and saved state to drain.': 'インスタンスの停止を要求しました。参加者の退出と保存処理が終わると停止済みになります。',
  'Stopped instances cannot be started again. Create another instance in the same world.': '停止したインスタンスは再起動できません。同じワールドで新しいインスタンスを作成してください。',
  'This page keeps the latest 50 requests without passwords, tokens or request bodies. Durable server results are saved in .orbisync-admin/admin.log; engine results are in engine.log.': 'この画面には直近50件のリクエストを表示します。パスワード・トークン・送信内容は記録しません。サーバーの結果は .orbisync-admin/admin.log、エンジンの結果は engine.log に保存します。',
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
let capability = location.hash.slice(1);
try {
  capability ||= sessionStorage.getItem('orbisync-local') || '';
  if (capability) sessionStorage.setItem('orbisync-local', capability);
} catch { /* The private link remains usable when browser storage is blocked. */ }
history.replaceState(null, '', location.pathname);
let accessToken = null;
let signedInUserId = null;
let refreshToken = null;
let refreshPromise = null;
let sessionEpoch = 0;
let sessionTimer = null;
let refreshAt = Infinity;
let instancePollTimer = null;
let lastStatus = null;
let statusRequestEpoch = 0;
let startupUi = null;
let assignedIds = [];
let guestSettingsLoaded = false, guestReadOnly = true, guestWorlds = [], guestWorldCursor = null, guestWorldEpoch = 0;
let guestAllowedWorlds = new Set();
const pages = {};
const records = { users: [], roles: [], worlds: [], instances: [] };
function notice(message, error = false, raw = true) { $('message').hidden = false; localized($('message'), () => raw ? t('Server response: ') + message : t(message)); $('message').classList.toggle('error', error); }
function errorNotice(error) {
  notice(error.message, true);
  if (error.uiRender) localized($('message'), error.uiRender);
  const context = [error.method, error.path, error.status && `HTTP ${error.status}`, error.code, error.requestId].filter(Boolean).join(' · ');
  if (context) {
    const render = error.uiRender || (() => t('Server response: ') + error.message);
    localized($('message'), () => `${render()}\n${context}`);
  }
  $('message').scrollIntoView({block: 'nearest'});
}
function parseManifest() {
  try { return JSON.parse($('manifest').value); }
  catch (error) { error.uiRender = () => `${t('Invalid JSON manifest.')}\n${error.message}`; throw error; }
}
function uiNotice(message, error = false) { notice(message, error, false); }
function credential(value, initial = false) {
  localized($('credentialTitle'), initial ? 'Initial administrator password' : 'One-time temporary password');
  localized($('credentialHelp'), initial ? 'Save it securely. This local administrator password is shown only once.' : 'Save it securely, then change it after signing in.');
  $('credentialValue').textContent = value; $('credential').hidden = false;
}
function clearCredential() { $('credentialValue').textContent = ''; $('credential').hidden = true; }
// API idempotency keys require UUIDv7, not crypto.randomUUID() (UUIDv4).
function uuid7() {
  const bytes = crypto.getRandomValues(new Uint8Array(16)); let time = BigInt(Date.now());
  for (let i = 5; i >= 0; i--) { bytes[i] = Number(time & 255n); time >>= 8n; }
  bytes[6] = (bytes[6] & 15) | 112; bytes[8] = (bytes[8] & 63) | 128;
  const h = Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('');
  return `${h.slice(0,8)}-${h.slice(8,12)}-${h.slice(12,16)}-${h.slice(16,20)}-${h.slice(20)}`;
}
function recordRequest(path, method, status, data, response, started) {
  if (method === 'GET' && status >= 200 && status < 300) return;
  const row = document.createElement('tr');
  const safe = value => typeof value === 'string' && /^[A-Za-z0-9_-]{1,100}$/.test(value) ? value : '';
  // Route labels only: no query strings, credentials, IDs or response messages.
  const route = path.split('?')[0].replace(/[0-9a-f]{8}-[0-9a-f-]{27,}/gi, '{id}');
  const requestId = safe(response?.headers.get('x-request-id') || data?.error?.request_id)
    || safe(response?.headers.get('x-orbisync-admin-request-id'));
  const code = safe(data?.error?.code || data?.code);
  for (const value of [new Date().toLocaleTimeString(), `${method} ${route}`, `${status || 'NETWORK_ERROR'} ${code} (${Math.round(performance.now() - started)} ms)`, requestId]) {
    const cell = row.insertCell(); cell.textContent = value;
  }
  $('operationRows').prepend(row);
  while ($('operationRows').children.length > 50) $('operationRows').lastChild.remove();
  $('noOperations').hidden = true;
  if (status === 0 || status >= 400) $('operationLog').open = true;
}
function cancelSessionTimer() {
  if (sessionTimer !== null) clearTimeout(sessionTimer);
  sessionTimer = null;
}
function scheduleSessionCheck(delay = Math.max(1000, refreshAt - Date.now())) {
  cancelSessionTimer();
  if (!accessToken || !refreshToken) return;
  sessionTimer = setTimeout(() => {
    sessionTimer = null;
    void maintainSession().catch(error => { if (error.sessionRejected) errorNotice(error); });
  }, Math.min(delay, 2147483647));
}
function planSessionRefresh(expiresIn) {
  // The server supplies expires_in. Leave a margin for network latency and
  // renew even while the operator is watching a background tab.
  const seconds = Number(expiresIn);
  const ttl = Number.isFinite(seconds) && seconds > 0 ? seconds * 1000 : 900000;
  refreshAt = Date.now() + Math.max(1000, ttl - Math.min(60000, ttl / 5));
  localized($('sessionContinuity'), refreshToken ? 'Keeping you signed in automatically.' : '');
  scheduleSessionCheck();
}
async function maintainSession(force = false) {
  if (!accessToken || !refreshToken) return;
  if (!force && Date.now() < refreshAt) { scheduleSessionCheck(); return; }
  const epoch = sessionEpoch;
  localized($('sessionContinuity'), 'Maintaining your sign-in…');
  try { await refreshSession(); }
  catch (error) {
    if (epoch === sessionEpoch) {
      if (error.status === 401 || error.status === 403) {
        clearSession(); error.sessionRejected = true;
        error.uiRender = () => t('Your administrator sign-in is no longer valid. Sign in again.');
      } else {
        // An outage is not evidence that the operator logged out. Preserve
        // credentials and edits; retry without sending a management mutation.
        localized($('sessionContinuity'), 'Reconnecting. Your sign-in and unsaved input are kept; operations resume when the engine is reachable.');
        error.uiRender = () => t('Reconnecting. Your sign-in and unsaved input are kept; operations resume when the engine is reachable.');
        refreshAt = Math.min(refreshAt, Date.now());
        scheduleSessionCheck(5000);
      }
    }
    throw error;
  }
}
function resumeSession() {
  void maintainSession().catch(error => { if (error.sessionRejected) errorNotice(error); });
}
window.addEventListener('focus', resumeSession);
window.addEventListener('online', resumeSession);
window.addEventListener('pageshow', resumeSession);
window.addEventListener('pagehide', cancelSessionTimer);
document.addEventListener('visibilitychange', () => { if (!document.hidden) resumeSession(); });
async function refreshSession() {
  if (refreshPromise) return refreshPromise;
  const epoch = sessionEpoch, token = refreshToken;
  const pending = (async () => {
    const session = await request('/api/v1/auth/refresh', 'POST', {refresh_token: token}, {}, false);
    if (sessionEpoch !== epoch) throw new Error('Session changed');
    if (!session?.access_token || !session?.refresh_token) throw new Error('Invalid refresh response');
    accessToken = session.access_token; refreshToken = session.refresh_token;
    planSessionRefresh(session.expires_in);
  })();
  refreshPromise = pending;
  try { await pending; } finally { if (refreshPromise === pending) refreshPromise = null; }
}
async function request(path, method = 'GET', body, extra = {}, retry = true) {
  const headers = { 'x-orbisync-local': capability, ...extra };
  const pathname = path.split('?')[0];
  // Startup/status operations use only the local capability. In particular,
  // background polling must not send an engine JWT to a reused launcher port.
  // Probe accepts the current signed-in identity, or the backend's local-trial
  // credentials when no token is available.
  const bearerRequired = pathname.startsWith('/api/v1/') || pathname === '/admin/settings' || pathname === '/admin/probe';
  const sessionRequest = !['/api/v1/auth/login', '/api/v1/auth/refresh'].includes(pathname);
  // Timers can be suspended by sleep or browser throttling. Recover before
  // sending an operator's request, rather than letting the first write fail.
  if (retry && bearerRequired && sessionRequest && accessToken && refreshToken && Date.now() >= refreshAt) await maintainSession();
  for (const name of Object.keys(headers)) if (name.toLowerCase() === 'authorization') delete headers[name];
  const sentToken = accessToken;
  const requestEpoch = sessionEpoch;
  if (sentToken && bearerRequired) headers.Authorization = `Bearer ${sentToken}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (method !== 'GET') headers['Idempotency-Key'] = uuid7();
  const started = performance.now();
  let response;
  try { response = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), cache: 'no-store' }); }
  catch (error) { recordRequest(path, method, 0, null, null, started); throw error; }
  const text = await response.text(); let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* Error bodies may be plain text. */ }
  recordRequest(path, method, response.status, data, response, started);
  if (response.status === 401 && sentToken && sessionRequest && retry && refreshToken && sessionEpoch === requestEpoch) {
    if (accessToken === sentToken) await maintainSession(true);
    if (accessToken && sessionEpoch === requestEpoch) return request(path, method, body, extra, false);
  }
  if (!response.ok) {
    const raw = data?.error?.message || data?.message;
    const error = new Error(raw || text);
    error.method = method; error.path = pathname; error.status = response.status;
    error.requestId = data?.error?.request_id || response.headers.get('x-request-id') || response.headers.get('x-orbisync-admin-request-id');
    error.code = data?.error?.code || data?.code || (response.status === 401 ? 'AUTHENTICATION_REQUIRED' : response.status === 403 ? 'FORBIDDEN' : 'REQUEST_FAILED');
    if (response.status === 401 && sentToken && sessionRequest) {
      if (sessionEpoch === requestEpoch) clearSession();
      error.uiRender = () => t('Your administrator sign-in is no longer valid. Sign in again.'); throw error;
    }
    if (raw) throw error;
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
  const epoch = ++statusRequestEpoch;
  const next = await request('/admin/status');
  if (epoch !== statusRequestEpoch) return lastStatus || next;
  lastStatus = next;
  $('setup').hidden = lastStatus.initialized; $('engine').hidden = !lastStatus.initialized;
  $('login').hidden = !lastStatus.initialized || !!accessToken;
  localized($('readiness'), lastStatus.ready ? 'Engine ready' : lastStatus.initialized ? 'Engine not ready' : 'Setup required');
  localized($('engineStatus'), () => `${t('Engine: ')}${t(lastStatus.engine)}${lastStatus.existing ? t(' · Existing installation; configuration is read-only.') : ''}`);
  $('startEngine').hidden = lastStatus.engine !== 'not started';
  $('restartNotice').hidden = !lastStatus.restart_required;
  startupUi?.observe(lastStatus);
  return lastStatus;
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
function clearSession() {
  if (instancePollTimer !== null) clearTimeout(instancePollTimer);
  instancePollTimer = null;
  cancelSessionTimer(); refreshAt = Infinity; localized($('sessionContinuity'), '');
  sessionEpoch += 1; accessToken = null; signedInUserId = null; refreshToken = null; refreshPromise = null; $('session').hidden = true; $('management').hidden = true; $('login').hidden = false;
  $('currentPassword').value = ''; $('newPassword').value = ''; clearCredential();
  for (const name of Object.keys(records)) { records[name] = []; pages[name] = null; $(`${name}Table`).replaceChildren(); }
  $('roleChoices').replaceChildren(); $('assignUser').replaceChildren(); $('manifest').value = ''; $('configFields').replaceChildren();
  guestWorldEpoch++; guestSettingsLoaded = false; guestAllowedWorlds.clear(); guestWorlds = [];
  $('guestEnabled').checked = false; $('guestEnabled').disabled = true; $('guestWorldChoices').replaceChildren(); $('moreGuestWorlds').hidden = true;
  startupUi?.authChanged();
}
async function signedIn(token, trial = false, refresh = null, expiresIn = null) {
  const epoch = ++sessionEpoch; accessToken = token; signedInUserId = null; refreshToken = refresh; $('loginPassword').value = '';
  planSessionRefresh(expiresIn);
  const me = await api('auth/me');
  if (sessionEpoch !== epoch) return;
  signedInUserId = me.id;
  localized($('signedIn'), () => `${t('Signed in as ')}${me.display_name} (${me.login_id})`);
  $('session').hidden = false; $('management').hidden = false; $('login').hidden = true; $('passwordDetails').open = !trial;
  uiNotice(trial ? 'Signed in to your local trial.' : 'Signed in. If this is a temporary password, change it now.');
  try { await loadUsers(); await loadRoles(); } catch (error) { if (trial) throw error; errorNotice(error); }
  startupUi?.authChanged();
}
action('loginForm', async () => {
  const result = await api('auth/login', 'POST', {login_id: $('loginId').value, password: $('loginPassword').value});
  await signedIn(result.access_token, false, result.refresh_token, result.expires_in);
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
  table('usersTable', ['Login / ID', 'Name', 'Status', 'Actions'], users.map(u => {
    const ownAccount = u.id === signedInUserId;
    const specs = [];
    // Keep the general engine API's explicit account-disable behavior. The
    // management UI must not let an operator accidentally revoke its own login.
    if (!ownAccount) specs.push([u.enabled ? 'Disable' : 'Enable', async () => {
      if (u.id === signedInUserId) return;
      await api(`users/${u.id}/${u.enabled ? 'disable' : 'enable'}`, 'POST'); await loadUsers();
    }]);
    specs.push(['Reset password', async () => {const result = await api(`users/${u.id}/reset-password`, 'POST'); credential(result.temporary_password);}]);
    const actions = buttons(specs);
    if (ownAccount) {
      const help = document.createElement('p'); help.className = 'muted';
      localized(help, 'Signed-in account. Disable it from another administrator account.');
      actions.prepend(help);
    }
    return [`${u.login_id}\n${u.id}`, u.display_name, uiText(u.enabled ? 'Enabled' : 'Disabled'), actions];
  }));
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
async function loadWorlds(more = false) {const worlds = await page('worlds', more); table('worldsTable', ['World / ID', 'Status', 'Actions'], worlds.map(w => [`${w.name}\n${w.id}`, uiText(w.status), w.status === 'active' ? buttons([['Create instance', async () => {await api('instances', 'POST', {world_id: w.id}); await loadInstances(); uiNotice('Instance created.');}]]) : '']));}
async function loadInstances(more = false) {
  if (instancePollTimer !== null) clearTimeout(instancePollTimer);
  instancePollTimer = null;
  const instances = await page('instances', more);
  table('instancesTable', ['Instance', 'World', 'Status', 'Actions'], instances.map(i => {
    const operations = i.status === 'created' ? ['start', 'stop'] : i.status === 'running' ? ['stop'] : [];
    const controls = buttons(operations.map(op => [op === 'start' ? 'Start' : 'Stop', async () => {
      await api(`instances/${i.id}/${op}`, 'POST'); await loadInstances();
      uiNotice(op === 'start' ? 'Instance started.' : 'Instance stop requested. Waiting for participants and saved state to drain.');
    }]));
    if (!operations.length) controls.append(uiText('Stopped instances cannot be started again. Create another instance in the same world.'));
    return [i.id, records.worlds.find(w => w.id === i.world_id)?.name || i.world_id, uiText(i.status), controls];
  }));
  if (accessToken && !$('worlds').hidden && instances.some(i => i.status === 'stopping')) {
    instancePollTimer = setTimeout(() => { instancePollTimer = null; void loadInstances().catch(errorNotice); }, 2000);
  }
}
action('refreshWorlds', async () => {await loadWorlds(); await loadInstances();}); action('moreWorlds', () => loadWorlds(true)); action('moreInstances', () => loadInstances(true));
action('worldForm', async () => {await api('worlds', 'POST', {name: $('worldName').value, capacity: Number($('worldCapacity').value)}); $('worldName').value = ''; await loadWorlds(); uiNotice('World created.');}, 'submit');
const labels = {'server.bind':'Engine address', 'database.max_connections':'Database pool limit', 'database.acquire_timeout_seconds':'Database acquire timeout (seconds)', 'database.readiness_timeout_seconds':'Readiness timeout (seconds)', 'realtime.max_connections':'Realtime connection limit', 'world.default_capacity':'Default world capacity'};
function renderGuestWorlds() {
  $('guestWorldChoices').replaceChildren();
  const available = new Map(guestWorlds.map(world => [world.id, world]));
  // Keep configured choices that are on later pages. Loading or toggling the
  // mode must never silently erase a saved World selection.
  for (const id of guestAllowedWorlds) if (!available.has(id)) available.set(id, {id, name: ''});
  for (const world of available.values()) {
    if (world.status && world.status !== 'active' && !guestAllowedWorlds.has(world.id)) continue;
    const label = document.createElement('label'), check = document.createElement('input');
    check.type = 'checkbox'; check.value = world.id; check.checked = guestAllowedWorlds.has(world.id);
    check.disabled = guestReadOnly || !$('guestEnabled').checked;
    check.addEventListener('change', () => {if (check.checked) guestAllowedWorlds.add(world.id); else guestAllowedWorlds.delete(world.id);});
    label.append(check, ` ${world.name ? world.name + ' — ' : ''}${world.id}`); $('guestWorldChoices').append(label);
  }
  if (!available.size) {const text = document.createElement('p'); localized(text, 'Create an active World before enabling guest participation.'); $('guestWorldChoices').append(text);}
  $('moreGuestWorlds').hidden = !guestWorldCursor;
  $('moreGuestWorlds').disabled = guestReadOnly;
}
async function loadGuestWorlds(more = false) {
  const epoch = guestWorldEpoch, session = sessionEpoch;
  const result = await api(`worlds?limit=50${more && guestWorldCursor ? '&cursor=' + encodeURIComponent(guestWorldCursor) : ''}`);
  if (guestWorldEpoch !== epoch || sessionEpoch !== session) return;
  guestWorlds = more ? [...guestWorlds, ...result.items] : result.items; guestWorldCursor = result.next_cursor;
  renderGuestWorlds();
}
$('guestEnabled').addEventListener('change', renderGuestWorlds);
action('moreGuestWorlds', () => loadGuestWorlds(true));
async function loadSettings() {
  const result = await request('/admin/settings'); $('configFields').replaceChildren();
  for (const [key, value] of Object.entries(result.values)) {const label = document.createElement('label'), input = document.createElement('input'); label.append(uiText(labels[key] || key)); input.value = value; input.dataset.key = key; input.disabled = result.read_only; label.append(input); $('configFields').append(label);}
  $('manifest').value = result.manifest ? JSON.stringify(result.manifest, null, 2) : '';
  $('manifest').placeholder = t('Managed by the existing installation.');
  $('manifest').dataset.languageAttribute = 'placeholder'; $('manifest').dataset.languageSource = 'Managed by the existing installation.';
  for (const id of ['manifest', 'replacementDb', 'saveSettings']) $(id).disabled = result.read_only;
  if (lastStatus?.local_trial) $('replacementDb').disabled = true;
  $('managedDatabaseHint').hidden = !lastStatus?.local_trial;
  guestWorldEpoch++; guestReadOnly = result.read_only; guestSettingsLoaded = !!result.guest;
  $('guestSettings').hidden = !guestSettingsLoaded;
  guestAllowedWorlds = new Set(result.guest?.allowed_worlds || []); guestWorlds = []; guestWorldCursor = null;
  $('guestEnabled').checked = result.guest?.enabled === true; $('guestEnabled').disabled = guestReadOnly;
  renderGuestWorlds();
  if (guestSettingsLoaded) await loadGuestWorlds();
  if (result.read_only) uiNotice('Existing configuration is read-only. Use its original operator workflow for changes.');
}
action('refreshSettings', loadSettings);
action('settingsForm', async () => {
  const values = Object.fromEntries([...$('configFields').querySelectorAll('input')].map(i => [i.dataset.key, i.value]));
  const guest = guestSettingsLoaded ? {enabled: $('guestEnabled').checked, allowed_worlds: [...guestAllowedWorlds]} : undefined;
  if (guest?.enabled && !guest.allowed_worlds.length) {uiNotice('Select at least one World for guest participation.', true); return;}
  await request('/admin/settings', 'POST', {values, database_url: $('replacementDb').value, manifest: parseManifest(), guest});
  $('replacementDb').value = ''; uiNotice(guestSettingsLoaded ? 'Saved. Restart the engine, then leave and Join again in your app to apply guest settings.' : 'Saved. Restart the engine to apply changes.'); await refreshStatus();
}, 'submit');
for (const button of document.querySelectorAll('[data-tab]')) button.addEventListener('click', async () => {
  const name = button.dataset.tab;
  if (name !== 'worlds' && instancePollTimer !== null) { clearTimeout(instancePollTimer); instancePollTimer = null; }
  for (const b of document.querySelectorAll('[data-tab]')) {b.setAttribute('aria-selected', String(b === button)); $(b.dataset.tab).hidden = b !== button;}
  try {if (name === 'worlds') {await loadWorlds(); await loadInstances();} else if (name === 'config') await loadSettings(); else if (name === 'roles') await loadRoles(); else await loadUsers();} catch (error) {errorNotice(error);}
});
const manualSetup = document.createElement('details'); manualSetup.id = 'manualSetup';
const manualSummary = document.createElement('summary'); localized(manualSummary, 'Use your own PostgreSQL database');
const manualFields = document.createElement('div');
while ($('setup').firstChild) manualFields.appendChild($('setup').firstChild);
manualSetup.append(manualSummary, manualFields); $('setup').append(manualSetup);
startupUi = createLocalConnection({ request, refreshStatus, localize: localized,
  canProbe: () => !!accessToken || lastStatus?.local_trial === true,
  onTrial: async result => {
    if (typeof result.initial_password === 'string') credential(result.initial_password, true);
    if (typeof result.login_id === 'string') $('loginId').value = result.login_id;
    if (typeof result.access_token !== 'string' || !result.access_token) throw new Error('Local sign-in failed');
    await signedIn(result.access_token, true, result.refresh_token, result.expires_in);
  },
  showManual: () => { $('setup').hidden = false; manualSetup.open = true; $('databaseUrl').focus(); },
});
initializeLanguage();
if (!capability) uiNotice('Open the private administration URL printed by the web-admin command. This page needs the launch capability in its URL fragment.', true);
else {refreshStatus().catch(error => errorNotice(error)); setInterval(() => refreshStatus().catch(() => { localized($('readiness'), 'Launcher disconnected'); }), 5000);}
