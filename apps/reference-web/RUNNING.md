# デモの起動方法

## 一人用を遊ぶ

Node.js 22以上を用意し、リポジトリのルートから実行します。

```powershell
npm ci --prefix apps/reference-web
npm run dev --prefix apps/reference-web -- --strictPort
```

http://127.0.0.1:5173/ を開き、「冒険をはじめる」を選択します。一人用にサーバーやアカウントは不要です。起動前にローカルのBufとprotoc-gen-esがSDKのTypeScriptコードを生成します。生成物はGit管理外です。

## オンラインで接続する

1. [ルートREADMEのローカル開発](../../README.md#ローカル開発)に従ってPostgreSQL、サーバー設定、署名鍵、開発用denylist、migrationを準備します。
2. [管理画面の起動手順](../admin-web/README.md)で管理者・参加ユーザーを用意します。
3. [REST API](../../openapi/orbisync-v1.yaml)または管理画面でWorldとInstanceを作成・開始し、ユーザーに必要な参加・Entity操作権限を与えます。
4. サーバーのCORS設定にフロントエンドのオリジンを登録します。

```toml
[cors]
allowed_origins = ["http://127.0.0.1:5173", "http://localhost:5173"]
```

サーバーの`/health/ready`が200になることを確認してから、デモの「友だちと同じ島へ」で自分の環境のサーバーURL・ログインID・パスワード・Instance IDを入力します。アカウントや既存ルーム、秘密値を含む環境ファイルは配布していません。

２人で確認する場合は、異なるアカウントで２つのブラウザウィンドウから同じInstanceへ参加します。星の収集と灯台の点灯は各プレイヤーのローカル状態です。デモ専用の空のルームを利用してください。再接続にはページの再読み込みが必要です。

## 終了

ブラウザを閉じ、デモとサーバーを実行中の各ターミナルで`Ctrl+C`を押して終了を待ちます。DBの停止方法は、自分の環境で使ったComposeや運用手順に従ってください。

## ビルドとテスト

リポジトリのルートから実行します。

```powershell
npm run build --prefix apps/reference-web
npm exec --prefix apps/reference-web -- playwright install chromium
npm test --prefix apps/reference-web
```

通常のブラウザテストは模擬サーバーを使用します。実サーバーとの接続確認は別途必要です。このコピー作成時には上記コマンドやサービス起動を実行していません。
