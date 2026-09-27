# 受入条件トレーサビリティ: auth-authorization.md

`docs/design/auth-authorization.md` §13 の受入条件 15 項目と、それを検証する
テストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `bce507c` 時点のコードとテストに基づく。統合テストは
`tests/integration/tests/` 配下、crate 内 unit test は `crates/**/src/*.rs`
の `#[cfg(test)]` を指す。fake adapter と決定論的 clock の併用は §13 冒頭
が明示的に許容しているため、port が fake でも HTTP handler / use case /
domain の実装経路を通るテストは production 経路の検証として扱う。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | 自己登録の経路は存在せず、アカウントは管理者操作（単一または CSV）でのみ作成される | PARTIAL | `tests/integration/tests/users_w15.rs::w15_missing_auth_is_unauthorized_401`<br>`tests/integration/tests/users_w15.rs::w15_non_admin_is_forbidden_403`<br>`tests/integration/tests/rest_users_auth.rs::import_users_without_permission_is_403` | User 作成（単一）と CSV import の両経路が未認証 401 / 権限不足 403 で拒否されることは検証済み。未検証: 自己登録用エンドポイント（例: `POST /v1/auth/register`）が存在しない（404 になる）ことの直接テスト。`openapi_route_gate.rs` は YAML 宣言済み operation のみを検査し、Router 側の未宣言 route を列挙しないため、不用意に追加された登録 route を検出できない。 |
| 2 | 作成された User は `must_change_password = true`、初回パスワード変更完了後に `false` | COVERED | `crates/orbisync-identity/src/admin.rs::create_user_requires_server_loaded_permission_and_audits_atomically`<br>`tests/integration/tests/w27_refresh_and_password.rs::w27_password_change_clears_flag_resets_lock_and_audits`<br>`tests/integration/tests/w27_refresh_and_password.rs::w27_http_change_password_end_to_end_clears_must_change` | production `create_user` が返す User の `must_change_password() == true` を unit test が直接検証し、`w27` 系が「DB 上 true で開始 → 変更後に false（service / HTTP 両経路）」の遷移を実 PostgreSQL で検証する。補足: `POST /v1/users` の 201 応答自体にはフラグが含まれないため、HTTP 応答経由での直接観測はできない（`application/vnd.orbisync.user-credential+json` 応答は `temporary_password` のみ）。 |
| 3 | パスワードは Argon2id で保存され、平文 / ハッシュが DB・API 応答・ログに現れない | PARTIAL | `crates/orbisync-identity/src/password.rs::configured_argon2_parameters_are_used_in_the_phc_hash`<br>`crates/orbisync-identity/src/password.rs::hashing_round_trip_and_dummy_verification_work`<br>`tests/integration/tests/w29_identity_read.rs::w29_no_secret_leakage_in_responses`<br>`tests/integration/tests/w27_refresh_and_password.rs::w27_audit_contains_no_token_secret`<br>`tests/integration/tests/identity_persistence.rs::audit_failure_rolls_back_identity_mutation_without_leaking_secret` | 検証済み: hash 生成が Argon2id PHC 形式であること（unit）、管理 API 応答群に平文パスワード / token が混入しないこと（実 HTTP + 実 PG）、監査 metadata に digest 語 / raw token が含まれないこと、エラー `Debug` 出力にハッシュが含まれないこと。未検証: (a) 管理者作成直後の `user_credentials.password_hash` 列が `$argon2id$` PHC であることの DB 直接検証（統合テストは production hash の round-trip を踏むが形式は assert しない）、(b) tracing ログ出力にパスワード平文が含まれないことの直接検証、(c) 管理者 API 応答にパスワードハッシュ（PHC 文字列）が含まれないことの明示検証。 |
| 4 | 存在しない login_id と誤りパスワードで同一形状の 401（enumeration 不能） | COVERED | `tests/integration/tests/auth_w13.rs::w13_wrong_password_and_missing_account_both_401_same` | 実際の `POST /v1/auth/login` handler と production `LoginService` を通し、誤りパスワード / 存在しないアカウントの両方で 401 + 同一 `error.code`（`AUTHENTICATION_REQUIRED`）を検証する。identity store は fake だが、§13 冒頭が fake adapter を許容しており、応答形状を決めるのは production handler / `LoginService::unauthenticated()` 側である。補足: テストは status と `error.code` の同一性のみ比較する（message は production で定数 `"authentication failed"` に統一されているため実質同一形状）。タイミング側面（応答時間差）は未検証。 |
| 5 | 連続失敗が DM-05 閾値（5 回）でロックされ、ロック中の認証は失敗する | COVERED | `tests/integration/tests/rv_b_lockout.rs::rv_b_locked_rejects_correct_password`<br>`tests/integration/tests/rv_b_lockout.rs::rv_b_concurrent_wrong_passwords_trigger_lockout`<br>`tests/integration/tests/rv_b_lockout.rs::rv_b_after_expiry_accepts_correct_password` | 実 HTTP login + 実 PostgreSQL。5 回の失敗で `locked_until` が未来時刻にセットされ、ロック中の正しいパスワードが 401 になること、並行失敗でもカウントが失われないこと、ロック期限後に復帰することを検証する。 |
| 6 | login は 15 分の Ed25519 署名 Access Token と 30 日の opaque Refresh Token を発行し、Refresh Token は HMAC-SHA-256 digest で保存される | COVERED | `crates/orbisync-identity/src/token.rs::access_token_ttl_is_fifteen_minutes_and_ticket_is_sixty_seconds`<br>`crates/orbisync-identity/src/token.rs::m1_corrupted_signature_is_rejected`<br>`tests/integration/tests/refresh_w_h.rs::cr_h_login_refresh_returns_200`<br>`tests/integration/tests/refresh_w_h.rs::cr_h_ttl_setting_changes_expiry`<br>`tests/integration/tests/v11_v12.rs::v12_digest_is_hmac_not_plain` | token unit test が `exp - iat == 900 秒` と署名 1 バイト改変の拒否（Ed25519 検証）を検証する。統合テストは実 HTTP login が `access_token` / `refresh_token` を返すこと、`refresh_tokens.expires_at` が TTL（既定 2_592_000 秒 = 30 日）に従うこと、digest が plain SHA-256 ではなく HMAC-SHA-256 であることを検証する。補足: login 応答の `expires_in == 900` の値そのものの直接 assert はないが、TTL は上記 unit test で担保される。 |
| 7 | `/v1/auth/refresh` は新 Refresh Token を発行して旧 token を消費し、再提示は拒否されて session family が即時失効する | COVERED | `tests/integration/tests/refresh_w_h.rs::cr_h_login_refresh_returns_200`<br>`tests/integration/tests/refresh_w_h.rs::cr_h_reuse_second_is_401_and_audited`<br>`tests/integration/tests/w27_refresh_and_password.rs::w27_refresh_reuse_is_distinct_and_revokes`<br>`tests/integration/tests/identity_persistence.rs::concurrent_refresh_consume_detects_reuse_and_revokes_session` | 実 HTTP + 実 PG で、refresh がローテーション後 token を返すこと、同一 token の再提示が 401 + `token.reuse_detected` 監査になること、session が `refresh_token_reuse` 理由で失効すること、並行消費でも所有者が 1 つに決定することを検証する。補足: 現行 schema では family は 1 セッションのチェーンに対応し、テストも 1 family = 1 session のみのため、「同一 family 内の複数セッションが全て失効する」ケースを区別して検証するテストはない。 |
| 8 | `/v1/auth/logout` で当該 AuthSession が失効し、その Access Token は以後の検証に失敗する | COVERED | `tests/integration/tests/rest_users_auth.rs::logout_revokes_session_and_old_refresh_token_is_rejected`<br>`tests/integration/tests/rest_users_auth.rs::logout_without_token_is_401` | 実 HTTP + 実 PG。logout（204）後に同一 access token での再 logout / 認証が 401 になること（access token 検証失敗）、失効した session に紐づく refresh token が拒否されることを検証する。 |
| 9 | User 無効化で当該ユーザーの全 AuthSession が失効する | COVERED | `tests/integration/tests/identity_persistence.rs::disabling_user_revokes_every_session`<br>`tests/integration/tests/rest_users_auth.rs::disable_then_enable_round_trip_and_disabled_login_rejected` | production store `IdentityAdministrationStore::disable_user_with_audit` + 実 PG で、active session 3 件が全て失効することを検証する。HTTP handler の disable 経路（`set_user_enabled` → `StoreUserStatus` mutation）は同一内容の全 session 失効 SQL（`WHERE user_id = $1 AND status = 'active'`）を持つが、テストは store メソッドを直接呼ぶ。HTTP 経路テストは無効化後の login 拒否までを検証する。補足: 「無効化前に取得した access token が以後 401 になる」ことの WS / REST での E2E 検証は無い。 |
| 10 | Realtime 接続 ticket は 60 秒で期限切れとなり、1 回の atomic consume 後の再利用が拒否される | COVERED | `crates/orbisync-identity/src/token.rs::realtime_ticket_expires_after_sixty_seconds`<br>`tests/integration/tests/auth_w13.rs::w13_expired_ticket_is_rejected`<br>`tests/integration/tests/realtime_rv_a.rs::c1_concurrent_consumers_only_one_owner`<br>`tests/integration/tests/realtime_rv_a.rs::c1_same_ticket_two_ws_only_one_succeeds` | 60 秒 + leeway 後の失効（unit + 実 WS 経路）、実 PostgreSQL `PgRealtimeTicketStore` での 10 並行 consume で成功が 1 件のみであること、同一 ticket による 2 本目の WS handshake が失敗することを検証する。 |
| 11 | ticket 検証前の WS は ClientHello 以外を処理せず、5 秒 timeout で閉じる | PARTIAL | `tests/integration/tests/realtime_rv_a.rs::c3_handshake_timeout_disconnects`<br>`tests/integration/tests/realtime_rv_a.rs::c3_oversized_initial_frame_rejected_at_codec` | 検証済み: handshake timeout 到達で connection が閉じ、metric が増加すること（実 WS + 実 PG。timeout 値は config 駆動で、既定値 5_000ms は `orbisync-config` の default として存在するが、テストは検証速度のため 500ms で実施）。未検証: ticket 検証前に ClientHello 以外のメッセージ（例: Join や不正 protobuf）を送った場合に処理されず切断されることの直接テスト。production 実装は `GatewayError::NotClientHello` を `ProtocolAbuse` に mapping して閉じるが、この経路を踏むテストは存在しない。 |
| 12 | 権限のない主体の管理操作は 403 を返し、use case は実行されない | COVERED | `tests/integration/tests/world_auth_rv_c.rs::rv_c_world_create_without_permission_is_403_and_not_persisted`<br>`tests/integration/tests/world_auth_rv_c.rs::rv_c_instance_create_without_permission_is_403_and_not_persisted`<br>`tests/integration/tests/world_auth_rv_c.rs::rv_c_application_authorizer_is_required_not_only_transport`<br>`tests/integration/tests/users_w15.rs::w15_non_admin_is_forbidden_403`<br>`tests/integration/tests/rest_users_auth.rs::patch_user_without_permission_is_403` | world / instance については 403 + 「DB に永続化されていない」こと + 失敗監査まで検証され、`rv_c_application_authorizer_is_required_not_only_transport` が use case 層自体の権限強制（HTTP をバイパスしても NotAuthorized、非永続化）を検証する。users / roles については 403 と `ACCESS_DENIED` を検証（非永続化の DB 直接 assert は world / instance 側のみ）。 |
| 13 | `entity.update.any` を持たない非 owner は owner 付き Entity を更新できず、client 申告 owner は無視される | COVERED | `tests/integration/tests/entity_w16.rs::entity_w16_update_by_non_owner_rejected_only_sender`<br>`crates/orbisync-world-runtime/src/actor.rs::spawn_ignores_client_owner_without_update_any_permission`<br>`crates/orbisync-world-runtime/src/actor.rs::update_any_permission_allows_other_owner` | 実 WS 経路で非 owner の update が `NOT_OWNER` で拒否され、元 owner に delta が漏れないことを検証する。actor unit test が、`entity.update.any` なしで client が申告した owner が無視され server 確定（requester）の owner が採用されること、`entity.update.any` 保持者は他 owner の Entity を更新できること（permission の正側）を検証する。補足: WS 経路テストの authorizer は testkit の `AllowEntityOwnerAuthorizer`（production `PgWorldAuthorizer` ではない）。 |
| 14 | 所有権移譲は `OwnershipTransferred` event を発行し、監査記録に残る | PARTIAL | `crates/orbisync-world-runtime/src/actor.rs::transfer_ownership_succeeds`<br>`crates/orbisync-world-runtime/src/extension.rs::event_kind_matches_webhook_name` | 検証済み: `TransferOwnership` の適用と `ExtensionEvent::OwnershipTransferred` 発行、event kind 文字列 `entity.ownership_transferred` への mapping（unit）。未検証: 所有権移譲に対応する監査レコード（`audit_events` row）が実経路で書き込まれることのテスト。`wh1_extensions.rs` は同 kind を delivery lease 制約の fixture に使うのみで、監査永続化は検証していない。 |
| 15 | token / ticket / password / hash が log、metric、trace、telemetry に含まれない | PARTIAL | `tests/integration/tests/w27_refresh_and_password.rs::w27_audit_contains_no_token_secret`<br>`tests/integration/tests/refresh_w_h.rs::cr_h_audit_contains_no_secret`<br>`tests/integration/tests/realtime_rv_a.rs::c1_audit_and_logs_contain_no_ticket_material`<br>`tests/integration/tests/w29_identity_read.rs::w29_no_secret_leakage_in_responses`<br>`tests/integration/tests/identity_persistence.rs::audit_failure_rolls_back_identity_mutation_without_leaking_secret` | 検証済み: 監査 rows に raw refresh token / digest が含まれないこと、WS handshake 経路で監査と DB に ticket 素材が残らないこと、管理 API 応答群に token / password が含まれないこと、エラー `Debug` にハッシュが含まれないこと。未検証: (a) tracing ログ出力そのものの capture 検証（`c1_audit_and_logs_contain_no_ticket_material` は名称に反し監査 / DB のみを検査し、ログは未確認と自己言及している）、(b) password 平文のログ混入防止、(c) metric / trace / telemetry への token / ticket / password / hash 混入防止。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 10 |
| PARTIAL | 5 |
| GAP | 0 |

PARTIAL の内訳:

- #1: 自己登録 route の不在（404）直接検証の欠如
- #3: 保存 hash 形式の DB 直接検証、ログへの平文混入防止、管理者 API のハッシュ非含有の明示検証の欠如
- #11: ClientHello 以外の初回メッセージ拒否の検証の欠如
- #14: 所有権移譲の監査レコード永続化の検証の欠如
- #15: ログ / metric / trace / telemetry の secrets 非含有の検証の欠如

## 既知の限界

- `tests/integration/tests/` の多くは `common::pool_or_skip()` で実 PostgreSQL
  を要求する。DB が無い環境ではテストが skip され、本表の判定が意味する
  実行時保証は CI / 検証環境の DB 有無に依存する。
- 実装の変更（特に `LoginService` のエラー mapping、`PgRealtimeTicketStore`
  の consume、`IdentityAdministrationStore` の session 失効 SQL）は本表の
  対応を壊す。設計文書の受入条件を変更した場合は本表を同期すること。
