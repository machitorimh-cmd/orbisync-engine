# 受入条件トレーサビリティ: repo-crate-conventions.md

`docs/design/repo-crate-conventions.md` §8 の受入条件 13 項目と、それを検証する
テスト/CI 機構の対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテスト、または CI 上の機構が存在する。
  対象を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテスト・機構が存在しない。

判定は `58cc57d` 時点のコードに基づく。本表の #1〜#9 は CI ゲート（`cargo test`
以外の静的解析スクリプトや lint）を根拠とする。これらは `cargo test` のように
`cargo test` コマンド一発で実行されるものではなく、`.github/workflows/ci.yml` /
`design-validation.yml` から呼ばれる、または開発者が直接実行するスクリプトである。
`test-and-ci.md` のトレーサビリティ表に記載したとおり `ci.yml` は
`workflow_dispatch` のみが trigger であり PR に自動実行されないが、本表の各項目は
「違反を検出できる機構が存在するか」を判定基準としており、「PR で自動的に強制
されるか」は別軸のため、個々の行では繰り返さず末尾の既知の限界にまとめる。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | `domain` crate の依存グラフに Axum、SQLx、Tokio、protocol 生成物が存在しない | COVERED | `scripts/check_architecture.py`（`.github/workflows/ci.yml` の `architecture` ジョブから実行） | `cargo metadata` の依存グラフを辿り、`orbisync-domain` から到達可能な crate 集合に禁止 crate が含まれないことを検証する。実際の workspace グラフに対して実行するため、fixture ではなく現行コードそのものを検査する。 |
| 2 | `application` crate の公開 API に HTTP/DB/WebSocket 型が存在しない | COVERED | `scripts/check_architecture.py` の `ALLOWED_INTERNAL["orbisync-application"]` 制約（内部依存は `orbisync-domain` のみ）と、依存グラフに Axum/SQLx/tokio-tungstenite 等が到達不能であることの検証 | `orbisync-application` の `Cargo.toml` は HTTP/DB/WS crate に一切依存しない。Rust の型システム上、依存していない crate の型を公開 API で名指すことはコンパイル不能なため、依存グラフの不在は公開 API 不在の十分条件になる。script のコメントは「structural half of condition 2」と述べるが、依存が皆無である以上 non-structural な違反経路（型を名指すこと自体）は原理的に存在しない。 |
| 3 | crate 依存グラフに循環がない | COVERED | `scripts/check_architecture.py::find_cycle`（`architecture` ジョブ） | 通常の Cargo 依存は循環自体が cargo build 時点で拒否されるが、本 script は `dev`/`build` エッジも含めて DFS で循環検出し、Cargo が許容してしまう経路まで検査する。 |
| 4 | `world-runtime` が `interest` または `realtime` を import しない | COVERED | `scripts/check_architecture.py` の `ALLOWED_INTERNAL["orbisync-world-runtime"]`（`orbisync-domain`、`orbisync-application` のみ） | 許可リストに `orbisync-interest`／`orbisync-realtime` が含まれず、`architecture` ジョブが実グラフと突き合わせて逸脱を検出する。 |
| 5 | `interest` が `realtime_presence`、`realtime_delivery`、`realtime_gateway` を import しない | COVERED | `scripts/check_architecture.py` の `ALLOWED_INTERNAL["orbisync-interest"]`（`orbisync-domain` のみ） | 現行実装には `realtime_presence`/`realtime_delivery`/`realtime_gateway` という名前の別 crate は存在せず、単一の `orbisync-realtime` crate（内部モジュールは `connection.rs`/`gateway.rs`/`session_store.rs` 等）にまとまっている。文書の記述はより細粒度だった過去の crate 分割案を指している可能性があり、現状とは名前が一致しない。ただし script は `orbisync-interest` が `orbisync-realtime` crate 全体に一切依存しないことを検証しており、これは文書が意図する「presence/delivery/gateway のいずれにも依存しない」を包含する（crate 全体への依存が無ければ、その内部のどのモジュールにも依存し得ない）。 |
| 6 | `buf lint`、base branch の `proto/` に対する `buf breaking`、および `buf generate` 後の tracked 差分ゼロ・ignore されない untracked 生成物ゼロが CI で検証される | PARTIAL | `.github/workflows/design-validation.yml` の `buf` ジョブ（`buf lint`、および `buf generate` 後 `git diff --exit-code` と `git ls-files --others --exclude-standard` の空チェック） | `buf lint` と生成後差分ゼロの 2 つは無条件ステップとして実行される。しかし `buf breaking` を実行する `Check Protocol Buffer compatibility` ステップは `if: github.event_name == 'pull_request'` でガードされており、このジョブを呼び出す唯一の経路（`ci.yml` の `workflow_dispatch` 手動実行）では `pull_request` イベントが発生しないため、**このステップは自動実行されないだけでなく、手動でワークフローを起動しても常にスキップされる構造的に到達不能な状態**にある（`github.base_ref` も PR イベント外では空になり、ステップ内の `git cat-file` 参照も成立しない）。したがって受入条件が列挙する 3 検証のうち `buf breaking` は機能していない。 |
| 7 | 生成コードは repository に tracked file として存在せず、build または code generation のたびに正本から生成される | COVERED | `.gitignore`（`/generated/`、`sdk/typescript/src/generated/`）+ `scripts/check_no_handwritten_proto.py`（`buf` ジョブ内） | 生成物のパスが `.gitignore` で除外され、`check_no_handwritten_proto.py` が load-generator と TypeScript SDK の双方について「`build.rs`/Buf 生成型の配線があること」と「手書き wire 定義が無いこと」を検査する。#6 の `git diff --exit-code` 相当のチェックと合わせ、tracked file 化を防ぐ二重の検証になっている。 |
| 8 | `cargo fmt --check` が pass する | COVERED | `.github/workflows/ci.yml` の `static-analysis` ジョブ（`cargo fmt --all --check`） | 受入条件の文言どおりのコマンドがそのまま実行される。 |
| 9 | `cargo clippy --all-targets --all-features -- -D warnings` が pass する | COVERED | `.github/workflows/ci.yml` の `static-analysis` ジョブ | 受入条件の文言どおりのコマンドがそのまま実行される。 |
| 10 | 本番 path に `unwrap()` / `expect()` がない（clippy lint で検出） | COVERED | `Cargo.toml` の `[workspace.lints.clippy]`（`unwrap_used = "deny"`、`expect_used = "deny"`）を全 15 crate が `[lints] workspace = true` で継承し、`cargo clippy -D warnings` で強制 | 15 crate すべての `Cargo.toml` に `lints.workspace = true` があることを確認済み（grep で該当 15 件）。`Cargo.toml` のコメントに「test code re-allows `unwrap_used`/`expect_used` locally」とあり、テストコード側は個別に `#[allow(...)]` で緩和する設計であるため、lint 違反は本番 path のみを狙い撃つ。 |
| 11 | public item に rustdoc がある | COVERED | `Cargo.toml` の `[workspace.lints.rust] missing_docs = "deny"` | `cargo clippy -D warnings`（rustc lint も含む）で強制。コメントにより generated protocol code のみ個別に `missing_docs` を再許可している。 |
| 12 | `UserId`、`InstanceId` 等の newtype が定義され、異なる ID の取り違えが compile error で検出される | COVERED | `crates/orbisync-domain/src/id.rs` のモジュール doc 内 `` ```compile_fail ``` `` doctest（`kick(InstanceId::generate())` が `UserId` を要求する関数に渡され、コンパイル失敗することを直接示す）＋ 同ファイルの `#[test]` 群（`UserId`/`InstanceId` が同一 UUID からでも異なる型として扱われることを確認） | `compile_fail` doctest は `cargo test`（doc test 込み）実行時に「実際にコンパイルが失敗すること」を検証する最も直接的な形の証拠であり、受入条件の文言と完全に一致する。 |
| 13 | protocol 型と domain 型の変換が adapter 境界に閉じ込められている | COVERED | `scripts/check_architecture.py` の `ALLOWED_INTERNAL` 制約（`orbisync-domain`、`orbisync-application`、`orbisync-world-runtime`、`orbisync-identity`、`orbisync-world-directory` はいずれも `orbisync-protocol` への依存を許可されていない） | `orbisync_protocol::` を実際に参照している crate は `orbisync-realtime` と `orbisync-server` のみ（grep で確認）で、いずれも adapter/composition-root 層に該当する。domain/application/world-runtime 等のコア層は `orbisync-protocol` に依存できないため、変換コードをコア層に書くこと自体がコンパイル不能であり、境界の閉じ込めは依存グラフ制約により構造的に保証される。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 12 |
| PARTIAL | 1 |
| GAP | 0 |

PARTIAL の内訳:

- #6: `buf breaking` ステップが `if: github.event_name == 'pull_request'` ガードと現行 trigger 構成（`workflow_dispatch` のみ）の組み合わせにより構造的に到達不能。`buf lint` と生成後差分チェックは機能する

GAP の内訳: 該当なし。

## 既知の限界

- 本表の #1〜#9、#13 は「違反を検出できる CI スクリプト・lint 設定が存在し、
  実際のコードベースに対して正しく機能する」ことを根拠とする。これらのスクリプトの
  多くは `scripts/test_check_*.py` のような専用 unit test を持たない
  （`check_architecture.py` 自体に対する `test_check_architecture.py` は存在しない）。
  スクリプトのロジックそのものにバグがあった場合、それを検出する仕組みは無い。
- `test-and-ci.md` のトレーサビリティ表に記載したとおり、`.github/workflows/ci.yml`
  と `design-validation.yml` は `workflow_dispatch` のみが trigger であり、PR に対して
  自動実行されない。したがって本表の各機構は「違反を検出できる」ことと「PR ごとに
  自動的に検出される」ことは別であり、後者の保証は無い。
- #5 は文書が指す `realtime_presence`/`realtime_delivery`/`realtime_gateway` という
  crate 名が現行コードに存在しない（実装は単一の `orbisync-realtime` crate）。
  意図は満たされているが、文書と実装の命名が一致していない。
