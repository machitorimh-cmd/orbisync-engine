# Coreの状態を使う外部ロックルール

`org.example.document` のJSON componentの `locked` を判定します。ロック正本はCoreのみ。外部DB、予約、finalize APIはありません。Entity所有者だけがロック・解除でき、ロック中は削除と他のcomponent更新も拒否します。複数Entityの一括操作や外部DBとの原子的確定の例ではありません。

## 接続

1. Coreに信頼されるTLS証明書を用意。ローカルCAの場合は`extensions.pre_commit_additional_ca_path`にCAファイルを設定し、`extensions.allow_loopback_endpoints=true`。
2. `HOOK_CERT`、`HOOK_KEY`へ証明書/鍵のパス、`ORBI_EXTENSION_SECRET_CORE_STATE_LOCK`へCoreと同じ署名secretを環境変数で指定。
3. `node examples/precommit-core-state-lock/server.mjs`（既定8844）。
4. Activeな拡張のendpointを`https://localhost:8844/precommit`、signing_secret_refを`ORBI_EXTENSION_SECRET_CORE_STATE_LOCK`、capabilitiesを`hooks:entity:spawn/update/delete`の3つに設定する。各capabilityの同時登録は1件まで。既存白板用登録と併用しない。
5. 通常の公開EntityCommandで未ロックEntityをSpawnし、component_key=`org.example.document`のUpdateに`locked:true/false`と本文等を渡す。Coreの確定返信revisionを次の要求に使う。

署名済み要求の`current_entity.components[component_key].value`が現在状態、`payload`がクライアント要求です。クライアントがpayload内にcurrent_entityを偽造しても正本にはしません。`current_entity=null`は対象不在。旧Coreが状態フィールドを送らない場合は拒否します。

判定中の対象変更はCore確定時のrevision検証で拒否します。外部ルールは副作用を持たず、拒否や再送でも外部状態の回収は不要です。continuous Transformの検証ではありません。

検証: `node --test examples/precommit-core-state-lock/rule.test.mjs`。Core側の実WebSocketテストは`authoritative_core_state_enforces_lock_and_owner_unlock_without_external_store`。
