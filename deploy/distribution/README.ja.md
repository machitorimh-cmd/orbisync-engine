# OrbiSync 実行ファイル配布

**日本語** | [English](README.en.md)

ダウンロード・展開して起動すると、同梱サーバーとブラウザ管理画面を試せるパッケージです。PostgreSQL 17.11 の実行環境も含まれます。ローカル試用には Rust、Node.js、Docker、PostgreSQL の別途インストールや DB 接続URLの入力は不要です。開発中のプレビュー版です。

初めて使う場合は[クイックスタート](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/docs/guides/quickstart.ja.md)から順に進めてください。このガイドは対応環境、保存先、手動設定などの詳細を説明します。

## Windows x64

ZIPを一般ユーザーが書き込める専用フォルダへ展開し、`start-web-admin.cmd` を開きます。管理者権限で実行する必要はありません。端末から起動する場合:

```powershell
.\orbisync-server.exe web-admin
```

ブラウザが開かない場合は、端末に表示されたプライベートURLを開いてください。Windows x64 用で、CI の配布テストは Windows Server 2022 を対象にしています。C/C++ ランタイムを静的リンクした配布物を使用するため、Visual C++ Redistributable の追加インストールは不要です。署名付きインストーラーではないため、ダウンロード元とSHA-256を確認してください。

## Linux x86_64

Ubuntu 22.04 のビルドを基準とする glibc 2.35 以降の x86_64 Linux 用です。Alpineのmusl環境やARM64向けではありません。通常の OS ライブラリ（`libgcc_s.so.1` 等）を使用し、外部 HTTPS 接続にはシステムの CA 証明書を使用します。DB は root では起動できないため、一般ユーザーで実行してください。確認済み環境はリリースノートも参照してください。

アーカイブを一般ユーザーが所有する書込み可能な専用フォルダへ展開します。

```sh
tar -xzf orbisync-v0.1.0-preview.3-linux-x86_64.tar.gz
cd orbisync-v0.1.0-preview.3-linux-x86_64
./start-web-admin.sh
```

画面なし環境では:

```sh
./start-web-admin.sh --no-browser
```

管理画面はサーバーのloopbackに限定されます。別端末から利用する場合は、例えば `ssh -L 8090:127.0.0.1:8090 USER@HOST` で接続し、プライベートURLをローカルブラウザで開いてください。URLのホスト・ポート・トークンを保持してください。ログをリダイレクトした場合、URLは `.orbisync-admin/launch-url.txt` に保存されます。このファイルやURLを共有しないでください。

## 初回設定

1. 管理画面の **このPCで試す** を押します。同梱 DB の準備、migration、管理者作成、エンジンの起動を順に実行します。
2. 準備完了後は自動で管理画面へログインし、HTTP / WebSocket / 参加 / 同期の接続テストを表示します。一度だけ表示される管理者パスワードを安全に保存してください。
3. 停止・起動・再起動は画面のボタンから操作できます。同じインストールフォルダを開くと DB と設定を再利用し、初期化し直しません。
4. 問題が出た場合は診断の案内に従います。ポート変更は画面から明示的に選択でき、無関係なプロセスを停止する必要はありません。

ローカル試用では一時的な開発用パスワードポリシーを使用します。実利用者向けの運用では、手動設定から運用者が選定した10,000件の denylist 等を設定してください。外部 PostgreSQL を使用する従来の手動設定も残っています。手動初期化は migration・管理者作成を行うため、既存の大切な DB を指定しないでください。

ヘッダーで日本語 / Englishを切り替えられます。管理画面は8090を優先し、使用中なら空きポートを選びます。ローカル試用のDBとエンジンも空きポートを選び、選んだ設定を保存します。次回起動時に競合した場合は、診断画面のポート変更を使ってください。

ランチャーはパッケージのフォルダを作業ディレクトリにします。設定・秘密情報・ローカル DB は既定で `.orbisync-admin/` に保存され、DB 本体はその中の `local-postgres/data/` に置かれます。再起動には同じフォルダで同じランチャーを使います。別の保存先には `--data-dir` を指定できます。終了は端末のCtrl+Cです。バックアップはDBとこの設定ディレクトリを安全に保管してください。使用中の DB ファイルをコピーするだけでは整合したバックアップになりません。認証情報入りディレクトリを再配布ZIPやGitへ追加しないでください。

このローカルWebランチャーは公開ネットワーク用のサービス構成を自動作成しません。通常の `serve`、既存環境、復旧、TLSなどは[Web管理ガイド](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/apps/admin-web/README.md)を参照してください。

## アプリからゲストで参加する

管理画面でWorldとInstanceを作成してInstanceを起動し、**設定 → ゲスト参加**でゲスト参加を有効にして対象Worldを選択します。保存後にエンジンを再起動してください。必要なGuestロールは初回に準備されます。

アプリには管理画面で確認した**エンジンの接続先URLとInstance ID**を設定します。管理画面のURLやWorld IDとは別の値です。設定変更前から接続していたアプリは退出して再度Joinし、新しいゲスト認証を受けてください。許可していないWorldへの入室は拒否されます。

## 内容と確認

- `orbisync-server[.exe]`: 管理UIとDB migrationを含むサーバー
- `start-web-admin.cmd` または `start-web-admin.sh`: 起動用スクリプト
- `BUILD.json`: ソースcommit、target、Rustバージョン、バイナリSHA-256
- `runtime/postgres/`: PostgreSQL の実行ファイル・標準モジュール・初期化データ・タイムゾーンデータ
- `runtime/postgres/BUILD.json` / `SHA256SUMS`: 固定したソースURL・SHA-256、ビルド条件、依存 DLL/ライブラリ確認、全 runtime ファイルのチェックサム
- `runtime/postgres/LICENSE` / `source/`: PostgreSQL ライセンスと、関連する著作権・ライセンス通知を含む検証済み公式ソース一式
- `SHA256SUMS`: 展開後の同梱ファイルのチェックサム
- `LICENSE-MIT` / `LICENSE-APACHE`: OrbiSyncのライセンス
- `THIRD-PARTY.json` / `third-party-licenses/`: 依存crateのライセンスと通知

Releasesの `SHA256SUMS.txt` と、ダウンロードしたアーカイブのSHA-256を比較してください。Windowsは `Get-FileHash <ZIP名> -Algorithm SHA256`、Linuxは `sha256sum <アーカイブ名>` で計算できます。

実施した確認と未実施項目はリリースノートに記載します。プレビュー版の公開は、本番運用、DB移行・復旧、長時間稼働、接続数の保証を意味しません。
