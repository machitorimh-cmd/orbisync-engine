# 認証発行制限とセッション管理

F01/F02 の設定・API・失効動作を記載します。認証方式そのものの有効化は既存の `auth.methods` と各方式の設定で行います。

## 初回認証の発行制限

`POST /v1/auth/login`、`/v1/auth/guest`、`/v1/auth/name`、`/v1/auth/external` は共通の制限を通過してから、パスワード・外部署名の検証や DB 保存を実行します。IP は既存の `server.trusted_proxies` に従って解決します。信頼していない接続元からの `X-Forwarded-For` では制限を回避できません。接続元を取得できない要求は一つの共通バケットに入ります。

| `rate_limit` のキー | 既定値 | 意味 |
|---|---:|---|
| `auth_issuance_ip_per_minute` | 60 | IP ごとの四方式合計の試行数 |
| `auth_issuance_local_per_minute` | 10 | IP ごとの local 試行数 |
| `auth_issuance_guest_per_minute` | 30 | IP ごとの guest 試行数 |
| `auth_issuance_name_only_per_minute` | 30 | IP ごとの name-only 試行数 |
| `auth_issuance_external_per_minute` | 30 | IP ごとの external 試行数 |
| `auth_issuance_max_concurrent` | 32 | 四方式合計の処理中要求数 |
| `auth_temporary_subjects_per_minute` | 1000 | guest/name-only 合計の新規主体発行試行予算 |

全値は 1 以上、同時処理数は 65536 以下です。環境変数は例えば `ORBISYNC_RATE_LIMIT_AUTH_ISSUANCE_MAX_CONCURRENT=16`、CLI override は `rate_limit.auth_issuance_max_concurrent=16` です。IP バケット数の上限は既存の `rate_limit.login_ip_max_buckets` を共有します。上限まで未失効バケットがある場合、新しい IP は空きができるまで拒否されます。既存バケットを追い出して試行数を初期化しません。

制限はサーバープロセス単位です。複数 replica の構成では、希望する配置全体の予算を replica 数で割って設定するか、ingress 側でも配置全体の制限を行います。カウンターは永続化されず、プロセス再起動で初期化されます。時間窓は最初の試行から 60 秒で、失敗した認証・発行試行も予算を消費します。既に制限で拒否した要求は DB 保存へ進みません。同時処理枠は検証と保存が完了するまで保持し、要求のキャンセルでも返却します。主体の保持期間中の保存量は、発行予算と既存の retention 設定の両方から見積もってください。

local には従来の `login_per_ip_per_minute` と `login_ip_block_seconds` も適用します。従来の IP 判定を先に行うため、設定したブロック期間を維持します。有効な上限は、従来の local 制限・共通 IP 制限・共通 local 制限のうち最も厳しい値です。既存の local 上限を増やした環境では、新しい二つの上限も合わせて設定してください。refresh と realtime ticket の既存ポリシーは別です。

拒否は標準エラー `RATE_LIMITED`、HTTP 429 と `Retry-After` を返します。時間窓の制限は窓が終わるまでの秒数（切り上げ）、同時処理枠不足は 1 秒が目安です。`rate_limit_rejected_total{scope="ip|auth_method|auth_concurrency|temporary_subject_budget"}` で件数を確認できます。local のハッシュ処理枠不足には既存の `password_hash` scope を使います。IP、user ID、token をメトリクスラベルに含めません。

## 自分のセッション

全 API で有効な OrbiSync access token が必要です。セッションの所有者は token と保存済みセッションから決定し、リクエストから user ID を指定しません。

| Method / path | 結果 |
|---|---|
| `GET /v1/auth/sessions?limit=50&cursor=...` | 一覧と `next_cursor`。limit は既存のページ設定内、最大 200 |
| `DELETE /v1/auth/sessions/{session_id}` | 選んだセッションを失効。204。既に失効した自分のセッションも 204 |
| `POST /v1/auth/sessions/revoke-others` | 要求に使ったセッションを残して失効。200 `{"revoked_count": N}` |
| `PATCH /v1/auth/sessions/{session_id}` | 参考用の端末名を変更。204 |

一覧の一項目は次の形です。token、refresh token、Resume token、その digest、IP アドレスは返しません。`current` は要求に使った access token の session ID との一致です。期限経過は `expired`、明示的失効は `revoked`、それ以外は `active` です。現在のセッションも個別失効でき、以後その認証情報は使用できません。

```json
{
  "session_id": "0192d43d-a18a-7fed-8123-0123456789ab",
  "created_at": "2026-10-24T10:00:00Z",
  "expires_at": "2026-11-23T10:00:00Z",
  "revoked_at": null,
  "status": "active",
  "current": true,
  "device_label": "Office laptop"
}
```

ラベルの body は `{"device_label":"Office laptop"}`、消去は `{"device_label":null}` です。空白だけ・制御文字・128 Unicode 文字超過を拒否します。ラベルは自己申告の表示情報で、端末の証明や権限判定に使いません。ラベル値は監査の詳細へ複写しません。一覧は session ID 昇順の keyset pagination で、cursor は署名し、所有者にも束縛します。他ユーザーの cursor を流用すると 400 です。他人のセッションの個別操作は、存在しない ID と同じ 404 です。

## 管理者による操作とアップグレード

| Method / path | 必要な権限 | 結果 |
|---|---|---|
| `GET /v1/users/{user_id}/sessions` | `admin.sessions.read` | 同じ一覧形式 |
| `DELETE /v1/users/{user_id}/sessions/{session_id}` | `admin.sessions.revoke` | 204 |
| `DELETE /v1/users/{user_id}/sessions` | `admin.sessions.revoke` | 200 `{"revoked_count": N}` |

閲覧権限だけでは強制失効できません。個別 ID は path の user ID にも束縛します。新規 bootstrap の `Administrator` に二つの権限と `admin.auth.keys.reload` を付与します。migration `0026_builtin_admin_capabilities.sql` は、元の名前 `Administrator`、説明 `Initial administration role`、元の管理権限 17 個を全て保持する既存ロールだけを自動更新します。追加があった場合に限り revision を一度進めます。再実行でも重複付与しません。

`World Administrator`、custom role、改名したロール、意図的に制限したロールには自動付与しません。これらは既存のロール管理で必要な権限を明示的に追加してください。表示名だけを根拠に custom role を昇格させません。

## 失効と監査

失効と監査の書き込みは一つの DB transaction で完了します。監査に失敗した場合は失効もロールバックします。一覧・ラベル変更・個別失効・その他の一括失効・管理者の全失効は、それぞれ `session.listed`、`session.label_updated`、`session.revoked`、`session.others_revoked`、`session.all_revoked` です。actor、対象 ID、request ID、管理操作かどうか、失効件数を記録します。送信元 IP は既存の別テーブルと保存期限に従います。

失効済みセッションの signed access token、refresh、realtime ticket は拒否します。接続済み WebSocket は受信時と送信バッチの直前にも保存済み状態を確認し、失効後の world data の配信を止めます。通信のない接続も次の heartbeat tick で閉じます。状態を確認できない場合は閉じる側に倒します。判定を通過した送信と、その直後に並行して commit する失効には通常の境界上の競合があります。

全セッションの失効は、その時点のセッションを止める操作です。有効な資格情報による後続の新しいログインを禁止する場合は、ユーザーの無効化や資格情報の変更も行います。
