# 受入条件トレーサビリティ: client-sdk.md

2026-09-27更新。以前の `31fc324` 時点の表は実装変更を反映していなかったため更新した。

- `COVERED`: 記載した振る舞いをテストが直接検証する。
- `PARTIAL`: 未検証部分を備考に明記する。実装の欠落と追加検証が必要な範囲を区別する。
- `GAP`: 対応する検証がない。

SDKテストは `sdk/typescript/src/`、Rust結合試験は `tests/integration/tests/`。`client.e2e.test.ts` は実Rust helperとTCP WebSocketを使用し、DBはfake store。DBを含む実mainの前回検証は wiring-completion (internal record omitted from this source distribution)、今回の追加障害注入は boundary-audit (internal record omitted from this source distribution) を参照する。

| # | 受入条件（要約） | 状態 | 直接の証拠・確認範囲 |
|---|---|---|---|
| 1 | loginとtokenの秘匿 | COVERED | `client.acceptance.test.ts` でconsole全5種をcaptureし、login→refresh→ticket→接続のpassword・新旧token・ticketが含まれないことを照合。公開状態の秘匿は `client.connection-state.test.ts` |
| 2 | 有効期限で自動refresh後に接続 | COVERED | `client.acceptance.test.ts` で60秒tokenの期限を越え、refresh→更新tokenでticket取得→WebSocket接続の順序を照合。80%境界と期限再起算は `client.auth-state.test.ts` |
| 3 | refresh失敗でUnauthenticated・RefreshFailed通知・resume停止 | COVERED | `client.auth-state.test.ts` の単一通知・token破棄、`client.lifecycle.test.ts` のresume停止 |
| 4 | ClientHello期限・属性・交渉・ticket単一使用 | COVERED | `client.acceptance.test.ts` がSDKの送信bytesをdecodeし、sequence・version・時刻・ticket・全Hello属性・subprotocol・交渉結果を直接照合。timeoutは `client.lifecycle.test.ts`、単一使用は `realtime_rv_a.rs`。v1のcompression/featuresはADR-004に従い空 |
| 5 | join後のSnapshotと状態通知 | COVERED | `client.acceptance.test.ts` がReady→Joining→全chunk適用→Activeを照合。途中の送信を保留し、完成したSnapshotのrevisionで初回送信。明示revisionと再送payloadは維持。joinはhandler登録用handleを先に返す。実mainでもActiveと適用cursorを照合 |
| 6 | 入室拒否後のPromise拒否・接続維持・再試行 | COVERED | `client.lifecycle.test.ts` のmachine-readable拒否と同一socket再試行、`realtime_e2e.rs` の不在instance拒否後の実socket入室 |
| 7 | TransformInputのlatest-wins集約 | COVERED | `client.queue.test.ts` の同一key集約・別key維持・容量境界・sequence連続性 |
| 8 | reliableの分離と上限 | COVERED | `client.queue.test.ts` の256件境界。未確認分も含む。`client.e2e.test.ts` の未到達送信の再送。`common/custom_events.rs` の重複再応答・再配信なし |
| 9 | 接続ごとの送受信sequence | COVERED | `client.queue.test.ts` の送信番号、`client.lifecycle.test.ts` の受信重複・gap、実TCPでresume/fresh join後の通信継続 |
| 10 | full-jitter backoffと30秒上限 | COVERED | `client.backoff.test.ts` の境界・上限・jitter端点 |
| 11 | resumeでreplayを適用して復帰 | COVERED | `client.e2e.test.ts` が実TCPで切断中2イベントの順序・重複なし、Snapshot component、Snapshot適用時とActive通知時のcursor＝サーバーrevisionを照合。そのrevisionで更新継続。実main+DBでも復帰後Active/cursor一致を確認 |
| 12 | resume拒否後のfresh joinとSnapshot | COVERED | `client.e2e.test.ts` の無効token→fresh Snapshotのentity照合→新token→更新継続 |
| 13 | HeartbeatAck連続未受信から再接続 | COVERED | `client.acceptance.test.ts` が仮想時計でproductionのintervalとack timeoutを3回満了させ、Heartbeat実送信・早すぎる切断なし・3回目の再接続を検証。miss hookは使用しない。実socketでの復帰は `client.e2e.test.ts` |
| 14 | Snapshot適用とrevision整合 | COVERED | `client.queue.test.ts` のchunk完成後のrevision置換・削除entity除去・不正JSON時の状態維持。アプリに渡す完成状態はsnapshotAppliedで検証 |
| 15 | 古いDeltaの破棄 | COVERED | `client.queue.test.ts` の8→7→8 revisionで1回のみ通知。古いcommand応答もrevisionを巻き戻さない |
| 16 | EntityCommand / DomainEventの通知と反映 | COVERED | `client.e2e.test.ts` の送受信・components更新・resume後の再更新。実mainの生成/移譲/削除は前回live-runtimeシナリオ |
| 17 | Snapshotの再組立て・欠落時回復 | COVERED | `client.queue.test.ts` のUTF-8分割・不正chunk・期限切れ時再接続要求。実TCPのSnapshot再取得と適用も検証 |
| 18 | 機械可読エラー型と秘匿 | COVERED | `client.acceptance.test.ts` が8種のwire codeで公開4フィールドのみ保持することを検証。login/refresh/ticketの通信例外と不正JSONへ秘密値を注入し、Promise・error通知・stack/causeに残らないことを照合。AbortErrorの識別も維持 |
| 19 | handler例外隔離とerror通知 | COVERED | `client.queue.test.ts` のEVENT_HANDLER_FAILED・他handler継続・error handlerで再帰しない検証、`client.lifecycle.test.ts` の初期配信継続 |
| 20 | 並行API・終了状態の保護 | COVERED | 並行connect共有、join重複拒否、退室後送信拒否、切断待機中のqueue保持、refresh/login/connect世代逆転を各SDKテストで検証 |

## 集計と限界

20 COVERED / 0 PARTIAL / 0 GAP。SDK受入条件に対応する直接検証の集計であり、全入力組合せ・全ブラウザ・エンジンの別機能を保証する集計ではない。追加の再現・修正と実mainの再検証は completion-validation (internal record omitted from this source distribution) を参照。

`phase` はtransport状態、`sessionState` はReady/Joining/Active/Resuming/Closed、認証状態は `getAuthState()` で公開する。`phase: connected` だけでは初期同期完了を意味しない。

DomainEventの履歴はprocess内の有界メモリ。resume不能時の未確認送信はDELIVERY_UNKNOWNで通知する。永続チャット履歴や再起動を越えるDomainEventのexactly-once保証は未提供の仕様範囲であり、テスト成功として扱わない。
