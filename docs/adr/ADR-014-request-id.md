# ADR-014: request_id の採番と伝搬

- Status: Accepted
- Date: 2026-08-03
- Decision Owners: avistoria

## Context

REST の `request_id` は response、structured log、trace、audit log を結ぶ相関 ID である。現在の readiness handler は process-local な連番 `health-N` を生成するため、再起動や replica 間で衝突し、同じ ID を structured log に記録していない。

ADR-001 は永続・公開 resource ID を UUIDv7 の canonical lowercase hyphenated string とする。一方、`rest-api-persistence.md` §4 の相関 ID 例は `req_01...` という接頭辞を持つ。OpenAPI の `ErrorEnvelope.request_id` は一般の `string` であり、どちらも表現可能である。

外部の `X-Request-Id` を無検証で採用すると、改行等による log injection、既存 ID の詐称、監査証跡の汚染を許す。

## Decision

- request ID は HTTP middleware（tower layer）が、認証や route handler より前に全 HTTP request へ一度だけ採番する。個別 handler は採番しない。
- 形式は `req_<uuidv7>` とする。`<uuidv7>` は ADR-001 と同じ canonical lowercase hyphenated UUIDv7 string である。例: `req_018f47a2-4b3c-7def-8a12-0123456789ab`。
- `request_id` は domain resource ID ではなく運用上の correlation ID であるため、型識別用の `req_` prefix を持つ。UUIDv7 payload の生成・canonical 表現は ADR-001 を正とし、`req_` prefix は本 ADR が定める transport 表現とする。
- inbound `X-Request-Id` は信頼境界にかかわらず authoritative request ID として受理せず、常に server が新しい ID を採番する。これにより client による ID 詐称と監査汚染を防ぐ。
- 将来 client 側の相関値が必要になった場合は `client_request_id` として server ID から分離し、文字種・長さを検証して log escaping を適用する。今回の契約には追加しない。
- server-generated ID を response の `X-Request-Id` header に常に設定する。error response の `ErrorEnvelope.error.request_id` には同じ値を設定する。
- middleware は request span に `request_id` を record し、その request 中のすべての structured log と trace span へ同じ値を伝搬する。async task を派生させる場合も instrument した span を伝搬する。
- 管理操作の audit log は `observability-and-config.md` §5.2 の `request_id` に同じ server-generated ID を保存する。Milestone 1 の audit 実装は本 ADR に従う。
- proxy が独自 correlation ID を必要とする場合も、server ID を置換しない。proxy ID は別 header / field で管理する。

## Alternatives

### UUIDv7 の canonical string のみ

ADR-001 と同一表現になるが、resource ID と correlation ID をログや運用画面で判別しにくい。

### ULID または process-local counter

`req_01...` を短く表現できる、または実装が単純という利点はあるが、既存 UUIDv7 ecosystem との統一を失う。counter は再起動・replica 間の一意性を満たさない。

### inbound `X-Request-Id` をそのまま採用

end-to-end correlation は容易だが、信頼できない入力による injection、詐称、監査汚染の危険があるため採用しない。

### 信頼済み reverse proxy からのみ採用

network topology と proxy 認証に依存し、直接到達経路や設定不備で信頼境界が曖昧になる。初期構成では server 採番を一貫して使用する。

## Consequences

- process 再起動や replica をまたいでも衝突しにくく、時系列 locality を持つ相関 ID になる。
- client は response header または error body の ID を運用者へ伝え、log、trace、audit を同じ値で検索できる。
- middleware と tracing span の実装が全 route の前提になる。
- server ID と client/proxy の correlation ID を直接引き継げないため、必要になれば分離 field の追加が必要になる。

## Migration

1. Milestone 0 の readiness 503 log に、response body と同じ現在の request ID を記録する。
2. Milestone 1 で tower middleware による `req_<uuidv7>` 採番、span 伝搬、response header 設定を実装する。
3. 全 error mapping が middleware の ID を `ErrorEnvelope` に使用するよう統一し、handler-local counter を削除する。
4. audit log 実装で middleware の ID を必須相関項目として保存する。
