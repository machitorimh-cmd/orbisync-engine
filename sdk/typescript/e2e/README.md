# 実サーバーでの機能確認

`live-runtime.ts` は、実際に起動した `orbisync-server` と PostgreSQL に対して、SDKから認証・管理API・protobuf WebSocket・保存後の再接続を確認する手動実行用テストです。最大3接続、数個のエンティティで実行します。

## 準備

1. 専用の破棄可能なDBでマイグレーションと `bootstrap-admin --login-id admin` を実行します。通常利用のDBは指定しないでください。このテストはユーザー・ロール・ワールドを作成します。
2. `bootstrap-admin --password-output` の出力を、リポジトリ外の専用ディレクトリに `admin-password.txt` として保存します。
3. 必須の署名鍵・HMAC鍵・10,000件のパスワード拒否リストを設定した実サーバーを起動し、`/health/ready` が200になるのを待ちます。stub ticketは使いません。
4. SDKの依存関係と生成済みprotobufコードを用意します。

SDKディレクトリで、PowerShellの場合:

```powershell
$env:ORBISYNC_LIVE_TEST_DIR = '<リポジトリ外の専用ディレクトリ>'
$env:ORBISYNC_LIVE_TEST_URL = 'http://127.0.0.1:18087'
npm run test:live -- exercise
```

成功時に検証対象のIDとrevisionを `live-state.json` に保存します。パスワード・トークンはこのファイルに含めません。再実行時は新しいテストデータを作成します。

## 終了と復元

```powershell
npm run test:live -- shutdown
```

`shutdown-ready` が作成されたら、30秒以内にサーバーへ通常の終了シグナルを送ります。テストは更新の受付と `SERVER_SHUTTING_DOWN` の受信を確認し、接続を閉じます。Windowsの `Stop-Process` は強制終了なので、正常終了確認にはサーバーのコンソールのCtrl+Cなどを使います。

サーバーログで `runtime.persistence_drained`、`checkpoint.saved`、`database.pool_closed`、`server.stopped` を確認し、同じDB・設定・鍵でサーバーを再起動します。

```powershell
npm run test:live -- restore
```

復元されたエンティティの位置とrevision、削除の維持、Snapshot内のコンポーネント値（shutdownで書き込んだ44）、復元後のコンポーネント更新を確認します。必ず `exercise` → `shutdown` → 再起動 → `restore` の順で実行してください。DB側のコンポーネント内容は別途照合します。終了後は専用サーバー・DBを片付け、テスト用の鍵と管理者パスワードを削除します。

`exercise` では `custom.chat.message` の同室配信・送信者情報・別室への非配信、再入室Snapshotのカスタムコンポーネント、切断中のメッセージのresume再送と `snapshotApplied` まで確認します。

通常の `npm test` にこのテストは含めません。`npm run check` はこのテストも型検査します。
