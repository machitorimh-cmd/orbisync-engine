# OrbiSync Admin Web

OrbiSync の実 REST API に接続する運用管理画面です。デモ用のローカル配列や擬似応答は使いません。

## 機能

- liveness / readiness / version の確認
- 管理者ログイン、refresh token によるセッション更新、ログアウト
- ユーザーの作成、表示名編集、有効化・無効化、パスワード再発行、ロール割り当て
- ロールの作成、編集、削除、一覧
- World の作成、一覧、archive
- Instance の作成、起動・停止、参加者一覧、kick
- action 完全一致による監査ログの絞り込み
- API エラーコード、request ID、ローカル操作履歴の表示

API 契約の正本は [`../../openapi/orbisync-v1.yaml`](../../openapi/orbisync-v1.yaml) です。画面に表示する機能もサーバー側の RBAC で検査されるため、ログインしたユーザーに権限がなければ API が拒否します。

## 起動

先に PostgreSQL、migration、OrbiSync server、最初の管理者作成を完了してください。リポジトリ直下の手順は [`../../README.md`](../../README.md) にあります。

サーバーはブラウザの origin を明示的に許可する必要があります。Vite の既定 URL を使うローカル開発例:

```shell
export ORBISYNC_CORS_ALLOWED_ORIGINS=http://localhost:5174,http://127.0.0.1:5174
cargo run -p orbisync-server
```

Compose 開発環境では `deploy/compose/.env.example` と `compose.dev.yml` に同じローカル origin が設定されています。

別の端末で管理画面を起動します。

```shell
npm --prefix apps/admin-console-web ci
npm --prefix apps/admin-console-web run dev
# http://localhost:5174
```

画面上部で API URL（既定 `http://127.0.0.1:8080`）を確認し、`bootstrap-admin` が発行した資格情報でログインします。

## セキュリティ上の扱い

- access token と refresh token は JavaScript のメモリ内だけに保持し、`localStorage` / `sessionStorage` へ保存しません。
- 保存するのは API のベース URL だけです。ログアウトまたはページ再読み込みで認証状態は消えます。
- 一時パスワードは API が一度だけ返します。コピー後は画面から消し、安全な経路で利用者へ渡してください。
- 本番では管理画面と API を HTTPS で提供し、`cors.allowed_origins` は実際の管理画面 origin だけを列挙してください。`*` は設定検証で拒否されます。
- 管理画面は RBAC を迂回しません。必要最小限の権限を持つ運用ロールを利用してください。

## 個別検証

```shell
npm --prefix apps/admin-console-web run check
npm --prefix apps/admin-console-web run build
```

ブラウザ回帰テストは、実 Chromium 上で画面を操作し、決定的なモック REST API に対する認証、冪等性キー、revision 照合、主要な管理操作を検査します。初回だけ Playwright の Chromium を導入してください。

```shell
cd apps/admin-console-web
npx playwright install chromium
npm run test:browser
```

このブラウザテストだけでは実 DB・実サーバーとの結合確認を置き換えませんが、日常的な UI 回帰確認には DB を必要としません。

この app は Rust workspace の crate ではありません。
