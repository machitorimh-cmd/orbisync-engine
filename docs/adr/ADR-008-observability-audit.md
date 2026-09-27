# ADR-008: Observability・監査

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

log、metric、trace、PII、audit保存・保持・改ざん対策の境界を運用機能実装前に固定する必要がある。

## Decision

- application logはJSON structured logとし、`tracing`を使用する。
- metricはPrometheus形式とし、`/metrics`は既定でlocalhost/internal networkだけへ公開する。
- trace exportはOTLP、既定無効とする。有効時はerror/管理操作100%、通常REST 10%、transform 1% samplingとする。
- debug logのUser/Session IDは用途別keyのHMACでpseudonymizeする。auditは権限管理された実IDを保持する。
- raw token、ticket、password、Authorization header、完全payloadを全signalで禁止する。
- auditはPostgreSQL append-only tableへ保存し、application runtime roleにUPDATE/DELETE権限を与えない。
- audit保持期間は既定1年、設定で延長可能とする。短縮は明示的な運用承認を必要とする。
- audit export失敗は正準状態更新を止めず、durable outboxへ保持しalertする。認証・権限変更のlocal audit insert失敗は操作を失敗させる。

## Decision — refresh 拒否時の監査判断 (CR-20 / W-27 / CR-H)

`crates/orbisync-storage-postgres/src/identity.rs:rotate_refresh_token` の2分岐は
監査行を残さず `tx.rollback()` して `RefreshRotation::Rejected` を返していた。
**CR-H で実装済み**（`POST /v1/auth/refresh` 配線時に `action = "token.rejected"` /
`result = "failure"` の監査を `identity.rs` に追加済み）。

- 未知 digest (`row` が `None`): 提示された digest に対応する行が無い。
- 期限切れ / 非 active セッション (`expires_at <= now || session_status != "active"`): 有効なセッションに紐づかない。

実装 (CR-H):

- 上記2分岐とも `tx.rollback()` 後に **別トランザクション** で監査行を挿入し `commit` する
  （同一トランザクション内で `rollback` すると監査も消えるため）。未知 digest は
  `actor_user_id = None` / `target_type = "refresh_token"` / `reason = "unknown_token"`、
  期限切れ/非 active は `actor_user_id = Some(user_id)` /
  `target_type = "auth_session"` / `session_id`・`family_id`・`reason = "expired"` / `"inactive_session"` を記録する。
  これにより拒否時も監査が永続化され、W-27 §0.1 の要求を満たしつつ `rollback` と両立する。
- 配線前は `POST /v1/auth/refresh` が planned で HTTP に未配線であり、未認証プローブで
  audit テーブルを洪水させないため意図的に監査を残していなかった（W-27 の「意図的か欠落か」
  判断の記録）。配線後は CR-H で監査を実装したため、本 ADR の当該判断は「実装済み」に更新する。

監査行の内容方針 (W-27 §0.1):

- raw token も keyed digest も監査行 (`audit_events.details` / `metadata` / `target_id` 等) に入れない。PostgreSQL 側は digest を indexed key としてのみ用い、監査は `session_id` / `family_id` / `reason` 等の非秘密メタデータに限定する。`rotate_refresh_token` のドキュメントコメントおよび本 ADR がその禁止を再確認する。

参照: `identity.rs` の `rotate_refresh_token` 実装コメント、本 ADR の Decision、W-27 §0.1。

## Alternatives

- 全trace保存: transform trafficで費用・負荷が過大。
- auditを通常logだけで管理: 改ざん耐性と検索性が不足。

## Consequences

- audit DB容量、retention job、key rotationが必要になる。
- HMAC key変更時は期間をまたぐ相関が失われる。

## Migration

signal schema、redaction test、audit DB role、retention job、export failure testを作成する。
