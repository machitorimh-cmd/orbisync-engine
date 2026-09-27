# 拡張専用tokenとCommand API

ADR-007のプロセス外拡張向け入口です。ユーザーのJWTやAuthSessionIdは使用しません。
運用者が拡張を登録し、有限の権限を持つservice tokenを発行します。

## 起動と更新

`ORBISYNC_EXTENSION_TOKEN_HMAC_KEY` に、他用途と共有しない32バイト以上のランダムな鍵を設定してください。
設定ファイルやDBには鍵本体を書きません。既存環境にもこの環境変数の追加が必要です。
`orbisync-server migrate` で `0022_extension_service_tokens.sql` を適用してから起動します。
Composeの環境変数と `doctor` の必須secret検査にも反映しています。
このHMAC鍵を変更すると既存の拡張tokenは認証できなくなるため、再発行が必要です。

## 運用者による登録

次のJSONをmanifestファイルに保存します。IDはUUIDv7です。

```json
{
  "extension_id": "019f0000-0000-7000-8000-000000000001",
  "name": "Example extension",
  "description": null,
  "endpoint": "https://extension.example/webhook",
  "subscribed_events": [],
  "capabilities": ["commands:entity:read"],
  "token_scopes": [
    "commands:entity:read",
    "instances:019f0000-0000-7000-8000-000000000002"
  ],
  "status": "active",
  "signing_secret_ref": "EXAMPLE_WEBHOOK_SIGNING_KEY"
}
```

```text
orbisync-server extension-register --manifest extension.json
orbisync-server extension-token --extension-id 019f0000-0000-7000-8000-000000000001 --scope commands:entity:read --scope instances:019f0000-0000-7000-8000-000000000002 --token-output extension.token
```

登録・発行・失効はDBへアクセスできる運用者のCLIで行います。拡張token自身に管理操作を許可するHTTP入口はありません。
`extension-token` は発行とrotationを兼ね、1拡張につき有効tokenは1つです。再発行時には別の新規出力ファイルを指定してください。
DB更新と監査記録を同じtransactionで確定し、旧tokenを無効にします。失敗時は旧tokenを維持します。

- tokenは暗号学的乱数32バイト（256-bit）、`orb_ext_` + 64桁の小文字hex。期限は発行から2,592,000秒です。
- DBへはHMAC-SHA-256 digest・発行scope・発行時刻・期限だけを保存します。
- 生tokenは指定した新規ファイルだけへ出力します。標準出力・ログ・manifest・監査情報に含めません。
- `extension-revoke --extension-id <UUIDv7>` で即時失効できます。
- manifestを `status: suspended` にして再登録すると使用を停止します。再有効化すると期限内の未失効tokenは再び使用可能です。恒久失効にはrevokeを使います。
- manifestのcapability/scopeを縮小すると次の要求から反映されます。拡大しても既発行tokenのscopeは増えません。

## RESTコマンド

`POST /v1/extensions/commands` に `Authorization: Bearer <extension token>` とJSONを送ります。
HTTPの共通request ID、body上限、deadlineを適用します。

| command | 必要capabilityとtoken scope | 引数 | 結果 |
|---|---|---|---|
| `entity.get` | `commands:entity:read` と対象の `instances:<UUIDv7>` scope | `instance_id`, `entity_id` | 稼働中instanceのentity 1件 |
| `audit.get` | `commands:audit:read` | `event_id` | 既存の秘密情報を含まない監査projection 1件 |

```json
{"command":"entity.get","instance_id":"019f0000-0000-7000-8000-000000000002","entity_id":"019f0000-0000-7000-8000-000000000003"}
```

成功応答は `{"result": {...}}` です。entityにはID、kind、owner_user_id、10進文字列のrevision、transform、カスタムcomponentのbyte配列を返します。
instanceの起動や状態変更は行いません。停止中または存在しないentityは404です。
entityの取得はそのinstanceのactorに依頼し、部屋全体のSnapshotをコピーしません。

**instance scopeは、そのinstance内の所有者限定等を含むentityをサービスとして参照する権限です。**
一般プレイヤーの可視範囲とは異なるため、運用者が必要なinstanceだけを明示して付与します。ワイルドカードはありません。
`commands:audit:read` はこの配備全体の監査参照権限です。監査IDは既存の管理APIや運用側から渡します。

認証と認可は、tokenに発行時記録されたscope、現在のmanifestのscope、現在のcapabilityのすべてを確認します。
未認証・期限切れ・停止・失効は401、権限外は403、不在は404、不正JSON・未知コマンド・未知フィールドは400です。
内部障害は詳細を隠した500です。ユーザーJWTをこのAPIに渡しても認証できません。
拡張tokenをユーザー管理APIへ渡しても認証できません。現在のコマンド集合に作成・更新・削除や管理操作はありません。

正本のHTTP契約は [OpenAPI](../../openapi/orbisync-v1.yaml)、実DBでの検証は
`tests/integration/tests/extension_commands.rs`、実main/CLIの検証は `sdk/typescript/e2e/extension-runtime.ts` です。
