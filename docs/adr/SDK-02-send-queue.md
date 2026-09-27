# SDK-02: TypeScript SDK 送信キューの容量上限・飽和時挙動・バッチ間隔

- Status: Accepted
- Date: 2026-08-24
- Decision Owners: orbisync-86 worker (F6)
- Related: `docs/design/client-sdk.md` §4.3, `docs/design/mobile-resume-interest-backpressure.md` §5 / §8.2, `docs/reviews/design-conformance-audit-2026-08-23.md` F-6

## Context

`client-sdk.md` §4.3 は送信キューを 3 種（latest-wins / reliable / control）に分類するが、容量上限・飽和時挙動・送信バッチ間隔を `[ADR] SDK-02` として未決としていた。
サーバー側の背圧制御は実装済みだが、クライアント側にキューが無いためモバイル回線の停滞時に送信が無制限に溜まり続ける課題があった（F-6）。
M3 の出口条件「latest-wins 集約が機能する」を満たすため、以下を決定する。

## Decision

### 3.1 詰まり判定
`WebSocket.bufferedAmount` が **256 KiB (262,144 bytes)** 未満なら即送信、以上なら latest-wins キューに溜める。
- 根拠: 位置更新 1 件 ≈80 バイト、256 KiB は約 3,200 件分。1,000 エンティティ ×10 Hz の最大負荷でも 0.3 秒分の余裕があり、瞬間的な波で誤検知しないかつモバイル停滞を 0.3 秒で検知できる。

### 3.2 flush 間隔
詰まっている間、**50 ms 周期（20 Hz）** でキューを flush する。
- 根拠: 位置更新の設計頻度は 10 Hz (`test-and-ci.md` load test シナリオ 3)。その倍で flush すればキューイングが遅延を上乗せしない。
- タイマーは詰まっている間だけ動かす。常時 50 ms タイマーを回さない（モバイルバッテリー配慮）。

### 3.3 latest-wins キューの容量上限
**1,024 key**。超過時は最も古い key を drop する。
- key は `(entity_id, component)` でサーバー側キュー（MRIB §8.2）と同一設計。
- 通常はエンティティ数で自然に上限に達しないため、防波堤としての値。

### 3.4 reliable キューの容量上限と飽和時挙動
**256 件**。超過時は **送信側にエラーを返す** (`ReliableQueueOverflowError` を throw)。メッセージを drop しない。
- `client-sdk.md` §4.3 の [REC] および `architecture.md` の「Reliable イベントは破棄してはならない」に従う。

### 3.5 control キュー
Heartbeat は `bufferedAmount` に関わらず **即送信**（優先送信）。詰まり中でも送る。
- heartbeat が止まるとサーバー側がタイムアウトで切断するため。

### 3.6 観測用カウンタ
SDK から以下を読めるようにする:
- latest-wins で drop した更新件数
- 容量超過で drop した key 件数
- reliable キュー飽和エラー回数
- 現在のキュー長（latest-wins / reliable / control それぞれ）

実装は `sdk/typescript/src/client.ts` の `OrbiSyncInstance.getQueueMetrics()` / `OrbiSyncConnection.getQueueMetrics()` および `SEND_QUEUE_BUFFERED_THRESHOLD` 等の定数で公開する。

## Consequences

- クライアント側でも latest-wins 集約が効き、モバイル回線のバースト時に古い位置更新が自動で破棄される。
- reliable メッセージは欠損せず、飽和時は呼び出し側が検知して再試行・バックオフできる。
- 50 ms タイマーが常時動作しないためバッテリー影響は最小。
- 1,024 / 256 の上限は通常到達しないが、暴走時のメモリ防波堤となる。

## Alternatives

- 常時 50 ms タイマーを回す案: 実装は単純だがモバイルで無駄な wakeup が増えるため不採用。
- reliable 飽和時に drop する案: 4 文書が禁止しており、状態不整合を招くため不採用。
- 閾値を 64 KiB / 1 MiB にする案: 80 バイト/件の実測から 256 KiB が 10 Hz ×1,000 エンティティの 0.3 秒分でバランスが良いため現行値を採用。変更時は ADR を更新する。

## References

- `docs/design/client-sdk.md` §4.3 / §12 SDK-02
- `docs/design/mobile-resume-interest-backpressure.md` §5, §8.2
- `docs/reviews/design-conformance-audit-2026-08-23.md` F-6 / M3 出口条件
