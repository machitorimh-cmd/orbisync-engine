# 受入条件トレーサビリティ: release-maintenance-license.md

`docs/design/release-maintenance-license.md` §4 の受入条件 11 項目と、それを
検証するテストの対応表。判定基準は次のとおり。

- `COVERED` - その項目を実際に検証しているテストが存在する。
  テスト本体を読み、受入条件が主張する振る舞いを検証していることを確認済み。
- `PARTIAL` - 一部のみ検証。未検証の部分を備考に具体化して記載する。
- `GAP` - 検証するテストが存在しない。

判定は `31fc324` 時点のコードとテストに基づく。本書の受入条件は CI 設定・
リポジトリ構成・リリースプロセスが主対象であり、Rust テストで検証できる
項目が少ない。条件自体が CI ワークフローの実行を要求する場合（例:
`cargo deny check licenses` が CI で実行される）は、そのワークフロー定義を
証拠として扱う。リポジトリの静的状態（ファイルの存在・内容一致）を要求する
条件で自動検証テストが無い場合は、静的確認の結果と検証の欠如を備考に書く。

| # | 受入条件（要約） | 状態 | 証明するテスト | 備考 |
|---|---|---|---|---|
| 1 | release CI が semver に準拠した tag を検証し、不正な tag を拒否する | GAP | （該当なし） | release 用ワークフロー（tag 検証 job）が存在しない。`.github/workflows/` 配下は `ci.yml`（手動 trigger）・`dependency-audit.yml`・`design-validation.yml` のみで、tag 形式を検証する step も script も存在しない。 |
| 2 | protocol の破壊的変更が MINOR/PATCH リリースに含まれない（CI の breaking change 検出） | GAP | （該当なし） | 破壊的変更を検出して MINOR bump を強制する CI job・script・テストは存在しない。`design-validation.yml` は設計文書と contract の整合を検証するが、semver との対応付けは行わない。 |
| 3 | 非推奨機能の使用時に warning ログが出力され、telemetry で利用回数が集計される | GAP | （該当なし） | deprecation warning の出力と telemetry 集計を検証するテストは存在しない。非推奨機能のカタログ自体も未整備（RL-02 は ADR 待ち）。 |
| 4 | release artifact に SBOM、checksum、migration notes が含まれる | GAP | （該当なし） | release artifact を生成・検証するワークフローが存在せず、SBOM 生成（`cargo cyclonedx` 等）・checksum・migration notes の組み込みを確認するテストもない。 |
| 5 | OCI image のタグが固定されており、`latest` が本番例で使用されない | PARTIAL | （該当なし・静的確認） | 自動検証テストは存在しない。静的確認では、`deploy/docker/Dockerfile` が base image を digest 固定（`rust:1.95-slim-bookworm@sha256:...` / `debian:bookworm-slim@sha256:...`）、`deploy/compose/compose.dev.yml` が postgres を digest 固定、`.github/workflows/ci.yml` が action と postgres を digest 固定しており、本番構成例に `latest` タグは使用されていない。固定値の維持を検証するテスト・CI 検査はないため PARTIAL。 |
| 6 | 必須ドキュメント（§2.1 の 10 項目）がリポジトリに存在する | PARTIAL | （該当なし・静的確認） | 自動検証テストは存在しない。静的確認では README.md / CONTRIBUTING.md / SECURITY.md / CHANGELOG.md / LICENSE-MIT + LICENSE-APACHE / docs/design/roadmap-and-traceability.md / docs/adr/ は存在するが、CODE_OF_CONDUCT.md / GOVERNANCE.md / MAINTAINERS.md の 3 件が欠落している（7/10）。 |
| 7 | CODEOWNERS で高リスク領域（proto、migrations、auth、realtime、workflows、SECURITY.md）の owner 承認が必須となっている | GAP | （該当なし） | CODEOWNERS ファイルがリポジトリに存在しない。owner 承認の強制を検証するテストも存在しない。 |
| 8 | セキュリティ報告が公開 Issue ではなく非公開経路で受け付けられる | PARTIAL | （該当なし・静的確認） | 自動検証テストは存在しない。静的確認では SECURITY.md が存在し非公開報告方法（GitHub Security Advisory）を記載している。報告経路が実際に機能すること（advisory の受付設定等）はリポジトリ外の設定に依存し、テストでは検証できないため PARTIAL。 |
| 9 | 仕様 §35.5 の RFC/ADR 必須変更が、RFC/ADR なしで merge できない（CODEOWNERS + PR テンプレートで強制） | PARTIAL | （該当なし・静的確認） | 自動検証テストは存在しない。静的確認では `.github/PULL_REQUEST_TEMPLATE.md` が存在するが、CODEOWNERS が無いため owner 承認による強制が機能しない。テンプレート単独では merge 阻止力を持たないため、条件の「merge できない」強制は未達成。 |
| 10 | `cargo deny check licenses` が CI で実行され、非互換ライセンスが検出される | COVERED | `.github/workflows/ci.yml`（`supply-chain` job: `cargo deny check licenses advisories bans sources`）<br>`deny.toml` `[licenses]` allowlist | 受入条件自体が CI での実行を要求しており、`ci.yml` の `supply-chain` job が `EmbarkStudios/cargo-deny-action` で `check licenses advisories bans sources` を実行する。`deny.toml` の `[licenses]` は `MIT OR Apache-2.0`（RL-05）と互換なライセンスのみを allow し、confidence-threshold 0.9 を設定する。allowlist 外のライセンスで job が失敗する構成になっている。Rust ユニットテストではなく CI 構成による証明である点は留意。 |
| 11 | `LICENSE-MIT` と `LICENSE-APACHE` が存在し、package metadata と CONTRIBUTING のライセンス表記が `MIT OR Apache-2.0` で一致する | PARTIAL | （該当なし・静的確認） | 自動検証テストは存在しない。静的確認では両 LICENSE ファイルが存在し、workspace `Cargo.toml` の `license = "MIT OR Apache-2.0"` と CONTRIBUTING.md の「OrbiSyncは `MIT OR Apache-2.0` のデュアルライセンスです」および両ファイルへのリンクが一致する。三者の一致を維持するチェック（テスト・CI・script）は存在しないため、将来の表記揺れを検出できない。 |

## 集計

| 状態 | 件数 |
|---|---|
| COVERED | 1 |
| PARTIAL | 5 |
| GAP | 5 |

PARTIAL の内訳:

- #5: digest 固定・`latest` 不使用は静的に確認できるが、固定値維持の自動検証の欠如
- #6: 必須ドキュメント 10 項目のうち 3 件（CODE_OF_CONDUCT / GOVERNANCE / MAINTAINERS）が欠落
- #8: SECURITY.md の存在と記載は確認できるが、非公開経路の実機能はテスト対象外
- #9: PR テンプレートは存在するが CODEOWNERS が無く merge 強制が機能しない
- #11: LICENSE ファイル・metadata・CONTRIBUTING は一致するが、一致維持のチェックの欠如

GAP の内訳:

- #1: release CI による semver tag 検証（release ワークフロー自体が未整備）
- #2: breaking change 検出と MINOR/PATCH 強制
- #3: deprecation warning と telemetry 集計
- #4: release artifact への SBOM / checksum / migration notes 組み込み
- #7: CODEOWNERS による高リスク領域の owner 承認必須化（ファイル自体が未整備）

## 既知の限界

- 本書の受入条件は CI 設定・リポジトリ構成・運用プロセスが主対象であり、
  Rust テストで検証できる項目が少ない。#10 の COVERED は CI ワークフロー
  定義による証明であり、テスト実行による保証ではない。
- #1〜#4 は release プロセス未整備に起因する GAP であり、release
  ワークフローの整備と同時に本表の更新が必要である。
- #5・#6・#8・#9・#11 の静的確認は `31fc324` 時点のリポジトリ状態に基づく。
  ファイルの追加・削除があった場合は本表を同期すること。
