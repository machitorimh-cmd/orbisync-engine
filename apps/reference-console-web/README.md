# OrbiSync Reference Web

`@orbisync/client` と公開REST APIへ実際に接続する、最小のブラウザ参照アプリです。offline用のfake dataやstub WebSocketはありません。

## 確認できること

1. SDKによるloginと、tokenを外へ公開しない認証済みREST request
2. Worldの一覧・作成
3. Instanceの一覧・作成・起動
4. realtime ticket、Protobuf WebSocket、Instance参加
5. UUIDv7 Entityのspawn・component update・transform・delete・ownership transfer
6. connection phase、再接続回数、RTT、revision、reliable queueの表示
7. transportを意図的に張り直し、SDKの自動resume経路を確認する操作

access token、refresh token、realtime ticket、resume tokenは画面・ログ・Web Storageへ出しません。保存するのはServer URLだけです。

## 起動

先にリポジトリrootでprotobuf生成とサーバー準備を行います。

```bash
buf generate
bash scripts/gen-dev-password-denylist.sh
cargo run -p orbisync-server -- migrate
cargo run -p orbisync-server -- bootstrap-admin \
  --login-id admin --display-name "Administrator" \
  --password-denylist deploy/dev-password-denylist.txt \
  --password-output ./admin-password.txt
```

APIをhostで起動する場合、Viteの2 originを明示的に許可してください。

```bash
export ORBISYNC_CORS_ALLOWED_ORIGINS=http://localhost:5173,http://127.0.0.1:5173
cargo run -p orbisync-server -- --config orbisync.toml.example
```

別terminalで参照アプリを起動します。

```bash
npm --prefix apps/reference-console-web ci
npm --prefix apps/reference-console-web run dev
# http://localhost:5173
```

画面では次の順に操作します。

1. Server URL、Login ID、Passwordを入力し「SDKでログイン」
2. Worldを選ぶか作成
3. Instanceを作成し「起動」
4. 「接続して参加」
5. Entityを作成し、値更新・WASD移動・削除を確認
6. 「自動復帰を検証」で `connected → reconnecting → connected` と `ResumeAccepted` を確認

ownership transferは、同じInstanceへ参加中の別User IDを入力した場合だけ成功します。権限・live membership・revisionはサーバーが検証します。

## 限定検証

```bash
npm --prefix apps/reference-console-web run check
npm --prefix apps/reference-console-web run build
```

このアプリは外部利用者視点の手動smokeにも使えます。CI無料枠を消費する自動scheduleは設定していません。
