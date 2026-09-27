# 受入条件トレーサビリティ: rest-api-persistence.md

`docs/design/rest-api-persistence.md` §14 の受入条件 12 項目と、それを検証する
テストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `58cc57d` 時点のコードとテストに基づく。§14 冒頭が fake adapter・決定論的
clock の使用を明示的に許容しているため、port が fake でも application/domain の
実装経路を通るテストは production 経路の検証として扱う。

`openapi/orbisync-v1-planned.yaml` は 21 行（実質空）で、REST 実装済み operation は
`orbisync-v1.yaml` に 39 個の `operationId` として存在し、`bce507c` の配線と
`openapi_route_gate.rs` が示すとおり実装/planned のギャップは無い。文書 §14 本文に
`planned`/`未配線`/`未実装` を前提にした記述は見当たらず、更新は不要だった。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | すべての管理エンドポイントは `/v1` 配下に公開され、公式フロントエンド専用で公開面に存在しない操作がない | PARTIAL | `tests/integration/tests/openapi_route_gate.rs::openapi_implemented_ops_are_routable`<br>`tests/integration/tests/openapi_route_gate.rs::openapi_fake_path_is_not_routable` | 検証済み: `orbisync-v1.yaml` に宣言された全 operation が実際の Router で routable であること、`orbisync-v1-planned.yaml` に残る operation が routable でないこと、宣言と異なる HTTP method が 404 になること、実装+planned の合算が固定総数から変化していない（設計の欠落が無い）こと。未検証: YAML に一切記載の無い、まったく新規の未知パス（`openapi_fake_path_is_not_routable` は 1 個の作り込みパスのみを確認）が Router に存在しないことの網羅的な列挙。テストは既知の operation 集合を起点にした probe であり、Router 全体を独立に enumerate して YAML と突き合わせるものではない。 |
| 2 | REST DTO と domain 型は別型であり、application の公開 API に HTTP/DB 型が現れない（compile-time 検査） | COVERED | `scripts/check_architecture.py`（`ci.yml` の `architecture` ジョブ、`ALLOWED_INTERNAL["orbisync-application"]` が `orbisync-domain` のみを許可） | `orbisync-application` は Axum/SQLx 等の crate に依存グラフ上到達不能であり、Rust の型システム上その公開 API が HTTP/DB 型を名指すことはコンパイル不能。REST DTO（`UserResponse` 等）は `orbisync-transport-http` 側にのみ定義され、application の型とは別に存在する（`crates/orbisync-transport-http/src/users.rs` の `UserResponse` は `orbisync_application` の `User` domain 型と別構造体）。 |
| 3 | User 作成 API の応答に `password_hash` が含まれない。GET/PATCH いずれもハッシュを返さない | PARTIAL | `crates/orbisync-transport-http/src/users.rs`（`UserResponse` 構造体定義、フィールドは `id`/`login_id`/`display_name`/`enabled`/`revision` のみ）<br>`tests/integration/tests/w29_identity_read.rs::w29_no_secret_leakage_in_responses` | 構造的に強い保証: `UserResponse` は `password_hash` フィールドを持たないため serde がそもそも出力し得ない。ただしこれは型定義の確認であり「テスト」ではない。既存のテスト `w29_no_secret_leakage_in_responses` は応答 JSON の文字列化に管理者/一般ユーザーの**平文パスワード**が含まれないことを assert するが、コード内コメントに「For now, just ensure the raw password value not present」とあるとおり、ハッシュ（PHC 文字列）自体の非含有を明示的に検査するものではない。GET/PATCH 応答についても同一の `UserResponse` 型を経由するため構造的には安全だが、それを直接確認するテストは無い。 |
| 4 | エラー応答は `{error:{code,message,request_id,details}}` 形式であり、`code` は機械可読で安定する。DB エラー詳細や stack trace を含まない | PARTIAL | `crates/orbisync-transport-http/src/worlds.rs`（`ErrorEnvelope`/`ErrorBody` 構造体、`InternalError` 時は `message` を固定文字列 `"internal error"` に置換する分岐）<br>`tests/integration/tests/world_auth_rv_c.rs::rv_c_audit_failure_rolls_back_world_insert`（500 応答の発生を確認） | 構造的に確認済み: エラー応答は `ErrorEnvelope { error: { code, message, request_id, details } }` 型で固定され、`code` は `&'static str` の enum 由来で安定する。`ErrorCode::InternalError` にマップされる経路では `error.detail()`（application 層のメッセージ）ではなく固定文字列 `"internal error"` を使うようソースコードで確認できるため、DB エラーの生テキストがそのまま漏れる経路は見当たらない。ただし `rv_c_audit_failure_rolls_back_world_insert` は 500 応答の JSON body を `let (status, _json, headers) = ...` で読み捨てており、実際の応答 body がこの構造・内容であることを直接 assert するテストは存在しない。 |
| 5 | 同一 `Idempotency-Key` の POST 再要求は、初回と同一結果を返し、副作用を重複しない | COVERED | `tests/integration/tests/password_reset_pw1.rs::pw1_idempotency_same_key_returns_same_password`<br>`tests/integration/tests/identity_persistence.rs::simultaneous_idempotency_retries_have_one_owner` | 同一 key での 2 回目の `POST /v1/users/{id}/reset-password` が 202 を返し、新しい `temporary_password` を生成しない（＝パスワードを再変更しない）ことを検証する。異なる key では異なるパスワードが生成されることも確認し、初回結果の非重複を裏付ける。並行リクエストでも所有者が 1 つに決定することも別テストで検証される。 |
| 6 | 一覧 API は cursor ページネーションで、`next_cursor` を辿ると全件を重複・欠落なく取得できる | COVERED | `tests/integration/tests/w29_identity_read.rs::w29_pagination_users_200_and_50_and_traversal` | 250 件の user を作成後、`limit=50` で `next_cursor` を辿って全件走査し、`HashSet` で重複が無いこと（`seen.insert` が false を返さない）と、走査総数が既知の作成件数を満たすことを検証する。 |
| 7 | `login_id` の重複は 409 + 安定 code を返す | GAP | `tests/integration/tests/identity_persistence.rs::constraints_are_enforced_by_postgresql`（部分的に近い） | 上記テストは `login_id` の DB 一意制約が PostgreSQL レベルで機能すること（重複 INSERT が `Err` になること）のみを検証する。`POST /v1/users` に対して重複 `login_id` を送った場合に HTTP 応答が 409 + 安定した `error.code` を返すことを検証する統合テストは見つからなかった（`rest_users_auth.rs`・`users_w15.rs` のいずれにも該当テストなし）。DB 制約と HTTP エラーマッピングの間に検証の空白がある。 |
| 8 | revision 不一致の更新は 412 `REVISION_MISMATCH` を返し、状態を変更しない | PARTIAL | `tests/integration/tests/rest_users_auth.rs::patch_user_updates_display_name_and_bumps_revision_once`（stale `If-Match` 部分）<br>`tests/integration/tests/rest_worlds_roles.rs`（world PATCH の stale revision 部分）<br>`crates/orbisync-transport-http/src/lib.rs::revision_mismatch_is_412`（unit test） | `crates/orbisync-transport-http/src/users.rs` のドキュメントコメントは、`PATCH /v1/users/{id}` の stale `If-Match` を意図的に `409 RESOURCE_CONFLICT` として実装しており（`DeleteRole` の先例に合わせるため `412 REVISION_MISMATCH` を採用しなかったと明記）、対応する統合テストも実際に `StatusCode::CONFLICT`（409）を assert する。`rest_worlds_roles.rs` の world PATCH も同様に 409 を返す。一方 `412 REVISION_MISMATCH` は realtime WS の entity 更新コマンド（`entity_w16.rs`）や `crates/orbisync-transport-http/src/lib.rs::revision_mismatch_is_412` という unit test で実在する。つまり「revision 不一致で状態が変わらない」こと自体は REST/WS 双方でテストされ機能しているが、**REST の user/world PATCH は文書が名指す `412 REVISION_MISMATCH` ではなく `409 RESOURCE_CONFLICT` を返しており、文書の記述と実装が一致しない**。 |
| 9 | module A の repository が module B のテーブルを直接更新する query が存在しない（所有権検査） | GAP | (なし) | `scripts/` 配下に crate 依存関係を検査する `check_architecture.py` はあるが、SQL query が触るテーブルと crate（module）の対応を検査する専用スクリプトは見つからなかった（`table`/`ownership`/`module` を名前に含む check script は存在しない）。手動レビューでの担保に留まり、自動検証は無い。 |
| 10 | migration は Expand → Migrate → Contract の各段階で旧バージョンと共存可能であり、既存 DB からの移行テストを通る | COVERED | `tests/integration/tests/migrations.rs::test_audit_source_ip_split_preserves_data_and_privileges` | 旧 migration（version 3）適用済みスキーマに legacy 形式データを insert し、新 migration（version 13、source_ip 分離）適用後もデータが正しく新テーブルに移行されることを実 PostgreSQL 上で検証する。補足: 検証対象は v3→v13 の 1 組のみで、Expand/Migrate/Contract の全段階を貫く一般的なテストではない（`test-and-ci.md` トレーサビリティ表の #11 と同一の根拠）。 |
| 11 | SQL injection 試験（悪意ある入力）に対し、parameterized query により意図しない query が実行されない | GAP | (なし) | `crates/orbisync-storage-postgres/src/*.rs` を grep した限り、ユーザー入力を `format!` 等で SQL 文字列に直接組み込む箇所は見当たらず（`sqlx::query`/`query!` によるバインドパラメータのみ）、構造的な緩和はある。しかし悪意ある入力（例: `'; DROP TABLE users; --` 相当の login_id や display_name）を実際に送って意図しない query が実行されないことを確認する adversarial なテストは存在しない。 |
| 12 | DB 障害を注入したとき、永続更新が成功扱いにならず、500 を返す | COVERED | `tests/integration/tests/world_auth_rv_c.rs::rv_c_audit_failure_rolls_back_world_insert`<br>`tests/integration/tests/world_auth_rv_c.rs::rv_c_failure_audits_are_persisted_for_both_resources` | 実 PostgreSQL に一時トリガーを仕込んで `audit_events` への insert を意図的に失敗させ、`POST /v1/worlds` が 500/503 を返すこと、かつ world 行が同一トランザクション内でロールバックされ永続化されていない（`count == 0`）ことを直接検証する。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 5 |
| PARTIAL | 4 |
| GAP | 3 |

PARTIAL の内訳:

- #1: YAML 宣言済み operation の routable/not-routable は検証されるが、YAML に無い未知エンドポイントの網羅的不在確認は無い
- #3: `UserResponse` にフィールドが無い構造的保証はあるが、ハッシュ非含有を直接 assert するテストは無い（既存テストは平文パスワードのみ検査すると自己言及）
- #4: エラー envelope の型定義と `InternalError` 時の固定メッセージ化は確認できるが、実際の 500 応答 body を読んで検証するテストが無い
- #8: revision 不一致検出自体は REST/WS 双方で機能・テスト済みだが、**REST の user/world PATCH は文書が指定する `412 REVISION_MISMATCH` ではなく `409 RESOURCE_CONFLICT` を返す**（実装側のコメントに意図的な選択と明記）。文書と実装のどちらを正とするか要判断

GAP の内訳:

- #7: `login_id` 重複が HTTP 409 + 安定 code を返すことを検証する統合テストが無い（DB 一意制約のみテスト済み）
- #9: repository のテーブル所有権を検査する自動チェックが無い
- #11: SQL injection に対する adversarial なテストが無い（パラメータ化クエリによる構造的緩和のみ）

## 既知の限界

- `tests/integration/tests/` の多くは `common::pool_or_skip()` または同等の DB 接続ヘルパーで実 PostgreSQL を要求する。DB が無い環境ではテストが skip され、本表の判定が意味する実行時保証は CI/検証環境の DB 有無に依存する。
- #8 の文書と実装の不一致（412 vs 409）は本ワーカーの担当範囲では修正しない。文書の記述を更新するか実装を揃えるかは別途判断が必要。
