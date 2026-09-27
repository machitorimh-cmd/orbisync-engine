# 受入条件トレーサビリティ: scale-and-nfr.md

`docs/design/scale-and-nfr.md` §10 の受入条件 12 項目と、それを検証する
テストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。
- `意図的スキップ` - 負荷試験（MVP #16）はユーザー判断で意図的にスキップ
  されているため、GAP ではなく意図的スキップとして記録する。

判定は `31fc324` (origin/main) 時点のコードとテストに基づく。Phase 1 / 2 /
3 の負荷・スケール試験シナリオ（§9）に対応する実測テストは存在しない
（`apps/load-generator` はツールの build / unit test のみで、測定を assert する
テストではない）。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | Phase 1: 50 クライアントが 10 Hz で送信し、全クライアントが StateDelta を受信、tick P99 ≤ 50 ms | 意図的スキップ | なし | 負荷試験（MVP #16）はユーザー判断で意図的にスキップ。実測を伴う試験は未実施で、tick 時間の P99 を検証するテストも無い（`crates/orbisync-server/benches/filter_hot_path.rs` は delivery hot path のベンチマークであり、P99 目標の assert ではない）。 |
| 2 | Phase 2: 200 クライアントが Interest 有効で入室し、各クライアントの可視 entity 数 ≤ 50 | 意図的スキップ | なし | 負荷試験（MVP #16）スキップ対象。Interest フィルタ自体の機能検証は `tests/integration/tests/interest_h6.rs::h6_two_clients_50m_apart_do_not_receive_each_other_state_delta` や `crates/orbisync-server/src/realtime_ws_tests_snapshot.rs::snapshot_interest_near_radius_changes_visible_entities` にあるが、200 クライアント・可視数 50 以下という規模の実測は行っていない。 |
| 3 | Phase 2: 1000 接続が複数インスタンスに分散し、全接続が正常に動作 | 意図的スキップ | なし | 負荷試験（MVP #16）スキップ対象。現在の実装は単一プロセス内に複数 instance を持つ構成で、複数サーバープロセスへの分散自体が Phase 3 の将来構成である点も含め未検証。 |
| 4 | TransformInput 処理中に DB 読み書きが発生しない（計装またはトレースで検証） | PARTIAL | `crates/orbisync-server/src/main.rs::transform_updates_do_not_produce_persistence_events`<br>`crates/orbisync-server/src/main.rs::entity_persistence_is_batched_once_per_tick_outside_the_per_handle_loop` | actor レベルで transform 更新が persistence event を生まないこと（auto-spawn の 1 回を除く）と、永続化呼び出し箇所が per-handle tick ループの外で 1 tick に 1 回であることを検証する。未検証: 「計装またはトレースで」DB 読み書きゼロを観測する検証（actor が DB port を持たない構造は確認できるが、観測ベースの検証テストは無い）。 |
| 5 | DB 停止中もリアルタイム一時状態の処理が継続し、ready=false が返る | PARTIAL | `tests/integration/tests/chaos_e4.rs::chaos_db_stop_and_restart_restores_readiness`<br>`tests/integration/tests/http_operational_endpoints.rs::test_readiness_reports_503_while_the_database_is_down` | DB 停止 / 復旧で readiness probe が false → true に遷移すること（実 PostgreSQL コンテナ停止）と、probe 失敗時の `/health/ready` 503 を検証する。未検証: DB 停止中にリアルタイム処理（WS 接続・tick・配信）が実際に継続すること（chaos テストは probe のみを見て、realtime 処理の継続は assert しない）。 |
| 6 | チェックポイントは tick を停止させずに非同期で完了する | PARTIAL | `crates/orbisync-server/src/main.rs::blocked_checkpoint_instance_does_not_create_cross_instance_hol`<br>`crates/orbisync-server/src/main.rs::periodic_checkpoint_concurrency_is_bounded_by_shared_permits`<br>`crates/orbisync-server/src/main.rs::entity_persistence_is_batched_once_per_tick_outside_the_per_handle_loop` | checkpoint 保存が instance ごとに spawn され、共有 semaphore（`MAX_PENDING_CHECKPOINT_SAVES`）で同時実行が bound されること、1 instance の保存滞留が他 instance の保存 / reap を block しないことを検証する。未検証: 保存中も tick が継続すること自体（periodic tick ループは actor tick の後に checkpoint タスクを join するため、保存が次 tick の開始を遅らせ得る構造で、tick 間隔維持の assert は無い）。 |
| 7 | 1 接続の slow consumer が同一インスタンスの他接続の配信レイテンシに影響しない | COVERED | `tests/integration/tests/backpressure_w17.rs::backpressure_w17_slow_does_not_block_fast` | 実 WS サーバーで slow client を滞留させた状態で、別の fast client が引き続き最新 delta を受け取れることを検証する（接続別キューの分離）。`backpressure_w17_latest_wins_slow_consumer_sees_latest` が slow consumer 側の latest-wins 挙動も検証する。 |
| 8 | 1 インスタンスの panic が同一プロセスの他インスタンスに影響しない | COVERED | `crates/orbisync-world-runtime/src/registry.rs::chaos_instance_panic_isolated_to_one_task` | panic を起こす instance task の mailbox が閉じ、別 instance の task が継続動作することを検証する（registry の production 経路）。`scripts/check_instance_task_ownership.py`（CI gate）が task 所有構造の静的保証も行う。 |
| 9 | persistence worker の失敗がリアルタイム hot path へ伝播しない | COVERED | `crates/orbisync-server/src/main.rs::persist_failure_is_counted_and_does_not_block_ephemeral_processing` | 永続化ストアの失敗が failure counter として記録され、その後の transform 更新が ephemeral 処理として継続適用されることを検証する（persistence は tick ループ外の batch 経路）。 |
| 10 | graceful shutdown 時に active connection が drain され、永続化要求が flush される | PARTIAL | `crates/orbisync-server/src/main.rs::graceful_shutdown_source_preserves_required_order_and_logging`<br>`crates/orbisync-server/src/main.rs::blocked_checkpoint_instance_does_not_create_cross_instance_hol`<br>`crates/orbisync-server/src/shutdown.rs::readiness_flag_and_connection_notices_are_ordered` | shutdown 順序（notify → shutdown checkpoint flush → drain）の静的検証、shutdown checkpoint flush が 1 instance の滞留に阻まれず健全な instance の保存を完了すること、connection への shutdown 通知配信を検証する。未検証: 実接続が drain（active_connections → 0）で終わることの実行時検証（SIGTERM 実行テスト無し、WS クライアントを巻き込むテスト無し）。 |
| 11 | レイテンシ目標（P50 / P95 / P99）を負荷試験で測定し、環境と条件を併記 | 意図的スキップ | なし | 負荷試験（MVP #16）はユーザー判断で意図的にスキップ。測定結果の記録も無い。 |
| 12 | セキュリティ要件（TLS 必須、rate limit、size limit）のテストが CI で実行される | PARTIAL | `tests/integration/tests/rv_b_lockout.rs::rv_b_concurrent_wrong_passwords_trigger_lockout`<br>`crates/orbisync-transport-http/src/auth.rs::ticket_issuance_is_rate_limited_per_user`<br>`tests/integration/tests/realtime_rv_a.rs::c3_oversized_initial_frame_rejected_at_codec` | rate limit（login lockout、realtime ticket 429 + 行数不変）と WS message size limit は CI の unit / integration job で検証される。未検証: TLS 必須（アプリは平文で listen し TLS 終端を外部 reverse proxy に委ねる設計のため、TLS を強制するテストは存在しない）。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 3 |
| PARTIAL | 5 |
| GAP | 0 |
| 意図的スキップ（負荷試験 MVP #16） | 4 |

PARTIAL の内訳:

- #4: 計装 / トレースによる DB 読み書きゼロの観測検証の欠如（構造的な unit 検証のみ）
- #5: DB 停止中のリアルタイム処理継続の検証欠如（probe 遷移のみ）
- #6: 保存中も tick が継続すること自体の検証欠如（spawn / 同時実行 bound / 他 instance への非阻塞のみ）
- #10: 実接続を伴う drain の実行時検証欠如（順序の静的検証と checkpoint flush の unit のみ）
- #12: TLS 必須の検証欠如（rate limit / size limit は検証済み）

意図的スキップの内訳（いずれも負荷試験 MVP #16 のユーザー判断によるスキップ）:

- #1: Phase 1 負荷シナリオ（50 クライアント / 10 Hz / tick P99 ≤ 50 ms）
- #2: Phase 2 Interest 負荷シナリオ（200 クライアント / 可視 entity ≤ 50）
- #3: Phase 2 1000 接続分散シナリオ
- #11: レイテンシ（P50 / P95 / P99）測定

## 既知の限界

- 統合テスト（backpressure / chaos）は実 PostgreSQL またはコンテナ環境を要求し、
  無い環境では skip される。
- 意図的スキップの 4 項目は負荷試験を実施する時点で本表の更新が必要である
  （GAP のまま残すべきではないため、現時点では実施判断を明示した）。
