# 受入条件トレーサビリティ: mobile-resume-interest-backpressure.md

> 下表は `31fc324` 時点の監査記録。SDKのbackoff・refresh・replay・復帰状態に関する
> 現在の対応は [client-sdk.md](client-sdk.md) と
> completion-validation (internal record omitted from this source distribution) を参照。
> 下表の過去のGAP件数を、現在のSDK未実装件数として使用しない。

`docs/design/mobile-resume-interest-backpressure.md` §10 の受入条件 21 項目と、
それを検証するテストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `31fc324` 時点のコードとテストに基づく。統合テストは
`tests/integration/tests/` 配下、crate 内 unit test は `crates/**/src/*.rs`
の `#[cfg(test)]` を指す。fake adapter と決定論的 clock の併用は §10 冒頭
が明示的に許容しているため、port が fake でも handler / use case /
domain の実装経路を通るテストは production 経路の検証として扱う。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | `active` 接続を強制切断し、指数バックオフ + jitter 後に再接続すると ResumeSession が送られ、履歴窓内の `last_revision` で差分 replay 後 `active` に復帰し、復帰後 revision は server の current と一致する | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_resume_same_presence_and_replay` | 検証済み: 切断後の再接続で同一 PresenceId・`replay_follows = true` の ResumeAccepted、切断中に作成された entity が replay (Snapshot / StateDelta) に含まれること。未検証: (a) 強制 TCP 切断（テストは `close(None)` の正常クローズを使用）、(b) 指数バックオフ + jitter 待機後の再接続（テストは即時再接続）、(c) 復帰後の instance revision が server current と一致することの直接 assert。 |
| 2 | access token 期限切れで切断した場合、client は refresh 後に再接続する。refresh 失敗時は再接続を断念し、resume を試行しない | GAP | （該当なし） | 実 Realtime 接続 + token 期限切れ + refresh → 再接続 / 断念の遷移を検証するテストは存在しない。HTTP 層の refresh 成功 / 失敗は `refresh_w_h.rs`・`w27_refresh_and_password.rs` が検証するが、Realtime 再接続との連携は未検証。 |
| 3 | バックオフ待機時間は `min(cap, base * 2^n)` に jitter され、同一時刻に集中しない | GAP | （該当なし） | client 再接続用の backoff 計算を検証するテストは存在しない。`crates/orbisync-extensions/src/delivery.rs::jitter_is_bounded_and_not_fixed_to_the_base` は同型の full jitter 公式を検証するが、extension webhook 配送の retry 用であり client 再接続は対象外。 |
| 4 | 有効な token での resume 成功後、旧 token は無効化され新しい token が発行される（単一使用・回転）。旧 token の再提示は拒否される | COVERED | `tests/integration/tests/resume_w18.rs::w18_resume_token_rotates`<br>`tests/integration/tests/resume_w18.rs::w18_single_use_token_rejected`<br>`crates/orbisync-realtime/src/session_store.rs::single_use_second_consume_fails` | 実 WS 経路で resume 成功時に新しい非空 token が発行され、同一 token の再提示が ResumeAccepted ではなく ResyncRequired で拒否されることを検証する。session store unit test が consume 後の 2 回目 consume 失敗（remove 後の単一使用保証）を直接検証する。 |
| 5 | grace 期間経過後の resume は拒否され、client は新規 Snapshot による入室へ移行する。接続は切断されない | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_expired_token_resync_required` | 検証済み: 決定論的 clock を 61 秒（grace 60 秒 + skew）進めた後の ResumeSession が ResyncRequired で拒否されること。未検証: ResyncRequired 受信後の client の新規 Snapshot 入室への遷移、および「接続は切断されない」こと（テストは ResyncRequired 受信のみで終了し、その後の接続継続は確認しない）。 |
| 6 | epoch 不一致の token 提示は拒否される | GAP | （該当なし） | `ResumeBinding.epoch` は保持されるが「Currently always 0; the store retains the field so future epoch bumps can be compared」（`session_store.rs`）のとおり比較・拒否の実装もテストも存在しない。resume 成功時に `epoch + 1` で再発行される実装はあるが、不一致検出のテストはない。 |
| 7 | ログ・メトリクス・telemetry に token の raw および先頭数文字（prefix）が含まれない。相関は鍵付き hash（HMAC）または server 側 opaque correlation ID のみ | PARTIAL | `crates/orbisync-realtime/src/gateway.rs::test_handshake_success_debug_redacts_the_ticket`<br>`tests/integration/tests/realtime_rv_a.rs::c1_audit_and_logs_contain_no_ticket_material`<br>`scripts/check_secret_logging.py`（CI `architecture` job で実行） | 検証済み: handshake 成功結果の `Debug` 出力に ticket が含まれず `<redacted>` になること、監査 rows と DB に ticket 素材が残らないこと、CI の静的検査で resume token / access token / realtime ticket の raw・prefix（slice / `[..n]`）ログ出力が禁止されていること。未検証: (a) tracing ログ出力そのものの動的 capture 検証（`c1_audit_and_logs_contain_no_ticket_material` は名称に反しログを検査しない）、(b) metric / telemetry 出力への token raw / prefix 混入防止。 |
| 8 | `last_revision < buffer.oldest` の resume は ResyncRequired + 新規 Snapshot へ移行し、欠落なく最新状態へ一致する | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_gap_resync_required`<br>`crates/orbisync-realtime/src/resume.rs::test_resync_gap_beyond_retained_requires_resync` | 検証済み: 実 WS 経路で履歴窓（256）を溢れさせる 300 件の transform 後に `last_applied = 1` で resume すると `revision_gap` / `history_unavailable` 理由の ResyncRequired が返ること、unit level で保持範囲外の revision が ResyncRequired に判定されること。未検証: ResyncRequired 後に新規 Snapshot を取得して最新状態へ欠落なく一致すること（テストは ResyncRequired 応答の受信のみで終了）。 |
| 9 | `last_revision > current` の異常な提示は ResyncRequired + Snapshot で回復する | PARTIAL | `crates/orbisync-realtime/src/resume.rs::test_resync_future_revision_requires_resync` | 検証済み: unit level で `last > current` が `ResyncReason::FutureRevision` に判定されること。未検証: 実 WS 経路で future revision を提示する統合テスト、およびその後の Snapshot による回復。統合層では gap / expiry のみが検証されている。 |
| 10 | 差分 replay は reliable イベントを順序保持・重複排除して送り、latest-wins の過去値は送らない | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_resume_same_presence_and_replay` | 検証済み: 切断中に作成された entity が replay 応答（Snapshot または StateDelta）に含まれること。未検証: (a) 複数 reliable イベントの到着順序保持、(b) replay 時の重複排除、(c) latest-wins の過去値（中間値）が replay に含まれないことの明示 assert。テストは replay 内容の存在のみを確認し順序・重複は検証していない。 |
| 11 | 書き込みを詰まらせた状態で同一 entity の `x=1.0..1.3` を投入すると、再開後に配送される latest-wins 値は最新（1.3）のみである | COVERED | `tests/integration/tests/backpressure_w17.rs::backpressure_w17_latest_wins_slow_consumer_sees_latest`<br>`crates/orbisync-realtime/src/outbound_queue.rs::test_latest_wins_overwrites_same_entity_id`<br>`crates/orbisync-realtime/src/outbound_queue.rs::test_latest_wins_same_entity_overwrites_preserves_len_and_updates_payload` | 実 WS 経路で slow consumer を 500 件の StateDelta で flood し、再開後に受信する値が最新のみであること（古い位置で assert が失敗する mutant 検出を明記）を検証する。OutboundQueue unit test が同一 entity id の上書き（len 維持・payload 更新・dropped_latest 計上）を直接検証する。 |
| 12 | 同一期間に投入した reliable イベントおよび Snapshot chunk はすべて順序保持・欠落なく配送され、1 件も silent drop されない | PARTIAL | `tests/integration/tests/backpressure_w17.rs::backpressure_w17_reliable_not_dropped_by_latest` | 検証済み: latest-wins flood 中に投入した reliable EntityCommand（spawn）が latest-wins に coalesce されず受信できること。未検証: (a) 複数 reliable イベントの順序保持、(b) Snapshot chunk（bulk lane）の順序保持・欠落なし、(c) reliable が「1 件も silent drop されない」ことの全量照合（テストは 1 件の reliable の生存のみ確認）。 |
| 13 | reliable queue が上限超過すると警告後に接続が切断され、イベントは silent drop されない。大量 Snapshot chunk（bulk lane）配信中も Control が飢餓しない | PARTIAL | `tests/integration/tests/backpressure_w17.rs::backpressure_w17_reliable_overflow_disconnects`<br>`crates/orbisync-server/src/delivery.rs::reliable_overflow_disconnects_instead_of_dropping_silently`<br>`crates/orbisync-realtime/src/outbound_queue.rs::test_control_overflow_tracks_metric` | 検証済み: 容量 16 の queue に 30 件の reliable を投入すると 17 件目で `RELIABLE_QUEUE_OVERFLOW` により切断されること（unit でも同様、sender 削除まで確認）、control overflow が metric として計上されること。未検証: (a) 大量 Snapshot chunk（bulk lane）配信中の Control（HeartbeatAck / ErrorMessage）飢餓防止、(b) 切断前の「警告」の出力検証。 |
| 14 | 任意の瞬間に 1 接続の socket へ並行 write が発生しない（計装またはテストで検証） | GAP | （該当なし） | `RealtimeSocket` が唯一の write 所有者という構造上の保証はあるが（`realtime_ws_socket.rs` の module doc）、並行 write が発生しないことを検出する計装またはテストは存在しない。条件が明示的に「計装またはテストで検証」を要求するため GAP。 |
| 15 | 1 接続を slow consumer 状態にしても、同一インスタンスの他接続の配送と `instance_runtime` の tick が遅延しない | PARTIAL | `tests/integration/tests/backpressure_w17.rs::backpressure_w17_slow_does_not_block_fast`<br>`crates/orbisync-server/src/delivery.rs::a_full_consumer_does_not_affect_a_healthy_one` | 検証済み: slow consumer が flood 中でも fast consumer が delta を受信し続けること（実 WS 経路）、満杯 consumer の存在下で healthy consumer が reliable を受信し slow 側のみ切断されること（unit）。未検証: `instance_runtime` の tick レイテンシが遅延しないことの計測。 |
| 16 | `interest` の可視集合計算は、同一入力 view に対して同一出力を返す純粋関数である。`interest` のテストに I/O adapter / socket / queue を必要としない | COVERED | `crates/orbisync-interest/tests/property.rs::spatial_grid_cell_contains_the_position`<br>`crates/orbisync-server/src/interest_filter.rs::distance_and_visibility_policy`<br>`tests/integration/tests/interest_h6.rs::h6_two_clients_50m_apart_do_not_receive_each_other_state_delta` | `UniformGrid` / `SpatialIndex` / `filter_visible_entities` は全て module doc で pure / I/O-free と明示され、対応するテスト（proptest を含む）は socket・queue・I/O adapter なしで純粋関数のみを呼び出す。`interest_h6.rs` も `InstanceActor` + grid + filter 関数のみで socket を使用しない。 |
| 17 | ヒステリシス: 30 m 以内で購読し、32 m で維持、36 m で解除、34 m で再購読せず、30 m 以下で再購読する | COVERED | `crates/orbisync-interest/src/uniform_grid.rs::test_hysteresis_subscribe_unsubscribe`<br>`crates/orbisync-server/src/realtime_ws_tests_snapshot.rs::n3_hysteresis_retains_at_31_after_29`<br>`crates/orbisync-server/src/realtime_ws_tests_snapshot.rs::n3_hysteresis_unsubscribes_at_36`<br>`crates/orbisync-server/src/realtime_ws_tests_snapshot.rs::n3_hysteresis_34_stays_culled_after_unsubscribe`<br>`tests/integration/tests/realtime_e2e.rs::e2e_hysteresis_31m_still_visible` | unit の `should_retain` テストが条件と同一の境界値（30 subscribe / 30.1 拒否 / 32 維持 / 36 解除 / 34 再購読せず / 29.9 再購読）を検証する。production 補助関数テスト（N-3）が 29 購読→31 維持→36 解除→34 culled の状態遷移を検証し、実 WS 経路の e2e が hysteresis による 31 m 維持を確認する。 |
| 18 | `OwnerOnly` entity は空間内でも owner 以外の subject に可視とならない。`Global` entity は距離に関係なく可視となる | COVERED | `tests/integration/tests/interest_h6.rs::h6_owner_only_entity_delta_not_received_by_non_owner`<br>`crates/orbisync-interest/src/uniform_grid.rs::test_visibility_global_always`<br>`crates/orbisync-server/src/interest_filter.rs::owner_only_requires_owner`<br>`crates/orbisync-server/src/interest_filter.rs::distance_and_visibility_policy` | OwnerOnly について snapshot / delta / anonymous viewer の 3 経路で非 owner 不可視を検証（2 m 配置＝空間内）。Global について遠距離（10,000 m）・ subscribed / not subscribed 両状態で常に可視を検証し、interest_filter の結合テストでも far Global が可視に留まることを確認する。 |
| 19 | HeartbeatAck を停止すると連続失敗後に client は切断扱いで再接続を開始する。server は最終受信から timeout 後に `closed` へ遷移させ、Presence は grace 期間保持される | PARTIAL | `crates/orbisync-realtime/src/heartbeat.rs::test_is_timed_out_after_timeout`<br>`crates/orbisync-realtime/src/heartbeat.rs::test_next_state_on_timeout_maps_to_closed`<br>`crates/orbisync-realtime/src/session_store.rs::disconnected_binding_is_retained_during_grace_then_pruned` | 検証済み: server 側で最終 activity + timeout 経過後に `HeartbeatTimeout` が発生し、Ready / Joining / Active / Resuming の全状態から `Closed` へ遷移すること（unit）、切断された binding が grace 中は保持され期限後に掃除されること（session store unit）。未検証: client 側の連続 HeartbeatAck 失敗 → 切断扱い → 再接続開始（このリポジトリに client 実装のテストは存在しない）。 |
| 20 | 接続単位 rate limit を超えた inbound は拒否 / 切断され、インスタンス単位総入力上限は `instance_runtime` の tick を保護する | PARTIAL | `crates/orbisync-realtime/src/rate_limit.rs::test_rate_limiter_persistent_maps_to_connection_event`<br>`crates/orbisync-world-runtime/src/actor.rs::component_rate_limit_rejects_without_disconnect_and_records_metric` | 検証済み: 接続単位 limiter が threshold 超過で `PersistentRateLimit` event（→ `Failing`、切断経路）に map されること、instance actor の component update limiter が超過分を拒否しつつ切断せず metric を記録すること（actor 経路＝tick 経路での拒否）。未検証: tick 保護の効果の計測（limiter が tick の遅延を実際に防ぐことのレイテンシ検証は行っていない）。 |
| 21 | Resume Token 単独では認証できず、新接続には有効な Access Token から発行した単一使用 Realtime 接続 ticket が必須である。`AuthSessionId` は Realtime 接続キーとして使用されない | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_resume_requires_ticket`<br>`tests/integration/tests/resume_w18.rs::w18_other_users_token_rejected`<br>`tests/integration/tests/realtime_rv_a.rs::c1_same_ticket_two_ws_only_one_succeeds`<br>`tests/integration/tests/realtime_rv_a.rs::c1_revoked_session_rejects_unused_ticket` | 検証済み: 無効 ticket + 有効 resume token が AUTHENTICATION_REQUIRED で拒否されること（resume が ticket 検証をバイパスしない）、他ユーザーの token 再使用が拒否されること、ticket が単一使用（同一 ticket の 2 接続目は失敗）であること、session 失効時に未使用 ticket が拒否されること。未検証: `AuthSessionId` が Realtime 接続キーとして使用されないことの直接テスト（接続キーは `RealtimeConnectionId` / `PresenceId` で運用されているが、誤用防止の検証テストはない）。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 5 |
| PARTIAL | 12 |
| GAP | 4 |

PARTIAL の内訳:

- #1: 強制切断・backoff 待機・復帰後 revision 一致 assert の欠如
- #5: ResyncRequired 後の Snapshot 入室遷移と接続継続の検証の欠如
- #7: tracing / metric / telemetry 出力そのものの動的 capture 検証の欠如
- #8: ResyncRequired 後の新規 Snapshot による最新状態収束の検証の欠如
- #9: future revision の統合経路テストと Snapshot 回復の検証の欠如
- #10: replay の順序保持・重複排除・latest-wins 過去値除外の明示検証の欠如
- #12: Snapshot chunk 順序保持・全量無欠落の検証の欠如
- #13: bulk lane 中の Control 飢餓防止と切断前警告の検証の欠如
- #15: `instance_runtime` tick 遅延の計測の欠如
- #19: client 側 heartbeat 失敗 → 再接続の検証の欠如
- #20: tick 保護効果のレイテンシ計測の欠如
- #21: `AuthSessionId` 非使用の直接テストの欠如

GAP の内訳:

- #2: token 期限切れ時の refresh → 再接続 / 断念フロー（Realtime 連携）
- #3: client 再接続 backoff の `min(cap, base * 2^n)` + jitter（extension 配送用の同型テストはあるが対象外）
- #6: epoch 不一致 token の拒否（実装自体が未着手、フィールド保持のみ）
- #14: 単一 writer の計装 / テスト検証（構造上の保証のみでテストなし）

## 既知の限界

- `tests/integration/tests/` の多くは `common::pool_or_skip()` で実 PostgreSQL
  を要求する。DB が無い環境ではテストが skip され、本表の判定が意味する
  実行時保証は CI / 検証環境の DB 有無に依存する。
- client 側の再接続・backoff・refresh ロジックは server リポジトリのテスト
  対象外であり、#2・#3・#19 の client 部分は SDK 側の検証に依存する。
- 実装の変更（特に `session_store` の epoch 比較導入、`resume.rs` の再同期
  判定、`OutboundQueue` の lane 優先度）は本表の対応を壊す。設計文書の
  受入条件を変更した場合は本表を同期すること。
