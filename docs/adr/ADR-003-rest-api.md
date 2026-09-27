# ADR-003: REST API契約・versioning・冪等性

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

REST APIのsource of truth、versioning、error、pagination、retry安全性、楽観的並行制御を実装前に固定する必要がある。

## Decision

- OpenAPI 3.1 documentをREST公開契約の正本とするOpenAPI-firstを採用する。
- endpointは`/v1` prefixを使用する。互換変更は同じmajorへ追加し、破壊的変更は`/v2`とする。
- errorは`{"error":{"code","message","request_id","details"}}`で統一し、安定したcodeを`openapi/errors.yaml`で管理する。
- 一覧はopaque cursor paginationとし、既定50件、最大200件とする。cursorはserverだけが解釈し、sort keyとtie-breakerを署名付きで保持する。
- 状態変更POSTは`Idempotency-Key`を受理する。keyはUUIDv7、request hash・status・responseをPostgreSQLへ24時間保存する。
- 同じkeyと同じrequestは保存済みresponseを返し、異なるrequestは`409 IDEMPOTENCY_KEY_REUSED`とする。
- revisionを持つresourceのPATCH/DELETEは`If-Match`を要求し、不一致は`412 REVISION_MISMATCH`とする。
- OpenAPI lintとbreaking-change検査をPR CIで必須にする。

## Alternatives

- Code-first生成: 実装と近いが、domain/transport境界と契約reviewが曖昧になるため不採用。
- offset pagination: 更新中に重複・欠落しやすいため不採用。
- server-wide transactionによるretry: 外部副作用を含められず、key単位の結果保存より不明確なため不採用。

## Consequences

- contract変更を実装前にreviewできる。
- idempotency recordのcleanupとresponse size上限が必要になる。
- cursor内容は公開contractではなく、decode不能・期限切れを安定errorへ変換する。

## Migration

実装前のためdata migrationはない。`openapi/orbisync-v1.yaml`と`openapi/errors.yaml`を作成し、contract testを追加する。
