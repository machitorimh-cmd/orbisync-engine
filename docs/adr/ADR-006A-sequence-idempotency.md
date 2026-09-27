# ADR-006A: Sequence・message identity・command冪等性

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

接続内ordering、再接続、replay、dedup、REST/Realtime commandのretry安全性を同じ識別子へ混同せず定義する必要がある。

## Decision

- 各`RealtimeConnectionId`はsend/receive独立の`u64` sequenceを持ち、1から開始する。
- sequenceは同一接続内の送信順序検査専用であり、再接続時に1から再開する。
- reliable/control messageは連続sequenceを要求する。duplicateは無視し、gapは`ResyncRequired`またはprotocol errorとする。
- latest-wins messageは古いsequenceを破棄できるが、正準状態revisionの検証を省略しない。
- `message_id`はUUIDv7とし、reliable replayでは同じ値を維持する。新接続でsequenceを再採番しても`message_id`は変えない。
- state commandはUUIDv7の`command_id`を持ち、Instanceごとのbounded dedup storeへ24時間保持する。
- 同じ`command_id`と同じpayloadは保存結果を返し、異なるpayloadはprotocol errorとする。
- revisionは正準状態の楽観的並行制御、sequenceはtransport ordering、message/command IDはdedupにのみ使用する。

## Alternatives

- sequenceを接続間で継続: durable global counterが必要となる。
- message_idだけでordering: gapを検出できない。

## Consequences

- dedup storeのmemory上限とcleanupが必要になる。
- 24時間を超えるretryは冪等性を保証しない。

## Migration

golden vectorsへduplicate、gap、reconnect、same-ID/different-payload caseを追加する。
