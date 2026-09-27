# 受入条件トレーサビリティ: test-and-ci.md

`docs/design/test-and-ci.md` §4 の受入条件 14 項目と、それを検証するテスト/CI 機構の
対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテスト、または CI 上の機構が存在する。
  対象を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテスト・機構が存在しない。

判定は `58cc57d` 時点のコードに基づく。`docs/design/test-and-ci.md` §4 の前文には
fake adapter 許容の明示がないため、実 DB を要求する項目（#2 等）は文言どおりに
「実 PostgreSQL」を要求する経路のみ COVERED とした。

## CI トリガーに関する前提（複数項目に影響）

`.github/workflows/ci.yml` と `dependency-audit.yml` はいずれも `on: workflow_dispatch`
のみで、`pull_request` / `push` トリガーが存在しない（コメントに「GitHub Actions
無料枠の分数節約のため意図的に省略」とある）。つまり CI は PR に対して自動実行されない。
さらに本リポジトリは private かつ無料枠のため、branch protection API は
`403 Upgrade to GitHub Pro or make this repository public` を返す
（`gh api repos/.../branches/main/protection` で確認済み）。この 2 点により、
「CI が pass しない PR は merge できない」という主張はワークフロー定義上は
成立する経路（`pr-ci` ジョブが必須ジョブの失敗/cancel/skip で fail する）を
持つものの、それを GitHub 側で強制する仕組み（自動トリガー + 必須ステータス
チェック）が現状の設定からは確認できない。この事実は #12 に直接影響し、#13
（release CI）・#14（脆弱性ゲート）にも同様の留保がかかる。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | unit test が I/O なしで実行され、全テストの合計が 5 分以内で完了する | GAP | (なし) | `ci.yml` の `build-and-test` ジョブは `cargo test --workspace --all-features` を実行するが `timeout-minutes` を設定しておらず、5 分以内という制約を検証・強制する仕組みがない。unit test が I/O を行わないことを検出する lint・CI チェックも存在しない（`architecture` ジョブの `check_architecture.py` は crate 依存関係のみを検査し、個々の unit test の I/O 有無は見ない）。 |
| 2 | integration test が実 PostgreSQL（Testcontainers）で実行され、login → join → state update → leave のフローが正常に完了する | GAP | (該当なし。近い候補: `tests/integration/tests/realtime_e2e.rs` の `e2e_two_clients_join_and_receive_snapshot` 等) | `realtime_e2e.rs` は handshake → join → transform（state update）→ disconnect（leave）の一連を検証するが `FakeWorldDirectoryStore` 等の in-memory adapter のみを使い、`common::pool_or_skip()` を呼ばない（`grep` で確認: 同ファイルに `PgPool` の参照なし）。逆に実 PostgreSQL を使う WS テスト（`realtime_rv_a.rs`）は ticket 消費・handshake timeout 等の断片検証のみで、join 後の state update から leave までの一連は扱わない。`resume_w18.rs` も同様に in-memory。したがって「実 PostgreSQL 上で login→join→state update→leave が完了する」ことを単体で示すテストは存在しない。補足: CI の `integration` ジョブは `testcontainers` crate ではなく GitHub Actions の `services: postgres:` を使っており、設計文書が名指す実装手段（Testcontainers、TC-01 未確定）とも異なる。 |
| 3 | reconnect → resume の integration test が、差分 replay 後に revision 一致で完了する | PARTIAL | `tests/integration/tests/resume_w18.rs::w18_resume_same_presence_and_replay` | 実 WS 経路で A が切断中に B が作成した entity を、A の再接続後の replay（Snapshot/StateDelta）が含むことを検証する（`resumed_presence == presence_a` かつ `replay_follows == true`）。未検証: replay 後のクライアント側 revision が サーバー revision と数値的に一致することの明示 assert（テストは entity 内容の包含のみを確認し、revision フィールドの等価性は assert しない）。in-memory adapter 使用（実 PostgreSQL ではない）。 |
| 4 | `sdk/test-vectors/protocol/` の golden binary を新サーバーで decode できる | GAP | `crates/orbisync-protocol/tests/contract_vectors.rs::test_envelope_valid_vector_round_trips` | ディレクトリパス自体が設計文書と異なる（実体は `test-vectors/protocol/v1/`、`sdk/` 配下ではない）。より本質的に、`contract_vectors.rs` 冒頭のコメントが「Binary golden files arrive with the realtime implementation in Milestone 2; the JSON vector is the contract that exists today」と明記しており、golden **binary** は未着手であることを実装コード自身が認めている。現行テストは JSON 記述の値から Rust 側で `Envelope` を組み立てて encode するのであって、他 SDK 等が生成した既存バイナリを decode するテストではない。 |
| 5 | `.proto` の reserved field number が再利用されていない（lint で検出） | GAP | (なし) | `proto/orbisync/v1/*.proto` に protobuf の `reserved` キーワード宣言は 1 つも無く（コメントで「Field numbers 1-19 are reserved」と述べるのみ）、単一ファイル内の自己矛盾チェック（lint）でこの制約を検出する経路が無い。唯一の検出手段である `buf breaking`（base branch との比較）は `design-validation.yml` の `Check Protocol Buffer compatibility` ステップが `if: github.event_name == 'pull_request'` でガードされているが、これを呼び出す経路は `ci.yml` の `workflow_dispatch`（手動実行）のみであり、`pull_request` イベントが発生することがないため**このステップは構造的に到達不能**（単に自動実行されないのではなく、手動実行しても常にスキップされる）。`buf lint`（`STANDARD` ルールセット）は単一スナップショットの検証であり、削除済みフィールド番号の再利用のような履歴比較を要する違反は検出しない。 |
| 6 | encode → decode → encode の round trip が一致する | COVERED | `crates/orbisync-protocol/tests/contract_vectors.rs::test_envelope_valid_vector_round_trips`<br>`crates/orbisync-protocol/tests/contract_vectors.rs::test_unknown_fields_are_tolerated_on_decode` | `encoded = envelope.encode_to_vec()` → `decode` → 元の値と一致 → 再 `encode_to_vec()` がバイト列一致することを直接 assert する。 |
| 7 | Phase 1 load test（50 クライアント、10 Hz）が合格基準（`scale-and-nfr.md` §9.2）を満たす | GAP | (なし) | `apps/load-generator/src/main.rs` の `#[test]` 群（2009 行以降）は percentile 計算等の内部ヘルパー関数のみを検証する unit test であり、実サーバーに対して 50 クライアント・10Hz 相当の負荷を実際にかけて `scale-and-nfr.md` §9.2 の合格基準と突き合わせる自動テスト・CI ジョブは存在しない。`ci.yml` の `load-generator` ジョブは `cargo build` と `cargo test`（上記 unit test）のみを実行し、実負荷を生成しない。 |
| 8 | 24 時間 soak test で RSS memory 増加率が 5% 以内である | GAP | `tests/soak/test_soak.py`（`soak.evaluate_rss` 等のロジック単体テスト） | `tests/soak/soak.py` は 24h soak 実行スクリプトとして存在し、`evaluate_rss`（5% 閾値判定）のロジック自体は `test_soak.py` の unit test で検証されている。しかし実際に 24 時間（またはそれに近い時間）動かして RSS を測定・判定する CI ジョブはどのワークフローにも存在しない（`grep` で `soak.py` の呼び出しが workflow ファイルに見つからない）。ロジックのテストはあるが、受入条件が要求する「24 時間 soak test の実行と合格判定」自体は行われていない。 |
| 9 | DB 一時停止中にリアルタイム一時状態の処理が継続し、DB 復旧後に永続化が再開する | PARTIAL | `tests/integration/tests/chaos_e4.rs::chaos_db_stop_and_restart_restores_readiness` | 検証済み: `docker stop` で DB を止めると `PgHealthProbe` の readiness が false になり、`docker start` で復旧後に true に戻ること。未検証: DB 停止中も realtime の一時状態処理（tick、delivery 等）が継続することの直接検証、および復旧後に永続化（checkpoint 等の DB 書き込み）が実際に再開することの検証（このテストは readiness probe の状態遷移のみを見る）。加えて `CHAOS_POSTGRES_CONTAINER` 環境変数が未設定だと即座に SKIP され、どの CI ワークフローにもこの変数の設定は見当たらないため、CI 上では常にスキップされている可能性が高い。 |
| 10 | SIGTERM 後に graceful shutdown が完了し、active connection が drain される | GAP | `crates/orbisync-server/src/shutdown.rs::readiness_flag_and_connection_notices_are_ordered` | 上記 unit test は `ShutdownState` 構造体（readiness flag と broadcast channel）の状態遷移のみを検証し、実プロセスも実ソケットも介さない。`sdk/typescript/src/client.e2e.test.ts` は `helper.proc.kill("SIGTERM")` を呼ぶが、これはテスト後片付けのプロセス終了であり、drain の完了や active connection の扱いを assert しない。SIGTERM を実際に送って graceful shutdown の完了と接続 drain を検証する統合テストは存在しない。 |
| 11 | 前バージョンの DB schema に新 migration を適用し、データが正しく移行される | COVERED | `tests/integration/tests/migrations.rs::test_audit_source_ip_split_preserves_data_and_privileges` | 旧 migration（version 3）のみを適用したスキーマに legacy 形式のデータを insert し、新 migration（version 13、source_ip 分離）を適用した後、データが新テーブルに正しく移行されていること（`source_ip` の値が保持されること）を実 PostgreSQL 上で検証する。補足: 検証対象は v3→v13 の 1 組の migration ペアのみで、他の migration 全般についての一般的な保証ではない。 |
| 12 | PR CI の全必須項目（§3.1 の 10 項目）が pass しない PR は merge できない | GAP | `.github/workflows/ci.yml` の `pr-ci` ジョブ | `pr-ci` は 12 個の必須ジョブのいずれかが `failure`/`cancelled`/`skipped` であれば明示的に fail する設計にはなっている。しかし前掲の「CI トリガーに関する前提」のとおり、`ci.yml` は `workflow_dispatch` のみで PR に対して自動実行されず、かつ本 repository では branch protection（必須ステータスチェック）の設定自体が GitHub API レベルで利用不可（無料枠 + private）と確認できる。したがって「pass しない PR は merge できない」を GitHub 側で強制する経路が現状の設定から確認できず、ワークフロー定義のロジックを検証するテストも存在しない。 |
| 13 | release CI の load/chaos/soak test が合格基準を満たさない場合、release がブロックされる | GAP | (なし) | `.github/workflows/` には `ci.yml`・`dependency-audit.yml`・`design-validation.yml` の 3 つのみが存在し、release 専用の CI ワークフローが存在しない。load/chaos/soak の合格基準で release をブロックする機構自体が実装されていない。 |
| 14 | Critical/High の脆弱性が検出された PR は merge できない | PARTIAL | `.github/workflows/ci.yml` の `supply-chain` ジョブ（`cargo-deny-action` の `check licenses advisories bans sources`） | `deny.toml` は `yanked = "deny"` かつ advisory の severity フィルタを設定していないため、cargo-deny のデフォルト動作により yanked/vulnerable な crate は severity に関わらず（Critical/High 限定ではなくすべて）fail 扱いになる。ロジックとしては受入条件を上回る厳しさで機能する設計だが、#12 と同じ理由（`ci.yml` が `workflow_dispatch` のみで PR に自動実行されない、branch protection が利用不可）により、実際に「検出された PR が merge できない」という強制が働くことを示すテスト・確認はできない。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 2 |
| PARTIAL | 3 |
| GAP | 9 |

PARTIAL の内訳:

- #3: replay 後のクライアント側 revision とサーバー revision の数値一致の明示検証が無い（entity 内容の包含確認のみ）
- #9: DB 停止中の realtime 処理継続、および復旧後の永続化再開そのものの検証が無い（readiness probe の状態遷移のみ）。CI での実行有無も未確認
- #14: cargo-deny によるロジック自体は存在するが、CI トリガー/branch protection の欠如により実際の merge ブロックが機能する保証がない

GAP の内訳:

- #1: I/O なし・5 分以内の制約を検証する仕組みが無い
- #2: 実 PostgreSQL 上での login→join→state update→leave 一連を検証するテストが無い（WS フルフローは fake adapter、実 PG テストは断片のみ）
- #4: golden **binary** decode は実装コード自身のコメントが「Milestone 2 で導入予定」と認める未着手機能。現行テストは JSON から構築した値の round trip に過ぎない
- #5: `reserved` キーワード宣言が無く単一ファイル lint では検出できず、唯一の検出手段 `buf breaking` は `if: github.event_name == 'pull_request'` ガードにより現行の trigger 構成（`workflow_dispatch` のみ）では常にスキップされ構造的に到達不能
- #7: Phase 1 load test を実サーバーに対して実行し合格基準と突き合わせる自動化が無い
- #8: 24h soak 実行と合否判定を行う CI ジョブが無い（判定ロジックの unit test のみ存在）
- #10: SIGTERM 後の graceful shutdown 完了と接続 drain を検証する統合テストが無い
- #12: PR CI 必須ジョブ失敗時に merge をブロックする GitHub 側の強制経路（自動トリガー + branch protection）が確認できない
- #13: release CI 自体が存在しない

## 既知の限界

- `tests/integration/tests/` の多くは `common::pool_or_skip()` で実 PostgreSQL を要求する。DB が無い環境ではテストが skip され、本表の判定が意味する実行時保証は CI/検証環境の DB 有無に依存する。
- CI ワークフローの `workflow_dispatch` 限定という運用上の制約は、多くの項目（特に #5、#12、#13、#14）の「CI で強制される」という主張の実効性に横断的に影響する。これはコードのバグではなく GitHub Actions 無料枠を節約するための意図的な設定（コメントに明記）だが、受入条件の文言（「merge できない」「ブロックされる」）と現状の運用は一致しない。
