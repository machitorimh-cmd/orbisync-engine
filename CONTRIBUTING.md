# Contributing to OrbiSync

OrbiSyncへの貢献に関心を持っていただき、ありがとうございます。

Rust workspace、認証、REST API、realtime protocol、TypeScript SDKと参照アプリを含む開発中のプロジェクトです。現在の機能と制約は[README](README.md)を参照してください。

## 開発環境の構築

```shell
git clone <repository-url> orbisync
cd orbisync
cp deploy/compose/.env.example .env
bash scripts/gen-dev-password-denylist.sh
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d postgres
set -a && . ./.env && set +a
cargo run -p orbisync-server -- migrate
cargo run -p orbisync-server -- --password-denylist deploy/dev-password-denylist.txt
```

`scripts/bootstrap-dev.sh` が上記と検査コマンドをまとめて実行します。必要なツール、設定の優先順位、endpointの詳細は [README.md](README.md#ローカル開発) を参照してください。

`protoc` のインストールは不要です。`.proto` はbuild時にpure Rust compilerで生成し、生成コードはcommitしません（ADR-004）。生成コードを手で編集しないでください。

## Pull Request前の検査

PR CIと同じ内容をローカルで実行できます。

```shell
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
python scripts/check_architecture.py
python scripts/validate_design.py --review
python -m unittest scripts/test_validate_design.py
```

`scripts/check_architecture.py` は `docs/design/repo-crate-conventions.md` §3.2 のAllowed Dependency Matrix、依存グラフの循環、`domain`/`application`/`interest` へのフレームワーク混入、`testkit` のdev-dependency限定を検査します。crateを追加した場合は同スクリプトの行列も更新してください。

## Merge条件（branch protection）

`test-and-ci.md` §4.5 受入条件12は、PR CIの必須項目がすべてpassしないPRをmergeできないことを求めます。リポジトリ管理者は、`main` のbranch protectionで次のstatus checkをrequiredに設定してください。

| status check | workflow |
|---|---|
| `PR CI` | `.github/workflows/ci.yml`（design / buf を含む全jobの集約） |

`PR CI` job は static analysis、build/unit test、依存規則、supply chain、OpenAPI、design validation、buf、PostgreSQL 16/17 integration の各jobを集約し、失敗・cancel・skip のいずれかがあれば失敗します。この設定はリポジトリ設定であり、リポジトリ内のファイルでは強制できません。

## 行動原則

- 相手ではなく、提案・設計・コードの内容を議論する
- 用途、経験、言語、所属にかかわらず敬意を持って協力する
- セキュリティ問題や個人情報を公開の場へ書かない
- 意思決定の根拠、代替案、trade-offを記録する

正式なCode of Conductは公開準備の一環として別途追加します。

## 貢献を始める前に

1. 既存のIssue、ADR、設計文書を検索する
2. 小さな誤記修正を除き、IssueまたはRFCで目的と範囲を共有する
3. 関連する原仕様と設計文書を確認する
4. 大きな判断変更は、実装より先にADRを提案する
5. セキュリティ問題は公開Issueを使わず、[SECURITY.md](SECURITY.md)に従う

設計文書の索引は [`docs/design/roadmap-and-traceability.md`](docs/design/roadmap-and-traceability.md) にあります。

## 変更の分類

### 仕様由来の確定事項

`metaverse_core_specification.md`で要求・禁止されている事項です。設計文書では原則として `[SPEC]` と、根拠となる仕様節を併記します。

### 設計上の推奨

要求を実装へ落とす設計案です。設計文書では `[REC]` とします。原仕様が推奨・候補・例として示す事項を、根拠なく確定事項へ格上げしないでください。

### 未決事項

複数案が成立する判断は `[ADR]` とします。AcceptedとなったADRだけが有効な決定です。ADRは `docs/adr/` に配置し、Context、Decision、Alternatives、Consequences、Migrationを記載してください。

## Pull Request

1つのPull Requestでは、レビュー可能な1つの目的に集中してください。

本文には次を記載します。

- 何を、なぜ変更したか
- 関連IssueまたはRFC
- 影響する仕様節、設計文書、module、公開契約
- 実施した検証と結果
- 互換性、security、performance、operationへの影響
- 未解決事項、残存リスク、後続作業

[PRテンプレート](.github/PULL_REQUEST_TEMPLATE.md)の全項目を確認し、該当しない項目には理由を記載してください。

## 文書変更の確認

文書を変更する場合、最低限次を確認してください。

- 仕様§1〜§47のcoverageに欠落・重複を作っていない
- `[SPEC]`には直接の仕様根拠がある
- 推奨、例、候補、将来事項を確定扱いしていない
- module名、ID名、所有権、依存方向が他文書と一致する
- Markdown heading、table、code fence、相対linkが壊れていない
- topology、support matrix、未Accepted ADRを勝手に確定していない

## 将来の実装変更

実装開始後は、変更に応じて次を追加・更新してください。

- Unit test
- 実PostgreSQLを使用するIntegration test
- Protocol compatibility testとtest vector
- migration test
- security test
- load/soak/chaos test
- OpenAPI、`.proto`、SDK生成物
- metrics、logs、traces、audit
- CHANGELOGとmigration notes

生成コードは直接編集せず、正規の生成手順を使用します。本番pathでsecretを出力せず、内部errorを公開contractへ漏らさないでください。

## Commit

- 命令形で変更内容が分かる短いsubjectを使用する
- 無関係なformat変更や生成物を混ぜない
- secret、credential、個人情報、実運用データをcommitしない
- review中のfixupを最終履歴へ残すかは、merge方式が確定した後の運用に従う

## License

OrbiSyncは `MIT OR Apache-2.0` のデュアルライセンスです。

別途明示しない限り、意図的に提出したcontributionは、利用者が次のいずれかを選択できる条件で提供されることに同意したものとして扱われます。

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

CLA/DCOの要否はまだ未決です。現時点で、未導入のCLAやDCOへの同意を要求しません。方針を変更する場合はAccepted ADRと本書の更新を先に行います。
