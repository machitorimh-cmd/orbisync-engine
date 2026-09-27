# 受入条件トレーサビリティ: deployment-and-threat-model.md

`docs/design/deployment-and-threat-model.md` §5 の受入条件 17 項目と、それを
検証するテスト / CI ジョブの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `31fc324` (origin/main) 時点のコードとテストに基づく。CI
（`.github/workflows/ci.yml`）は `compose-e2e`（`scripts/compose-e2e.sh`）と
`restore-drill`（`scripts/restore-drill.sh`）を必須ジョブとして実行するため、
これらのシェルスクリプトもテスト相当として扱う。統合テスト
（`tests/integration/tests/`）は実 PostgreSQL を要求し、DB 無し環境では
skip される点に注意。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | Docker Compose で server と PostgreSQL が起動し、`/health/live` が 200 を返す | PARTIAL | `scripts/compose-e2e.sh`（CI job `compose-e2e`）<br>`tests/integration/tests/http_operational_endpoints.rs::test_liveness_is_independent_of_dependencies` | Compose スタックの起動と実用性（migrate → `/health/ready` 200 → admin login → ticket 発行）を CI の compose-e2e が検証する。`/health/live` の 200 応答は router レベルで fake probe により検証されるが、composed スタック上での `/health/live` 直接 assert は無い（compose healthcheck に宣言があるのみ、compose-e2e が assert するのは `/health/ready`）。 |
| 2 | DB 停止時に `/health/ready` が 503、復旧後に 200 に戻る | COVERED | `tests/integration/tests/http_operational_endpoints.rs::test_readiness_reports_503_while_the_database_is_down`<br>`tests/integration/tests/chaos_e4.rs::chaos_db_stop_and_restart_restores_readiness`<br>`tests/integration/tests/migrations.rs::test_readiness_probe_answers_against_a_real_database` | router レベルで probe 失敗時の 503 を検証し、chaos テストが実 PostgreSQL コンテナの stop / restart で `PgHealthProbe` が not-ready → ready に遷移することを検証する（`CHAOS_POSTGRES_CONTAINER` 未設定時は skip）。単一テストで「HTTP 応答 × 実 DB 停止」を同時に見るわけではないが、両要素の組合せで受入条件を検証している。 |
| 3 | SIGTERM 後に graceful shutdown が完了し、仕様 §37.3 の 8 手順が順序通り実行される | PARTIAL | `crates/orbisync-server/src/main.rs::graceful_shutdown_source_preserves_required_order_and_logging`<br>`crates/orbisync-server/src/shutdown.rs::readiness_flag_and_connection_notices_are_ordered` | shutdown 関数のソース上の手順順序（readiness 低下 → connection 通知 → checkpoint flush → drain → pool close）と、deadline / force ログの存在を静的検証し、`ShutdownState` の通知順序（Begin → Force）を unit test が検証する。未検証: 実プロセスに SIGTERM を送って 8 手順が実行時に完了すること（checkpoint の実保存、実 socket の drain までの E2E）。 |
| 4 | shutdown 中に新規接続が拒否される | PARTIAL | `crates/orbisync-transport-http/src/lib.rs::shutdown_keeps_liveness_but_lowers_readiness` | shutdown flag 設定時に HTTP の新規 work（`POST /v1/instances`）が 503、`/health/ready` が 503、`/health/live` は 200 を返すことを検証する。未検証: WebSocket upgrade の拒否（`realtime_ws_handler.rs` の `is_rejecting()` 分岐と既存接続への `SERVER_SHUTTING_DOWN` 通知は実装があるが、これを踏むテストは無い）。 |
| 5 | OCI image のタグが固定されており、`latest` が使用されない | GAP | なし | 現状のファイルは diges 固定（`deploy/compose/compose.dev.yml` の postgres 17.6 / 16.10、CI の services）と build from source（server）で条件を満たす構成だが、これを検証・強制するテスト / gate が無い（`latest` 検出 lint や compose 検証スクリプトは存在しない）。 |
| 6 | PostgreSQL のバックアップからリストアし、サーバーが正常に起動する | COVERED | `scripts/restore-drill.sh`（CI job `restore-drill`） | 実 PostgreSQL コンテナで pg_dump（custom format）→ pg_restore を行い、リストア先 DB に対して server を起動して `/health/ready` 200 と bootstrapped admin のログイン成功まで検証する。リストア先が元 DB と分離されていることも確認する。 |
| 7 | リストア後に migration が適用され、schema が最新になる | PARTIAL | `scripts/restore-drill.sh`（CI job `restore-drill`） | リストア後の `_sqlx_migrations` 件数が source と一致し、主要テーブルの行数が一致することを検証する（schema 等価性の確認）。未検証: リストア後に `migrate` を実行して新 migration を適用する工程（drill はリストア直後の schema 比較のみで、migrate 適用ステップを実行しない）。 |
| 8 | ブルートフォースログインが rate limit で拒否される | COVERED | `tests/integration/tests/rv_b_lockout.rs::rv_b_concurrent_wrong_passwords_trigger_lockout`<br>`tests/integration/tests/rv_b_lockout.rs::rv_b_locked_rejects_correct_password`<br>`crates/orbisync-transport-http/src/login_rate_limit.rs::blocks_the_attempt_after_the_configured_limit` | 実 PostgreSQL + HTTP router で、連続誤りパスワードによる lockout 発火、lockout 中の正しいパスワード拒否、期限切れ後の回復を検証する。IP 単位の login rate limiter（429 + `Retry-After`）の unit test もある。 |
| 9 | 不正 token（署名不一致）が拒否される | COVERED | `crates/orbisync-identity/src/token.rs::m1_corrupted_signature_is_rejected` | access token と realtime ticket の両方について、署名 1 バイトを壊した token が `InvalidToken` で拒否されることを検証する。HTTP 経路側では `tests/integration/tests/cr13_repro.rs::cr13_broken_sid_is_401_before_fix` が（署名は有効で session 参照が壊れた場合の）401 を検証する。 |
| 10 | 失効後 token の再利用が拒否される | COVERED | `tests/integration/tests/refresh_w_h.rs::cr_h_reuse_second_is_401_and_audited`<br>`tests/integration/tests/rest_users_auth.rs::logout_revokes_session_and_old_refresh_token_is_rejected`<br>`tests/integration/tests/password_reset_pg.rs::pg_reset_revokes_sessions_and_old_token_401` | refresh token の再利用が 2 回目 401 + 監査（reuse_detected）になること、logout 後 session 失効で refresh / access token が 401 になること、password reset 後の旧 access token が 401 になることを実 PostgreSQL + HTTP で検証する。 |
| 11 | message size limit を超える WebSocket メッセージが拒否される | COVERED | `tests/integration/tests/realtime_rv_a.rs::c3_oversized_initial_frame_rejected_at_codec`<br>`crates/orbisync-realtime/src/gateway.rs::test_check_message_size_rejects_oversized` | 実 WS サーバーで設定値（1024 バイト）を超える ClientHello が ServerHello 前に codec 層で拒否されることを検証し、gateway の unit test も limit 超過の `OversizedMessage` を検証する。 |
| 12 | NaN / Infinity を含む TransformInput が拒否される | COVERED | `crates/orbisync-domain/src/transform.rs::rejects_non_finite`<br>`crates/orbisync-world-runtime/tests/m2_minimum.rs::transform_validation_rejects_nan`<br>`crates/orbisync-world-runtime/tests/m2_minimum.rs::transform_validation_rejects_infinity` | `Vec3` / `Quaternion` の構築が NaN / ±Infinity を拒否することを検証する。この構築は `TransformInput` 適用経路（actor の `Transform::new`、`validate_transform_update_with_limits`）から呼ばれる production path であり、domain 層での拒否がそのまま受入条件の実現になる。 |
| 13 | 他ユーザーの entity 更新が所有権検証で拒否される | COVERED | `tests/integration/tests/entity_w16.rs::entity_w16_update_by_non_owner_rejected_only_sender`<br>`crates/orbisync-world-runtime/src/actor.rs::spawn_ignores_client_owner_without_update_any_permission` | 実 WS 経路で非 owner の更新が拒否され元 owner に delta が漏れないことを検証し、actor unit test が client 申告 owner の無視（server 確定 owner の採用）を検証する。 |
| 14 | Interest の可視範囲外の entity が Snapshot に含まれない | COVERED | `crates/orbisync-server/src/realtime_ws_tests_snapshot.rs::snapshot_interest_near_radius_changes_visible_entities`<br>`tests/integration/tests/interest_h6.rs::h6_two_clients_50m_apart_do_not_receive_each_other_state_delta` | near radius を狭めた場合に可視範囲外 entity が snapshot JSON から除外されることを検証し、50m 離れた 2 クライアントが互いの StateDelta を受け取らないことも検証する。snapshot 構築は server の production 関数、interest grid は本物の `UniformGrid` を使用。 |
| 15 | Webhook の宛先がプライベート IP の場合、配送が拒否される | COVERED | `crates/orbisync-extensions/src/delivery.rs::private_ip_endpoint_is_rejected_before_signing`<br>`crates/orbisync-extensions/src/delivery.rs::special_and_mapped_addresses_are_not_public`<br>`crates/orbisync-extensions/src/delivery.rs::real_engine_re_resolves_and_rejects_private_rebinding` | private / loopback / link-local（169.254.169.254 含む）や IPv4-mapped IPv6 の拒否に加え、DNS rebinding（一度 public で解決した名前が private に変わる場合の再解決と拒否）まで実 reqwest engine + テスト用 resolver で検証する。 |
| 16 | 監査ログが append-only で、既存エントリの変更・削除ができない | COVERED | `tests/integration/tests/identity_persistence.rs::runtime_role_cannot_update_or_delete_audit_events`<br>`tests/integration/tests/audit_retention_operator.rs::audit_retention_is_bounded_at_boundary_and_idempotent` | `orbisync_runtime` role による `UPDATE` / `DELETE` が失敗すること（実 PostgreSQL）、maintenance role も直接 DELETE できず保持期間経過分のみ retention function 経由で削除できることを検証する。 |
| 17 | sequence の重複（リプレイ）が検知され、drop される | PARTIAL | `crates/orbisync-realtime/src/sequence.rs::duplicates_do_not_advance` | `InboundSequenceTracker` が受信済み未満の sequence を `Duplicate` と分類し expected を進めないことを検証する。未検証: 実接続で同一 sequence を再送した場合に production 経路（`realtime_ws_connection.rs` の `Duplicate => continue`）で drop され、状態変化が起きないことの E2E 検証（重複 sequence を送る統合テストは存在しない）。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 11 |
| PARTIAL | 5 |
| GAP | 1 |

PARTIAL の内訳:

- #1: composed スタック上での `/health/live` 直接 assert の欠如（compose-e2e は `/health/ready` を見る）
- #3: SIGTERM 実プロセスでの shutdown 手順実行の E2E 検証欠如（静的な順序検証 + unit のみ）
- #4: shutdown 中の WebSocket upgrade 拒否・既存接続通知の検証欠如
- #7: リストア後の migrate 適用工程の実行検証欠如
- #17: 実接続での重複 sequence drop の E2E 検証欠如

GAP の内訳:

- #5: image tag 固定の検証 gate 欠如（現状のファイルは条件を満たすが、逸脱を検出できない）

## 既知の限界

- `tests/integration/tests/` は `common::pool_or_skip()` により実 PostgreSQL を
  要求する。chaos テストはさらに `CHAOS_POSTGRES_CONTAINER` の指定が必要で、
  未指定環境では skip される。
- `scripts/compose-e2e.sh` / `scripts/restore-drill.sh` は Rust / TS のテスト
  ランナーではなく bash スクリプトだが、CI の必須ジョブとして毎回実行される
  ためテスト相当として扱った。ローカルで再現するには Docker が必要。
