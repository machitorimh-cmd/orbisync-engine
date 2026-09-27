# 受入条件トレーサビリティ: observability-and-config.md

`docs/design/observability-and-config.md` §7 の受入条件 17 項目と、それを
検証するテストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `31fc324` 時点のコードとテストに基づく。統合テストは
`tests/integration/tests/` 配下、crate 内 unit test は `crates/**/src/*.rs`
の `#[cfg(test)]` を指す。fake adapter と決定論的 clock の併用は §7 冒頭
が明示的に許容しているため、port が fake でも handler / use case /
domain の実装経路を通るテストは production 経路の検証として扱う。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | 本番モード（`log_format = "json"`）のログ出力が有効な JSON であり、必須フィールド（§2.2）をすべて含む | COVERED | `crates/orbisync-observability/src/logging.rs::test_json_record_contains_the_always_on_fields`<br>`crates/orbisync-observability/src/logging.rs::test_event_field_falls_back_to_the_callsite_target` | `OrbisyncJson` formatter の出力を共有 buffer に capture し、各行が `serde_json::from_str` で parse できること（有効 JSON）と、`service` / `version` / `level` / `event` / `timestamp`（RFC 3339 UTC）の必須フィールドを検証する。`event` 未指定時の callsite target への fallback も検証する。 |
| 2 | ログ出力にパスワード、token（raw・prefix）、完全な payload が含まれない | PARTIAL | `crates/orbisync-realtime/src/gateway.rs::test_handshake_success_debug_redacts_the_ticket`<br>`crates/orbisync-transport-http/src/auth.rs::login_response_and_logs_contain_no_secrets`<br>`tests/integration/tests/w29_identity_read.rs::w29_no_secret_leakage_in_responses`<br>`scripts/check_secret_logging.py`（CI `architecture` job で実行） | 検証済み: handshake 成功結果の `Debug` 出力に ticket が含まれず `<redacted>` になること、login 応答・audit・HttpState `Debug` に password が含まれないこと、管理 API 応答群に token / password / HMAC key が含まれないこと、CI 静的検査で raw・prefix（slice / `[..n]`）のログ出力が禁止されていること。未検証: tracing ログ出力そのものの動的 capture による password / token / payload 混入防止（テストは応答 body・audit・`Debug` 表現を検査しており、実際のログレコード出力は検査していない）。 |
| 3 | `log_level = "warn"` の場合、`info` 以下のログが出力されない | COVERED | `crates/orbisync-observability/src/logging.rs::test_level_filter_suppresses_lower_severities` | `EnvFilter` に `LogLevel::Warn` を設定した subscriber で `info!` と `warn!` を出力し、buffer に `ignored`（info）が含まれず `kept`（warn）が含まれることを検証する。 |
| 4 | 仕様 §27.2 の最低限のメトリクスがすべて `/metrics` endpoint に出力される | PARTIAL | `crates/orbisync-transport-http/src/metrics.rs::metrics_endpoint_returns_prometheus_format_for_loopback`<br>`scripts/check_metrics_wiring.py`（CI `architecture` job で実行） | 検証済み: `/metrics` が `http_requests_total` / `http_request_duration_seconds` / `auth_login_failures_total` / `db_query_duration_seconds` / `rate_limit_rejected_total` / `process_cpu_seconds_total` / `process_resident_memory_bytes` を含む Prometheus 形式を返すこと。制約: テストは `FakeMetrics::render()` の手書き exposition を検査しており、production exporter が仕様 §27.2 の全メトリクスを出力することの動的検証はしていない（`check_metrics_wiring.py` が静的にワイヤリングを確認するのみ）。 |
| 5 | metric label に高 cardinality 値（instance_id、user_id、connection_id、entity_id）が含まれない | GAP | （該当なし） | metric label の cardinality 制約を検証するテストは存在しない。`FakeMetrics` の label は `method` / `status` / `scope` のみだが、「高 cardinality 値が含まれない」ことを assert するテストはない。 |
| 6 | histogram の bucket が設定され、`_bucket` / `_sum` / `_count` が出力される | PARTIAL | `crates/orbisync-transport-http/src/metrics.rs::metrics_endpoint_returns_prometheus_format_for_loopback` | 検証済み: fake exposition に `http_request_duration_seconds_bucket{le=...}` / `_sum` / `_count` および `db_query_duration_seconds` の同 3 形式が含まれること。未検証: production exporter の histogram が bucket 設定・`_bucket` / `_sum` / `_count` を出力すること（fake の `render()` を検査しているため実装経路の検証ではない）。 |
| 7 | REST request、WebSocket handshake、join flow、snapshot generation、extension delivery、DB query の各 span が出力される | GAP | （該当なし） | tracing span の出力を検証するテストは存在しない。`#[instrument]` や `span!` の使用を確認するテスト、span 名を検査するテストは見つからない。 |
| 8 | transform update の span が sampling され、全件が出力されない（sampling rate < 1.0 の場合） | GAP | （該当なし） | sampling 設定や span 出力件数の上限を検証するテストは存在しない。 |
| 9 | span attribute に機密情報（パスワード、token）が含まれない | GAP | （該当なし） | span attribute の検査テストは存在しない。近接の保証として handshake 結果の `Debug` redaction（`test_handshake_success_debug_redacts_the_ticket`）と CI の `check_secret_logging.py` があるが、いずれも span attribute そのものは検査しないため、本条件に対する直接的な証明にはならないと判定した。 |
| 10 | 全管理操作（ユーザー作成、ロール割当、インスタンス開始/停止、kick 等）の監査ログが記録される | PARTIAL | `crates/orbisync-identity/src/admin.rs::create_user_requires_server_loaded_permission_and_audits_atomically`<br>`tests/integration/tests/w27_refresh_and_password.rs::w27_password_change_clears_flag_resets_lock_and_audits`<br>`tests/integration/tests/delete_role_w_g.rs::delete_role_audit_exists_after_success`<br>`tests/integration/tests/wh1_extensions.rs::instance_lifecycle_persists_started_and_stopped_events` | 検証済み: ユーザー作成（unit・監査 atomic 性込み）、password 変更（実 PG で `audit_events` row 確認）、role 削除（`role.deleted` 監査 row 確認）。instance start / stop について outbox event（`instance.started` / `instance.stopped`）の永続化は検証されるが、対応する `audit_events` row の直接 assert は存在しない。未検証: instance start / stop・kick の監査 row の直接確認、ロール割当（作成以外）の監査確認。 |
| 11 | 監査ログの `details` にパスワード、token、完全な payload が含まれない | PARTIAL | `tests/integration/tests/w27_refresh_and_password.rs::w27_audit_contains_no_token_secret`<br>`tests/integration/tests/refresh_w_h.rs::cr_h_audit_contains_no_secret`<br>`tests/integration/tests/w29_identity_read.rs::w29_no_secret_leakage_in_responses` | 検証済み: refresh token 回転・再利用検出の監査 metadata に raw token / 新 token / digest が含まれないこと（実 PG）、login / refresh 関連監査 rows に secret が含まれないこと、管理 API 応答に password / token が含まれないこと。未検証: 全監査アクション種別の `details` / payload 欄に対する網羅的な secret 検査（password 変更監査の `details` の明示検証、world / instance 操作監査の metadata 検査は行っていない）。 |
| 12 | 監査ログに実 ID（actor_id、resource_id）が記録される | COVERED | `tests/integration/tests/w29_identity_read.rs::w29_audit_required_fields`<br>`tests/integration/tests/w29_identity_read.rs::w29_token_audit_actor_type_user`<br>`crates/orbisync-transport-http/src/auth.rs::login_success_commits_session_refresh_and_audit_atomically` | 実 PG 経路で監査一覧が `audit_id` / `timestamp` / `actor_type` / `action` / `resource_type` / `result` / `request_id` を必ず含むこと、token 再利用監査の `actor_type` が `user` であること、login 監査の `actor_id` が実 user ID と一致することを検証する。 |
| 13 | 起動時に全設定が検証され、不正な値（範囲外、型不一致）で起動が中止される | COVERED | `crates/orbisync-config/src/model.rs::test_validate_rejects_out_of_range_values`<br>`crates/orbisync-config/src/model.rs::test_cfg2_limits_and_cors_validation_are_enforced`<br>`crates/orbisync-config/src/source.rs::test_file_values_are_type_checked`<br>`crates/orbisync-config/src/source.rs::test_environment_overrides_are_validated`<br>`tests/integration/tests/audit_retention_config.rs::audit_retention_days_rejects_zero`<br>`tests/integration/tests/audit_retention_config.rs::audit_retention_days_rejects_values_above_database_bound` | `Config::validate()` が範囲外値（tick Hz、接続数等）を拒否すること、ファイル / 環境変数由来の値が型検査・範囲検査の対象になること、`ConfigErrorKind::InvalidValue` / `FileUnreadable` で load が失敗することを検証する。audit retention の 0 / 上限超過も統合テストで拒否される。 |
| 14 | 不明な設定キーが警告またはエラーとして報告される | COVERED | `crates/orbisync-config/src/source.rs::test_unknown_file_key_warns_by_default_and_rejects_in_strict_mode`<br>`crates/orbisync-config/src/source.rs::test_unknown_environment_variable_is_reported` | 不明ファイルキーがデフォルトで warning に含まれ、strict モード（`UnknownKeyPolicy::Reject`）で `ConfigErrorKind::UnknownKey` エラーになること、不明環境変数（`ORBISYNC_SERVER_MYSTERY`）が warning に報告されることを検証する。secret / reserved 変数が誤検出されないことも検証する。 |
| 15 | secret の環境変数が未設定の場合、起動が中止される | COVERED | `crates/orbisync-config/src/source.rs::test_verify_secrets_requires_named_variables` | `verify_secrets` が `DATABASE_URL` 等の必須 secret 変数が欠落している場合に `ConfigErrorKind::MissingSecret`（key 名付き）で失敗し、全変数が揃っていれば成功することを検証する。 |
| 16 | 設定ファイルに secret の値が直接記述されていない（参照先のみ） | COVERED | `crates/orbisync-config/src/source.rs::test_secret_values_never_appear_in_the_configuration_model` | 設定ファイルが `url_env = "CUSTOM_DATABASE_URL"`（参照先名）のみを持ち、実際の接続文字列が環境変数経由で供給される場合、load 後の `Config` の `Debug` 表現に接続文字列が含まれず参照先名のみが含まれることを検証する。 |
| 17 | CLI 引数 > 環境変数 > 設定ファイル > デフォルトの優先順位が守られる | COVERED | `crates/orbisync-config/src/source.rs::test_precedence_cli_beats_env_beats_file_beats_default`<br>`crates/orbisync-config/src/source.rs::test_discover_path_prefers_cli_then_environment` | 同一キー（`server.bind`）を file / env / CLI override の 3 源で設定し、CLI が env より、env が file より優先され、未 override キーは file が default より優先されることを検証する。config file path の探索順（CLI > env）も検証する。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 8 |
| PARTIAL | 5 |
| GAP | 4 |

PARTIAL の内訳:

- #2: tracing ログレコード出力そのものの動的 capture 検証の欠如（静的検査と応答 / audit / `Debug` 検査のみ）
- #4: production exporter の全メトリクス出力の動的検証の欠如（fake render の検査）
- #6: production exporter の histogram 形式出力の検証の欠如
- #10: instance start / stop・kick の監査 row 直接確認の欠如（outbox event のみ検証済み）
- #11: 全監査アクション種別の `details` / payload 網羅検査の欠如

GAP の内訳:

- #5: metric label の高 cardinality 値排除の検証
- #7: 各処理の span 出力の検証
- #8: transform update span の sampling 検証
- #9: span attribute の機密情報排除の検証

## 既知の限界

- #4・#6 の metrics テストは `FakeMetrics::render()` の手書き exposition を
  検査する。production exporter（`orbisync-application::metrics`）の実装経路
  を通るテストは存在しないため、出力形式の実行時保証は CI の静的ワイヤ
  リング検査に依存する。
- #7〜#9（tracing span 系）は実装コードにもテストにも存在しない領域であり、
  OTLP exporter・sampling の導入時に本表の更新が必要である。
- `tests/integration/tests/` の多くは `common::pool_or_skip()` で実 PostgreSQL
  を要求する。DB が無い環境ではテストが skip され、本表の判定が意味する
  実行時保証は CI / 検証環境の DB 有無に依存する。
