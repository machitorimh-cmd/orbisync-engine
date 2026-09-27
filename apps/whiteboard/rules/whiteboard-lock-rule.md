# 付箋ロック外部ルール

このルールはADR-025のpre-commit validation hookを利用する。Coreは、Active registrationに次のcapabilityがある場合、`SpawnEntity` / `UpdateEntityComponent` / `DeleteEntity`の確定前に、署名付きHTTPS POSTを外部ルールへ送り、`{"decision":"allow"}`または`{"decision":"deny","reason":"..."}`を待つ。timeout・非2xx・不正応答はdenyになる。

実装は[whiteboard-lock-rule.mjs](whiteboard-lock-rule.mjs)（判定のみ、副作用なし）と[whiteboard-lock-hook.mjs](whiteboard-lock-hook.mjs)（署名検証とHTTPS受付）に分かれる。

## 付箋データ

- component key: `com.orbisync.whiteboard.note`
- payload: `{ component_key, kind, visibility, text, color, locked, position_x, position_y }`
- entity kind: `object`

CoreはEntityCommandのargument structをそのままcomponent payloadとして保存し、更新のたびに全体を置き換える。したがって更新コマンドは常に付箋全体を送らなければならない。`locked`を省いた部分更新はロックを黙って外すことになるため、UIは[whiteboard-state.mjs](whiteboard-state.mjs)の`notePayload()`だけを経由して組み立てる。

## Coreが渡す現在状態

pre-commit要求の`payload`は次を含む: `request_id`, `instance_id`, `entity_id`, `operation`, `requester`, `client_expected_revision`, `current_entity`, `component_key`, `payload`。

`current_entity`がCoreの読み取り値で、ロックの正本はここだけにある。

```json
{
  "entity_id": "...",
  "revision": 7,
  "owner_id": "...",
  "components": { "com.orbisync.whiteboard.note": { "encoding": "json", "value": { "locked": true, "text": "..." } } }
}
```

- `spawn`では対象が存在しないため`current_entity`は`null`。
- `owner_id`はCoreが保持するEntity所有者で、`requester`（送信者）とは別物。
- `components`は予約された`core.*`を除いたvalidated custom componentで、JSONとして解釈できないpayloadは`encoding=base64`になる。
- `client_expected_revision`はクライアント主張値。Coreはhook呼び出し前に自身の読み取りとの不一致を`REVISION_MISMATCH`で弾くが、ルール側でも`current_entity.revision`と一致しなければdenyする。

外部の永続lock store、予約、finalize API、TTLはこのルールには存在しない。クライアントが`payload`の中に`current_entity`を偽造しても、判定は`request.current_entity`だけを見る。

## 判定

1. `current_entity`フィールドが無い要求はdeny（状態を送らないCoreに対しては強制できない）。
2. `spawn`: `current_entity === null` かつ要求`locked`が`true`でないときだけallow。
3. `spawn`以外: `current_entity`が存在し、`revision`が`client_expected_revision`と一致しなければdeny。
4. 付箋componentが`encoding=json`でなければdeny。componentが未作成なら未ロック扱い。
5. `locked === true`のとき、allowするのは所有者本人による`component_key`が付箋componentのupdateで、要求`locked`が`false`のものだけ。他者の解除、本文編集、削除、他componentの更新はすべてdeny。
6. `locked !== true`のとき、他者の編集・移動・削除は通す。`locked:true`へ変える更新は所有者だけがallow。

つまりロックは「所有者だけが編集できる状態」ではなく「所有者が解除するまで凍結する状態」である。ロック中の本文編集は`locked:true`を伴うので所有者でもdenyされる。

## 既知の限界

- 解除と編集は1コマンドで届く。所有者は解除と同時に本文・色・位置を変更でき、ルールはそれを分離しない。分離するには`current_entity`とのfield単位比較が必要で、この実験では行わない。
- ロックはEntity所有者に固定される。所有権移譲やロック権の委譲は扱わない。
- 未ロック時の他者編集はCore側の権限に依存する。Coreはhook呼び出し前に`entity_update_any`、または`entity_update_own`かつ所有者一致を要求するため、他者編集を成立させるにはdemo worldが`entity_update_any`を許可している必要がある。許可が無い場合はルールに届く前に`NOT_OWNER`で拒否され、所有者限定ロックの検証もできない。
- 複数Entityの一括操作や、continuous Transformの検証ではない。
- hook allowとCore確定は別段階だが、外部状態を持たないため回収すべき予約は無い。判定中に対象が変わった場合はCore側のrevision検証で拒否される。

## 旧実装

外部JSON lock storeを正本にしていた[lock-service.mjs](lock-service.mjs)とそのテストはデモから切り離した。ファイルは記録として残してあるが、start scriptもUIも参照しない。旧実装と新hookはどちらも8843を使うため、同時には起動できない。

## 検証

`node --test apps/whiteboard/rules/whiteboard-lock-rule.test.mjs`。Core側の状態契約の実WebSocketテストは`authoritative_core_state_enforces_lock_and_owner_unlock_without_external_store`（`tests/integration/tests/precommit_hook_2_2.rs`）。

UIはロック状態を表示するだけで、編集・削除・ドラッグをローカルで止めない。したがってロック中の操作は実際にCoreへ送られ、拒否は`PRE_COMMIT_DENIED`として画面ログに出る。UIの無効化に依存しない証拠が必要な場合は、別クライアントから同じEntityへ直接EntityCommandを送る。
