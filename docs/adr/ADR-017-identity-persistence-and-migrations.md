# ADR-017: Identity persistence、audit transaction、migration recovery

- Status: Accepted
- Date: 2026-08-03
- Decision Owners: avistoria

## Context

ADR-002/003/005/008はsession rotation、idempotency、outbox、append-only auditを要求するが、M1に必要な列、cleanup、認証・権限変更とaudit insertの原子性は未確定だった。ADR-005は一般化cross-module UoWを禁止し、ADR-008は認証・権限変更のlocal audit insert失敗時に操作を失敗させるため、狭いtransaction境界を明記する必要がある。

## Decision

### Identity session schema

- Identity coreのRBAC表現では`permissions.name TEXT`をsurrogate IDなしの主キーとし、`roles`はrevision列を持たない。Permissionは名前空間付き文字列そのものがidentityであり、Roleの更新は完全なpermission setの置換として同一transactionで扱うためである。`role_permissions`と`user_roles`のFKは暗黙削除を避けるため既定の`RESTRICT`とし、参照中のRole/User/Permissionを削除する場合はapplication use caseが関連を明示的に解消する。
- `auth_sessions`: `id UUID PK`, `user_id UUID NOT NULL FK users`, `status TEXT CHECK(active/revoked)`, `created_at TIMESTAMPTZ`, `expires_at TIMESTAMPTZ`, `revoked_at TIMESTAMPTZ NULL`, `revocation_reason TEXT NULL`, `revision BIGINT NOT NULL`。indexは`(user_id,status)`と`expires_at`。
- `refresh_tokens`: `id UUID PK`, `session_id UUID NOT NULL FK auth_sessions ON DELETE CASCADE`, `family_id UUID NOT NULL`, `token_digest BYTEA NOT NULL UNIQUE`, `issued_at`, `expires_at`, `consumed_at NULL`, `replaced_by UUID NULL FK refresh_tokens`, `reuse_detected_at NULL`。indexは`(session_id,family_id)`, `expires_at`。raw tokenは保存しない。
- refreshはtoken rowを`SELECT ... FOR UPDATE`し、未消費かつ未期限切れなら同一transactionで旧rowの`consumed_at/replaced_by`と新rowを記録する。消費済みtokenの提示は`reuse_detected_at`を記録し、同じsession/familyをrevokeする。digest比較はconstant-time。
- revoked/expired sessionとtoken rowは監査/incident windowのため90日保持後batch deleteする。reuse検知rowはaudit retention（既定1年）まで保持する。

### Idempotency and outbox

- `idempotency_records`: `key UUID PK`, `actor_user_id UUID NULL`, `operation TEXT`, `request_hash BYTEA`, `state TEXT CHECK(in_progress/completed)`, `status_code SMALLINT NULL`, `response_content_type TEXT NULL`, `response_body BYTEA NULL`, `created_at`, `expires_at`。`(expires_at)` index。24時間後にbatch cleanup。**AUD-C1決定: 一時パスワードは永続化しない。** `response_body` には秘密（temporary_password / token）を一切含めず、`must_change_password: true` のような非秘密のみを保存する。初回 202 レスポンスでのみ一時パスワードを返し、冪等リトライ時は 202 + non-secret body を返す。AEAD による暗号化保存案は不採用（保存しない方が露出面ゼロ、鍵管理不要のため）。同じkey/operation/actorで異なるhashは`IDEMPOTENCY_KEY_REUSED`。
- `outbox_events`: `id UUID PK`, `owner_module TEXT`, `event_type TEXT`, `payload JSONB`, `created_at`, `available_at`, `attempt_count`, `delivered_at NULL`, `last_error_code NULL`。未配送index `(available_at) WHERE delivered_at IS NULL`。payloadにsecretを含めない。配送済みは30日後cleanup、未配送は削除せずalertする。

### Audit transaction ownership

- application crateが用途限定port `IdentityAdministrationStore`を所有する。各methodは一つのidentity mutationと対応するaudit eventを引数に取り、storage-postgres adapterがidentity tableと`audit_events`へのappendを同じPostgreSQL transactionで実装する。
- これは汎用UoWを公開せず、SQLx transactionもapplication/domainへ漏らさない。identity moduleはaudit table/repositoryを直接扱わない。許可use caseはUser/Credential作成・変更、session revoke/reuse、Role/Permission変更だけとする。
- 上記use caseのaudit insert失敗はtransaction全体をrollbackする。login失敗のように正準mutationを伴わないsecurity auditはaudit portへappendし、失敗時はrequestを認証失敗のまま返しつつdurable outbox/alertを使用する。
- 本ADRはADR-005の「一般化cross-module UoW禁止」を維持し、ADR-008が要求するidentity/auditの限定的なatomicityを列挙する補足決定である。

### Migrations and recovery

- migration名はrepository開始時から4桁連番`NNNN_<snake_case>.sql`へ統一する。適用済みfileは変更せず、`sqlx migrate`のforward-only migrationとする。
- 初回migrationはappend-only audit用の`orbisync_runtime` roleを作るため、migration roleに`CREATEROLE`を要求する。組織の運用規則でapplication migrationへ`CREATEROLE`を与えない環境では、DBAが同名NOLOGIN roleを事前provisioningし、migration roleには既存roleへのgrantを実行できる権限を与える。
- down migration file/reversible SQLは要求しない。`test-and-ci.md`の「Contract前のrollback可能性」は、直前backupから隔離DBへrestoreできること、旧binaryがExpand schemaで起動できること、失敗時のforward corrective migrationをrehearsalすることを意味する。
- destructive Contractは自動backupの存在・restore test成功・旧binary退去をgateにする。schemaを戻す目的で適用済みmigrationを編集しない。

## Alternatives

- auditをeventual outboxだけにする案は、ADR-008の認証・権限変更でaudit insert失敗時に操作失敗という決定を満たさない。
- 一般的なcross-module UoWはtable ownershipとcrate境界を弱めるため不採用。
- timestamp migration名はmerge conflictを減らすが、設計例と初回repositoryの可読な順序を優先して連番へ統一した。
- down SQLはdata lossを安全に逆転できる保証がなく、forward-only/backup方針と衝突するため不採用。

## Consequences

- storage-postgresはaudit tableを技術的に書くが、用途限定port経由でのみ許可され、論理所有権はaudit_observabilityに残る。
- **AUD-C1**: 一時パスワードの永続化をやめたため、`response_body` の暗号化・鍵管理は不要になった。24時間 cleanup は引き続き行うが、秘密の保持期間はゼロである。
- cleanup job、outbox relay、retention monitoringがM1の永続化scopeに含まれる。

### Future direction (AUD-C1 target, not implemented in this scope)

- 将来的には一時パスワードを API レスポンスで直接返さず、**期限付き・単回使用の reset token を安全な別経路（例: メール、セキュアな配信チャネル）で利用者へ渡す**設計が最も堅牢である。
- 今回のスコープは「一時パスワードを永続化しない」までとする。上記最終形は将来の ADR（または本 ADR の追補）で「目標設計」「今回はそこまで行かない理由（スコープ・互換性）」「移行時に何が変わるか（API レスポンス形状、永続化、監査）」を明記して残す。
- 実装は本 PR では行わない。記録のみとする。

## Migration

1. `0001_identity_core.sql`, `0002_rest_idempotency.sql`, `0003_audit_outbox.sql`の順に作成する。
2. PostgreSQL 16/17でempty DB、upgrade、concurrent refresh、privilege、restore/forward recoveryを検証する。
3. `migrations/README.md`とtest designのrollback表現を本決定へ合わせる。
