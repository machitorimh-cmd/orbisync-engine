# OrbiSync 実行ファイル配布

**日本語** | [English](README.en.md)

RustやNode.jsをインストールせず、同梱サーバーとブラウザ管理画面を使うためのパッケージです。開発中のプレビュー版です。PostgreSQLは同梱していません。空のPostgreSQLデータベースを別途用意してください。管理画面の初期化はmigration・管理者作成を行うため、既存の大切なDBを指定しないでください。

## Windows x64

ZIPを一般ユーザーが書き込める専用フォルダへ展開し、`start-web-admin.cmd` を開きます。管理者権限で実行する必要はありません。端末から起動する場合:

```powershell
.\orbisync-server.exe web-admin
```

ブラウザが開かない場合は、端末に表示されたプライベートURLを開いてください。署名付きインストーラーではありません。ダウンロード元とSHA-256を確認してください。Microsoft Visual C++ランタイムのDLL不足が表示された場合は、Microsoft公式のVisual C++ v14 x64 Redistributableを導入してください。

## Linux x86_64

glibcを使うLinux用です。Alpineのmusl環境やARM64向けではありません。必要なglibcバージョンと動作確認環境はリリースノートを参照してください。システムのCA証明書と `libgcc_s.so.1` も必要です。

アーカイブを一般ユーザーが所有する書込み可能な専用フォルダへ展開します。

```sh
tar -xzf orbisync-v0.1.0-preview.1-linux-x86_64.tar.gz
cd orbisync-v0.1.0-preview.1-linux-x86_64
./start-web-admin.sh
```

画面なし環境では:

```sh
./start-web-admin.sh --no-browser
```

管理画面はサーバーのloopbackに限定されます。別端末から利用する場合は、例えば `ssh -L 8090:127.0.0.1:8090 USER@HOST` で接続し、プライベートURLをローカルブラウザで開いてください。URLのホスト・ポート・トークンを保持してください。ログをリダイレクトした場合、URLは `.orbisync-admin/launch-url.txt` に保存されます。このファイルやURLを共有しないでください。

## 初回設定

1. 空のPostgreSQL DBの接続URLを入力し、接続を確認します。
2. 実ユーザーが利用する場合は、運用者が選定した10,000件のパスワードdenylistを指定します。一時的なローカル試用だけなら **Local development** を明示選択できます。生成されるプレースホルダーは一般的な実パスワードを防ぎません。
3. 管理者のログインID・表示名を入力し、初期化します。鍵とmigrationが準備されます。一度だけ表示される初期パスワードを安全に保存します。
4. **Start engine** で起動し、準備完了後にログインします。初期パスワードを変更し、再ログインしてください。

ヘッダーで日本語 / Englishを切り替えられます。既定の管理ポートは8090、エンジンは8080です。ポートが使用中の場合は既存プロセスを停止せず、設定を変更してください。

ランチャーはパッケージのフォルダを作業ディレクトリにします。設定と秘密情報は既定で `.orbisync-admin/` に保存されます。再起動には同じフォルダで同じランチャーを使います。別の保存先には `--data-dir` を指定できます。終了は端末のCtrl+Cです。バックアップはDBとこの設定ディレクトリを安全に保管してください。認証情報入りディレクトリを再配布ZIPやGitへ追加しないでください。

このローカルWebランチャーは公開ネットワーク用のサービス構成を自動作成しません。通常の `serve`、既存環境、復旧、TLSなどは[Web管理ガイド](https://github.com/machitorimh-cmd/orbisync-engine/blob/main/apps/admin-web/README.md)を参照してください。

## 内容と確認

- `orbisync-server[.exe]`: 管理UIとDB migrationを含むサーバー
- `start-web-admin.cmd` または `start-web-admin.sh`: 起動用スクリプト
- `BUILD.json`: ソースcommit、target、Rustバージョン、バイナリSHA-256
- `LICENSE-MIT` / `LICENSE-APACHE`: OrbiSyncのライセンス
- `THIRD-PARTY.json` / `third-party-licenses/`: 依存crateのライセンスと通知

Releasesの `SHA256SUMS.txt` と、ダウンロードしたアーカイブのSHA-256を比較してください。Windowsは `Get-FileHash <ZIP名> -Algorithm SHA256`、Linuxは `sha256sum <アーカイブ名>` で計算できます。

実施した確認と未実施項目はリリースノートに記載します。プレビュー版の公開は、本番運用、DB移行・復旧、長時間稼働、接続数の保証を意味しません。
