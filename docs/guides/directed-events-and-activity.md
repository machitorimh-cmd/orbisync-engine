# 宛先限定イベントと入力中・操作中の状態

同じInstanceに参加しているユーザー間で、宛先を限定したイベントと期限付きの活動状態を共有できます。メッセージ本文、通知の種類、入力中・操作中の意味はアプリが定義します。

## エンジン・SDK・アプリの責務

| 層 | 担当 |
|---|---|
| エンジン | 認証済み送信者の付与、参加資格・宛先の検証、対象だけへの配信、再接続時にも対象を維持、活動状態の所有者・期限・容量管理 |
| TypeScript SDK | 型付き送信・購読、宛先の事前検証、対応機能の確認、活動snapshotの取得・分割組立、切断時の破棄・ローカル期限処理 |
| アプリ | 本文や通知の意味、ユーザー選択、表示名・アイコン、入力や操作の検出、表示方法、必要な場合の独自履歴・未読管理 |

エンジンにチャット欄などの画面は追加しません。活動状態は操作権のロックやアクセス制御を提供しません。

## 宛先を限定して送る

ログイン・接続・参加を済ませ、`instance.ready()` が完了してから呼び出します。

```ts
const unsubscribe = instance.onDirectedEvent(({ event }) => {
  // event.data の sender_user_id / sender_presence_id はサーバーが設定する。
  receiveApplicationEvent(event.eventType, event.data);
});

const eventId = instance.sendDirectedEvent({
  eventType: "custom.message.created",
  recipientUserIds: [recipientUserId],
  data: { text: "この内容は指定した相手に届けます" },
});

// 画面の破棄時などに購読を解除する。
unsubscribe();
```

`recipientUserIds` は対象ユーザーの現在の全Presence、`recipientPresenceIds` は特定の接続Presenceを指定します。両方の同時指定はできません。どちらか片方に、重複しない正規形UUIDv7を1〜64件指定します。自分の送信元Presenceにも同じイベントが返ります。

全宛先が同じInstanceの参加者であることを確認してから配信します。1件でも無効なら全体を拒否し、一部の相手だけに送ることはありません。`eventType` は `custom.` で始まる最大128文字のASCII名前空間です。

イベントのデータは送信者情報を含むProtocol BuffersのStructとして最大64KiBです。宛先・ヘッダーを含むフレーム全体にもサーバーの通信上限が適用されます。SDKは送信時のJSONデータにも64KiBの上限を設けています。

送信時の対象Presenceを履歴にも固定するため、後から同じユーザーの別端末が参加しても、その端末へ以前の内容は渡りません。同じイベントIDの再送は元の対象のまま重複を抑止し、送信元へ応答を返します。保持中のResume履歴が利用できる範囲で再配信します。

送信元への応答はサーバーの処理確認です。相手の受信・既読、永続的な会話履歴、別InstanceへのDM、オフラインの受信箱は含みません。

## 入力中・操作中を共有する

活動状態は、所有するPresence・アプリ定義キー・任意の小さなデータ・有効期限で構成されます。自分の状態だけを更新でき、現在のInstance内で共有されます。

```ts
const unsubscribe = instance.activities.onChange((activities) => {
  updateActivityDisplay(activities);
});

// 入力が続いている間、アプリが適度な間隔で更新する。
instance.activities.set({ key: "chat.typing", ttlMs: 5000 });

// 別の用途でも同じAPIを利用する。意味と表示はアプリが決める。
instance.activities.set({
  key: "workspace.editing",
  data: { resourceId },
  ttlMs: 5000,
});

// 入力完了・操作終了時には明示的に解除できる。
instance.activities.clear("chat.typing");
instance.activities.clear("workspace.editing");
const current = instance.activities.current; // 読み取り専用の現在値
unsubscribe();
```

TTLは250〜30,000ms、SDKの既定値は5,000msです。活動が続く間の更新タイミングはアプリが管理します。解除し忘れても期限で消え、切断・退出でも削除されます。再接続時に古い入力中状態を再送せず、新しいsnapshotを取得します。SDKも期限で消去し、サーバーは1秒周期で期限切れを配信します。

活動データはJSON相当のオブジェクトで、深さ16・値1,024個までです。両機能とも有限数だけを受け付け、整数は±9,007,199,254,740,991の範囲に限ります。大きな整数は文字列で表現してください。

キーは小文字英数字・`_`・`-`を`.`で区切った名前空間で、最大128bytesです。空の区間と、先頭名前空間 `core` / `orbisync` / `system` / `admin` は使用できません。

1つの値はProtocol Buffersで最大4KiB、1Presenceで16キー、1Instanceで最大4,096件です。Instance全体のsnapshotにはメタデータと分割フレームを含む512KiB・128chunkの上限があり、件数上限より先に容量に達する場合があります。プロセス全体にも8MiB・65,536件の上限を設けています。超過は明示的に拒否し、部分的なsnapshotを公開しません。

活動状態にはworld revisionと独立した単調増加versionがあります。SDKは古い更新を除外し、分割snapshotを全て受信してから反映します。活動状態は永続化・checkpoint・Resume履歴の対象外です。

## 対応機能とエラー

接続時に宛先限定配信は `orbisync.directed-events.v1`、活動状態は `orbisync.activity.v1` をnegotiationします。SDKは対応を確認し、未対応サーバーには送信しません。宛先限定イベントを従来の全体配信に変換するfallbackはありません。

不正な入力、参加していない宛先、失効した認証、容量超過は拒否されます。SDKの `error` イベントも購読してアプリの表示に反映してください。接続し直した後に機能が利用できなくなった場合も、宛先限定データが全体へ送られることはありません。

契約は [realtime.proto](../../proto/orbisync/v1/realtime.proto)、SDKの公開APIは [TypeScript SDK](../../sdk/typescript/README.md) を参照してください。
