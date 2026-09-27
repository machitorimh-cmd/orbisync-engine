# OrbiSync REST API と永続化設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §21（REST API 概要）と §22（データベース設計案）を、REST 資源と公開契約、エラー/冪等性/ページネーション/versioning、テーブル所有権、cross-module atomicity 方針、マイグレーションの実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `AP-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| REST/WebSocket 責務分離、DTO 変換境界、失敗・整合性境界 | `transport-boundaries.md` §1, §3, §4 | 前提として参照。本書は REST 資源と永続化を具体化 |
| モジュール所有権、DAG、Application coordinator、UoW 方針 | `architecture.md` §3, §3.1, §3.3 | 前提として参照。所有権と atomicity 方針を再定義しない |
| 集約・Entity・Value Object・ID 体系 | `domain-model.md` §2, §3 | 前提として参照。テーブルは集約の永続化表現 |
| 認証・認可の意味論 | `auth-authorization.md` | 前提として参照。本書は HTTP 表面を定義 |
| 拡張登録の永続化・配送 | `extension-mechanism.md` | `extension_registrations` の所有のみ参照 |
| 技術選定（JSON+OpenAPI、PostgreSQL、SQLx） | `technology-decisions.md` TD-05, TD-06, TD-11 | 前提として参照 |
| API versioning / idempotency / pagination | ADR-003 | Acceptedな設計前提として参照する |
| 永続化境界（SQLx、migration、transaction、outbox） | ADR-005 | Acceptedな設計前提として参照する |

## 1. 所有権と前提

**設計前提** REST/OpenAPI、認証コンテキスト、管理/制御 API adapter は `http_api` が所有する（`architecture.md` §3）。

**設計前提** PostgreSQL adapter、repository/outbox 実装、マイグレーションは `persistence` が所有する（`architecture.md` §3）。各 domain モジュールは自身が所有する port を定義し、`persistence` がそれを実装する。

**設計前提** Protocol/REST DTO と domain model の相互変換は adapter 境界に閉じ込める（`architecture.md` §2.1、`transport-boundaries.md` §3）。

**[SPEC]** 公式フロントエンドで実行できる操作は、すべて公開 API または公開プロトコルからも実行できなければならない（仕様 §3.2、`architecture.md` §1）。

## 2. REST 資源と責務

**[SPEC]** すべて `/v1` 配下とする（仕様 §21）。

**[REC]** 資源グループと所有モジュールの対応。`http_api` は adapter であり、処理の意味論は各 application use case が所有する：

| 資源グループ | エンドポイント例 | application 所有モジュール |
|---|---|---|
| Authentication | `POST /v1/auth/login`, `/refresh`, `/logout`, `/change-password`, `GET /v1/auth/me`, `POST /v1/realtime/tickets` | `identity_access` |
| Users | `GET/POST /v1/users`, `GET/PATCH /v1/users/{user_id}`, `/disable`, `/enable`, `/reset-password`, `POST /v1/users/import` | `identity_access` |
| Roles | `GET/POST /v1/roles`, `GET/PATCH/DELETE /v1/roles/{role_id}`, `PUT /v1/users/{user_id}/roles` | `identity_access` |
| Worlds | `GET/POST /v1/worlds`, `GET/PATCH /v1/worlds/{world_id}`, `/archive` | `world_directory` |
| Instances | `GET/POST /v1/instances`, `GET /v1/instances/{instance_id}`, `/start`, `/stop`, `/kick/{user_id}`, `/members` | `world_directory`（ライフサイクル調停） |
| Audit | `GET /v1/audit-events`, `GET /v1/audit-events/{event_id}` | `audit_observability` |
| Operations | `GET /health/live`, `/health/ready`, `/metrics`, `/version` | 運用 adapter（`audit_observability` / 起動処理） |

**[SPEC]** 仕様 §21.1〜§21.7 のエンドポイント集合を公開契約として採用する。各エンドポイントの認可要件は `auth-authorization.md` §10 に従う。

**[REC]** 状態変更を伴う操作（作成、有効化/無効化、start/stop、kick、role 割当）は POST/PATCH/PUT/DELETE、読み取りは GET とする。リアルタイム状態のプッシュ配信には REST を使用しない（`transport-boundaries.md` §1.1）。

**設計前提** `/health/live`、`/health/ready`、`/metrics`は認証なし・localhost/internal network限定とする（ADR-009）。`/metrics`は高cardinality labelを含まない（TD-09）。

## 3. 公開契約と domain DTO の分離

**設計前提** REST DTO と domain model の相互変換は adapter 境界に閉じ込める（`architecture.md` §2.1、`transport-boundaries.md` §3.1。背景要件として仕様 §7.3 の「protocol 型と domain 型は明示的に変換する」）。

**設計前提** Application の公開関数は HTTP/DB 型を引数・戻り値にしない（`architecture.md` §2.1）。Axum extractor、HTTP status、SQLx transaction/row を application へ漏らさない。

**[REC]** 変換の方向と所有（`transport-boundaries.md` §3.2 と整合）：

```text
Inbound:  http_api: JSON body → REST DTO → application command
Outbound: application result → REST DTO → JSON body (http_api)
```

**[REC]** 変換は検証の機会として使用する（`transport-boundaries.md` §3.3）：

- **Inbound 変換時**: 構文的検証（フィールド存在、型、サイズ上限）
- **Domain 変換時**: 意味的検証（値域、権限）は domain/application 層が実施
- **Outbound 変換時**: 機密情報の除外（パスワードハッシュ、Refresh Token、内部 ID 等）

**[SPEC]** パスワードハッシュ・token を API 応答へ含めない（仕様 §19.2, §26.4、`auth-authorization.md` §3）。

**設計前提** ADR-003によりOpenAPI 3.1を正本とするOpenAPI-firstを採用した（`docs/adr/ADR-003-rest-api.md`）。

## 4. エラーフォーマット

**[SPEC]** エラーフォーマット（仕様 §21.8）：

```json
{
  "error": {
    "code": "USER_LOGIN_ID_CONFLICT",
    "message": "The login ID is already in use.",
    "request_id": "req_018f47a2-4b3c-7def-8a12-0123456789ab",
    "details": {}
  }
}
```

**設計前提** ADR-014 により `request_id` は HTTP middleware が server 側で採番する `req_<canonical UUIDv7>` 形式とする。これは domain resource ID ではなく correlation ID であり、UUIDv7 payload は ADR-001 の canonical lowercase hyphenated 表現に従う。inbound `X-Request-Id` は authoritative ID として受理せず、server-generated ID を response の `X-Request-Id` header、error body、log、trace、audit log へ伝搬する。

**[SPEC]** `code` は機械可読で安定させる。`message` は人間向けであり互換性を保証しない。内部スタックトレースを返さない（仕様 §21.8）。

**[SPEC]** Transport エラーは公開エラーコードへ変換し、内部 DB/ライブラリエラーを露出しない（仕様 §21.8、`architecture.md` §6）。

**[REC]** エラーコードの境界（`transport-boundaries.md` §4.3 と整合）：

| 公開してよいもの | 公開してはならないもの |
|---|---|
| 機械可読エラーコード | 内部スタックトレース |
| 人間向けメッセージ（互換性非保証） | DB クエリ詳細 |
| request_id | 内部モジュール名 |
| 検証失敗のフィールド名（管理 API） | token / パスワード |

**[REC]** HTTP status とエラーコードの対応：

| 状況 | status | code 例 |
|---|---|---|
| 認証失敗 | 401 | `AUTHENTICATION_REQUIRED` |
| 認可失敗 | 403 | `ACCESS_DENIED` |
| リソース不存在 | 404 | `RESOURCE_NOT_FOUND` |
| 競合（一意制約等） | 409 | `RESOURCE_CONFLICT` |
| revision 不一致 | 412 | `REVISION_MISMATCH` |
| ドメイン検証失敗 | 400 | `INVALID_REQUEST` |
| rate limit | 429 | `RATE_LIMITED` |
| 内部エラー | 500 | `INTERNAL_ERROR`（詳細非公開） |

**設計前提** ADR-003/ADR-016によりエラーコードは`openapi/errors.yaml`のregistryを正本とし、resource固有codeを追加せず上表の汎用codeへ統一する。`code` の安定性は公開契約として保証する。

## 5. 冪等性

**設計前提** REST API の冪等性は `transport-boundaries.md` TB-01 の推奨に従う：POST 以外は冪等、POST は Idempotency-Key header。

**[REC]** 冪等性の設計：

| メソッド | 冪等性 | 方針 |
|---|---|---|
| GET | 冪等 | 副作用なし |
| PUT | 冪等 | 同一要求で同一結果 |
| DELETE | 冪等 | 複数回実行しても状態は同一 |
| PATCH | 条件付き | 部分更新。競合は revision で検知（409） |
| POST | 非冪等 | `Idempotency-Key` header で重複排除 |

**[REC]** `Idempotency-Key` を受け取った POST は、key 単位で結果を保持し、同一 key の再要求には初回結果を返す。key の保持期間と保存先は AP-02 で決める。

**設計前提** ADR-003により`Idempotency-Key` recordをPostgreSQLへ24時間保持する。同じkeyと異なるrequest hashは409を返す。

## 6. ページネーション

**[REC]** 一覧取得（`GET /v1/users`, `/roles`, `/worlds`, `/instances`, `/audit-events`）は cursor ベースのページネーションとする。offset ベースはデータ増大時の性能と一致性の面で採用しない。

**[REC]** 応答は `items` と `next_cursor` を含む。`next_cursor` がなければ最終ページとする。

**設計前提** ADR-003によりopaque cursor、既定50件、最大200件、安定sort key + tie-breakerを採用した。

## 7. API versioning

**[SPEC]** すべて `/v1` 配下とする（仕様 §21）。

**[REC]** v1 内では後方互換の追加変更のみを行う。破壊的変更は新 major version（`/v2`）として導入し、移行期間を設ける。

**[REC]** 破壊的変更の判定、非推奨化の告知方法、OpenAPI の差分検査は AP-04 で決める。

**設計前提** ADR-003により`/v1` prefixとOpenAPI-firstを確定した。破壊的変更は`/v2`で行う。

## 8. DB テーブル所有権

**[SPEC]** 一時状態は主にメモリで管理し、永続状態は PostgreSQL へ保存する（仕様 §10、`architecture.md` §3.3）。

**設計前提** 各永続テーブル/一時状態には所有モジュールを一つだけ割り当てる。他モジュールは所有者の query/port を通じて参照し、直接更新しない（`architecture.md` §3.3）。

**[REC]** 仕様 §22.1 のテーブル一覧（設計案）をベースラインとして採用し、所有モジュールを次に対応させる。`persistence` は全 repository と migration を実装するが、テーブルの論理所有権は domain モジュールが持つ：

| テーブル | 論理所有モジュール | 集約（`domain-model.md`） |
|---|---|---|
| `users` | `identity_access` | User |
| `user_credentials` | `identity_access` | Credential |
| `roles` | `identity_access` | Role |
| `permissions` | `identity_access` | Permission |
| `role_permissions` | `identity_access` | Role（関連） |
| `user_roles` | `identity_access` | User（関連） |
| `auth_sessions` | `identity_access` | AuthSession（`auth-authorization.md` §5.2） |
| `refresh_tokens` | `identity_access` | AuthSession（RefreshToken） |
| `world_definitions` | `world_directory` | WorldDefinition |
| `world_instances` | `world_directory` | WorldInstance |
| `persistent_entities` | `instance_runtime` | Entity |
| `persistent_entity_components` | `instance_runtime` | Entity（Component） |
| `instance_checkpoints` | `instance_runtime` | checkpoint（`state-and-runtime.md` §1.3） |
| `audit_events` | `audit_observability` | AuditEvent |
| `extension_registrations` | `extension_gateway` | ExtensionRegistration（`extension-mechanism.md`） |
| `schema_migrations` | `persistence` | （基盤） |

**設計前提** module が他 module の repository/table を直接操作することは許可しない（`architecture.md` §3.3、ARC-03）。

## 9. テーブル設計と constraints/index

**[REC]** 仕様 §22.2〜§22.5 のスキーマ（設計案）をベースラインとして採用する（users、user_credentials、world_definitions、audit_events）。最終 DDL は ADR-005 / AP-05 / migration で確定する。

**[REC]** 共通の設計規約：

- 時刻は `TIMESTAMPTZ`（UTC）で保存する（仕様 §31.4、`domain-model.md` §4.4）
- 楽観的並行制御に `revision BIGINT` を使用する（`domain-model.md` §4.4）
- IDはUUIDv7のnewtypeで表現し、DDLはPostgreSQL native `UUID` columnを使用する（ADR-001 / DM-01）
- 機密カラム（`password_hash`）は API 応答へ映射しない（§3）

**[REC]** 推奨 constraints/index（所有モジュールの repository が定義）：

| テーブル | constraints / index |
|---|---|
| `users` | `login_id` UNIQUE（仕様 §22.2）、`status` CHECK、`created_at`/`updated_at` NOT NULL |
| `user_credentials` | `user_id` PK + FK→users、`failed_login_count >= 0` CHECK |
| `roles` | `name` UNIQUE |
| `role_permissions` / `user_roles` | 複合 PK、FK→roles/users、削除時 CASCADE の要否は AP-05 |
| `world_definitions` | `revision` NOT NULL、`capacity > 0` CHECK |
| `world_instances` | FK→world_definitions、`lifecycle` CHECK |
| `audit_events` | `occurred_at` / `actor_user_id` / `action` の index（検索用） |
| `auth_sessions` / `refresh_tokens` | FK→users、`status`、token hash UNIQUE |

**設計前提** ADR-001によりID内部表現をUUIDv7、DDLのID columnをPostgreSQL native `UUID`に確定した。公開transportではcanonical lowercase hyphenated UUID stringを使用する（`docs/adr/ADR-001-naming-identifiers.md`）。

**[ADR]** index の具体構成、JSONB（`metadata`, `default_spawn`）の index 要否、`metadata` のサイズ/深度制限は AP-05 / DM-02 で決める。

## 10. cross-module atomicity 方針

**設計前提** ADR 確定前は、module 所有をまたぐ atomic transaction を一般要件としない。原則は各 module 所有データの local transaction である（`architecture.md` §3.3、ARC-03）。

**設計前提** 厳密な横断 atomicity が必要と確認された use case に限り、Application が所有する Unit of Work port を設計候補とする。許可対象は ADR で列挙し、module が他 module の repository/table を直接操作することは許可しない。外部副作用を同一 DB transaction に含めず、outbox の要否も同 ADR で決める（`architecture.md` §3.3、ARC-03）。

**[REC]** 本書は上記方針を具体化して再確認する。横断 atomicity の候補 use case：

| use case | 関与モジュール | 初期方針 |
|---|---|---|
| User 作成 + 初期 Credential 作成 | `identity_access` のみ | module-local transaction（単一モジュール） |
| Role 割当 + 監査記録 | `identity_access` + `audit_observability` | local + audit port（監査は best-effort または outbox） |
| Instance 作成 + 監査記録 | `world_directory` + `audit_observability` | 同上 |
| 永続化 + 外部イベント配送 | 各モジュール + `eventing`/`extension_gateway` | 同一 transaction に外部副作用を含めず outbox（`architecture.md` §5.5） |

**[REC]** 監査記録の原子性は、監査の要件（改ざん対策、保持）と合わせて AP-06 で決める。候補は (a) 同一 local transaction、(b) outbox 経由の eventual、のいずれか。

**[ADR]** 横断 atomicity を許可する use case の列挙と UoW port の採用は ARC-03 / AP-06 で決める。本書は UoW を一般化せず、列挙主義を維持する。

## 11. マイグレーション

**[SPEC]** 仕様 §22.6：

- 前方移行を基本とする
- 破壊的変更は複数リリースに分ける
- Expand → Migrate → Contract 方式を推奨する
- リリース前に既存 DB からの移行テストを行う
- 自動バックアップなしに破壊的 migration を実行しない

**設計前提** マイグレーションは `persistence` が所有する（`architecture.md` §3）。

**[REC]** Expand → Migrate → Contract の各段階で後方互換を保ち、稼働中の旧バージョンと共存可能にする。Contract（旧カラム削除等）は、旧バージョンが完全に退去した後のリリースで行う。

**[REC]** migration は version 管理され、`schema_migrations` で適用済み version を管理する（仕様 §22.1）。

**設計前提** ADR-005によりSQLx migration、`cargo sqlx prepare --check`、PostgreSQL 16/17 supportを確定した。

## 12. 脅威・失敗時挙動

**[REC]** 本書が対象とする脅威と対策：

| 脅威 | 対策 | 根拠 |
|---|---|---|
| SQL injection | parameterized query（SQLx）、動的 SQL の検査 | 仕様 §26.3、TD-11 |
| 内部エラー露出 | 公開エラーコードへ変換、stack trace 非公開 | 仕様 §21.8、§4 |
| IDOR（他ユーザー資源への不正アクセス） | リソース単位の認可 | 仕様 §26.3、`auth-authorization.md` §10 |
| 機密情報漏洩 | outbound 変換で hash/token 除外 | 仕様 §19.2, §26.4、§3 |
| mass assignment | REST DTO で受け取るフィールドを限定 | §3 |
| DB 障害時の不正な成功扱い | 永続更新を成功扱いしない | `architecture.md` §6 |

**[REC]** 失敗時挙動：

| 失敗 | 応答 | 備考 |
|---|---|---|
| 一意制約違反 | 409（安定 code） | DB エラー詳細を露出しない |
| revision 不一致 | 412（`REVISION_MISMATCH`） | 楽観的並行制御（`domain-model.md` §4.4、ADR-003） |
| DB 接続/クエリ失敗 | 500（`INTERNAL_ERROR`） | 永続更新を成功扱いしない |
| 検証失敗 | 400（`INVALID_REQUEST`） | フィールド名は公開してよい |

**設計前提** DB 障害時に永続更新を成功扱いしない（`architecture.md` §6）。

**[SPEC]** Transport エラーは公開エラーコードへ変換する（仕様 §21.8）。

## 13. 監査・観測

**[REC]** REST 管理操作の監査は各 application use case が audit port を呼ぶことで実施する（`auth-authorization.md` §12、`architecture.md` §1.2）。`audit_events` は `audit_observability` が所有する（§8）。

**[SPEC]** 最低限のメトリクスは仕様 §27.2 の確定要件である（TD-09）。本書の領域に関連する項目：`http_requests_total`、`http_request_duration_seconds`、`db_query_duration_seconds`。

**[SPEC]** 構造化ログの必須フィールドは仕様 §27.1 の確定要件である（TD-08）：request_id、connection_id、user_id、event、duration_ms、error_code 等。

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4、TD-08）。

## 14. テスト可能な受入条件

**[REC]** 実装は次の受入条件をテストで示す。repository は fake adapter、clock は決定論的実装を用いる（`architecture.md` §9、仕様 §31.4）。

1. すべての管理エンドポイントは `/v1` 配下に公開され、公式フロントエンド専用で公開面に存在しない操作がない。
2. REST DTO と domain 型は別型であり、application の公開 API に HTTP/DB 型が現れない（compile-time 検査）。
3. User 作成 API の応答に `password_hash` が含まれない。GET/PATCH いずれもハッシュを返さない。
4. エラー応答は `{error:{code,message,request_id,details}}` 形式であり、`code` は機械可読で安定する。DB エラー詳細や stack trace を含まない。
5. 同一 `Idempotency-Key` の POST 再要求は、初回と同一結果を返し、副作用を重複しない。
6. 一覧 API は cursor ページネーションで、`next_cursor` を辿ると全件を重複・欠落なく取得できる。
7. `login_id` の重複は 409 + 安定 code を返す。
8. revision 不一致の更新は 412 `REVISION_MISMATCH` を返し、状態を変更しない。
9. module A の repository が module B のテーブルを直接更新する query が存在しない（所有権検査）。
10. migration は Expand → Migrate → Contract の各段階で旧バージョンと共存可能であり、既存 DB からの移行テストを通る。
11. SQL injection 試験（悪意ある入力）に対し、parameterized query により意図しない query が実行されない。
12. DB 障害を注入したとき、永続更新が成功扱いにならず、500 を返す。

## 15. 要 ADR 事項

本書が主担当となる判断を AP ID で管理する。他文書が正本の判断（ADR-001、ADR-003、ADR-005、ARC-03、DM-01、DM-02 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| AP-01 | エラーコード registry と命名規則 | **ADR-016で解決:** `errors.yaml`の汎用codeへ統一 | 仕様 §21.8 の `code` 安定性 |
| AP-02 | Idempotency-Key の保持期間・保存先 | DB 保存、保持期間は有限 | TB-01、ADR-003。POST の再試行安全性 |
| AP-03 | cursor 形式と page size | opaque cursor、既定/最大 size を設定 | ADR-003。一覧の性能と安定ソート |
| AP-04 | API 破壊的変更の判定と非推奨ポリシー | v1 は後方互換、破壊は v2 | ADR-003。公開契約の安定性 |
| AP-05 | constraints/index 具体構成と JSONB 制限 | FK/CHECK/検索 index、metadata は DM-02 | 性能と整合性。具体は負荷試験で調整 |
| AP-06 | 監査記録の原子性と outbox 要否 | local transaction を基本、横断は ARC-03 で列挙 | ARC-03。UoW を一般化しない |

API versioning / idempotency / pagination の確定は ADR-003、永続化境界（SQLx、migration、transaction、outbox）は ADR-005、横断 atomicity の許可列挙は ARC-03 が正本であるため、本書の AP ID では管理しない。
