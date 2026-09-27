# OrbiSync

Browser setup: run `cargo run -p orbisync-server -- web-admin`. See the
[engine Web setup quickstart](apps/admin-web/README.md) for first-admin login,
restart, and existing-installation instructions. UI assets are embedded; consumers
do not need Node.


<p align="center">
  <img src="icon/OrbiSync.png" alt="OrbiSync logo — Realtime. Together." width="360">
</p>

OrbiSyncは、複数の認証済みクライアントが同じワールドへ参加し、ユーザー、汎用エンティティ、位置・向き・状態、イベントをリアルタイムに共有するための、ヘッドレスなオープンソースバックエンドです。

用途固有のUI、レンダリング、アセット、物理エンジンへ依存しないCoreと、公開REST API・リアルタイムプロトコル・SDK境界を提供することを目標としています。

> [!IMPORTANT]
> 現在のOrbiSyncは開発途上です。管理者の作成、ユーザー作成、ログイン、realtime ticket発行、WebSocket参加、Transform同期と検証、snapshot/delta配信、Interest Managementまでの経路は動作します。Entityの生成・更新・削除も`EntityCommand`から利用できます。切断からのResumeと送信集約（backpressure）も動作します。ロール管理（一覧・作成・取得・PATCH更新・削除・ユーザーへの割り当て）、ユーザーの有効化・無効化・PATCH更新・import、パスワードリセット、ログアウト、World/Instance管理（一覧・作成・参照・更新・archive・start・stop・kick・members）、監査ログの参照、`/metrics`もHTTPの経路へ接続済みです。`openapi/orbisync-v1.yaml` は実際にサーバーが応答する37 path / 46 operationsのみを宣言する真実の契約です（`scripts/validate_openapi_routes.py` が実Routerから導出した集合と照合します）。未配線だった設計はすべて公開契約へ取り込まれ、`openapi/orbisync-v1-planned.yaml` は0 path / 0 operationsになりました。公開container imageも提供していません。本番環境では使用できません。
>
> 機能ごとの内訳は[開発状況](#開発状況)を参照してください。

## 目標

- 認証済み利用者によるリアルタイムなWorld Instance参加
- server-authoritativeなEntity所有権と状態更新
- モバイル回線の切断・遅延を考慮した再接続とResume
- Interest Managementと接続単位のbackpressure
- PostgreSQLによる永続化
- 公開REST API、Protocol Buffers、TypeScript SDK
- 単一バイナリとDocker Composeによる小規模セルフホスト
- 用途固有機能をCoreへ埋め込まない拡張境界

## Coreの対象外

次の機能はOrbiSync Coreの責務ではありません。

- 3Dレンダリング、UI、アセット配信
- 音声・映像配信
- 物理エンジン、高度なゲームロジック
- 決済、NFT、SNS
- ワールドエディタ
- 製品固有の業務フロー

これらは公開API、公開プロトコル、SDK、Extension境界を通じて外部実装と連携します。

## ゲームに限らない汎用基盤としての位置づけ

OrbiSync Coreはゲーム専用の基盤ではありません。用途固有のUI・アセット・ルールをCoreへ埋め込まない設計方針（上記「Coreの対象外」）は、LLMがフロントエンド（画面）とアプリ固有のデータ形状・ルールを実装するだけでオンラインアプリを構築できることを意図しています。例えば、複数人によるリアルタイム共同編集ツールや、外部の飛行力学エンジンを用いるドローン・シミュレーター（OrbiSync自体は物理エンジンを内蔵しません）も想定用途です。

実装の構成は[アーキテクチャ](docs/design/architecture.md)、クライアントの組み込み方は[フロントエンド連携ガイド](docs/guides/frontend-integration.md)を参照してください。用途ごとの運用条件や性能は、実際の構成で確認してください。

## 現在の内容

```text
.
├─ metaverse_core_specification.md  # 原仕様
├─ Cargo.toml                       # Rust workspace
├─ crates/                          # workspace member crates
├─ sdk/typescript/                  # 公式TypeScript SDK
├─ apps/                            # admin-web・reference-web・load-generator
├─ examples/                        # 最小クライアント実装
├─ migrations/                      # PostgreSQL migration（sqlx migrate）
├─ tests/integration/               # workspace横断のintegration test
├─ deploy/                          # Dockerfile・Docker Compose開発環境
├─ scripts/                         # 設計検査・依存規則検査・開発支援
├─ docs/
│  ├─ design/                       # アーキテクチャ・領域別設計
│  ├─ operations/                   # 復旧・変更Runbook
│  ├─ guides/                       # クライアント・Extension連携ガイド
│  ├─ consumer-kit/                 # SDK利用・配布手順
│  ├─ security/threat-model.md      # 脅威モデル
│  └─ adr/                          # Accepted/Proposedな設計判断
├─ proto/                           # Realtime公開契約
├─ openapi/                         # REST公開契約・error registry
├─ contracts/                       # 機械可読な状態機械・protocol policy
├─ test-vectors/                    # 互換性test vectors
├─ .github/PULL_REQUEST_TEMPLATE.md
├─ LICENSE-MIT
└─ LICENSE-APACHE
```

crateの責務と依存規則は [`docs/design/repo-crate-conventions.md`](docs/design/repo-crate-conventions.md) が正本です。

設計索引、仕様全47章のcoverage、ロードマップ、MVP受入条件は [`docs/design/roadmap-and-traceability.md`](docs/design/roadmap-and-traceability.md) を参照してください。

## 主要ドキュメント

- [原仕様](metaverse_core_specification.md)
- [フロントエンド接続ガイド](docs/guides/frontend-integration.md)
- [設計索引](docs/design/README.md)
- [ADR索引](docs/adr/README.md)
- [運用Runbook](docs/operations/README.md)
- [システムコンテキスト](docs/design/system-context.md)
- [アーキテクチャ](docs/design/architecture.md)
- [ドメインモデル](docs/design/domain-model.md)
- [Realtime protocol](docs/design/realtime-protocol-and-connection.md)
- [認証・認可](docs/design/auth-authorization.md)
- [デプロイと脅威モデル設計](docs/design/deployment-and-threat-model.md)
- [詳細脅威モデル](docs/security/threat-model.md)
- [テスト・CI](docs/design/test-and-ci.md)
- [ロードマップとトレーサビリティ](docs/design/roadmap-and-traceability.md)
- 汎用アプリ基盤化に向けた改善提案（計画） (internal record omitted from this source distribution)

## 公開契約と設計検査

- [OpenAPI v1](openapi/orbisync-v1.yaml)
- [REST error registry](openapi/errors.yaml)
- [Realtime Protocol Buffers v1](proto/orbisync/v1/realtime.proto)
- [Realtime接続状態機械](contracts/realtime-connection-state-machine.json)
- [Realtime Resume Token Policy](contracts/realtime-resume-token-policy.json)
- [互換性test vectors](test-vectors/)

追加packageなしで設計全体を検査できます。

```shell
python scripts/validate_design.py --review
```

この検査はUTF-8、ローカルリンク、Markdown fence、仕様第1〜47章のcoverage、ADR参照、legacy名、Proto field番号、OpenAPI/error registry、状態機械、test vectorsを確認します。

## ローカル開発

### 必要なもの

| ツール | 用途 | 備考 |
|---|---|---|
| Rust 1.95.0 | workspaceのbuild/test | `rust-toolchain.toml` が固定します。rustupが自動導入します |
| Docker | 開発用PostgreSQL | Docker Compose v2を使用します |
| Python 3.12+ | 設計検査・依存規則検査 | OpenAPI検査には `openapi-spec-validator` と `pyyaml` が必要です |

`protoc` は不要です。`orbisync-protocol` のbuild scriptがpure Rustのcompiler（protox）で `proto/` をbuild時に生成します。生成コードはcommitしません（ADR-004）。

### 最短起動

Docker Composeで一式を起動します。

```shell
git clone <repository-url> orbisync
cd orbisync
cp deploy/compose/.env.example .env
bash scripts/gen-dev-password-denylist.sh    # 起動に必須。下記参照
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d --build
docker compose -f deploy/compose/compose.dev.yml --env-file .env \
  run --rm --entrypoint orbisync-server server migrate
```

サーバーをローカルでbuildして動かす場合は、PostgreSQLだけを起動します。

```shell
docker compose -f deploy/compose/compose.dev.yml --env-file .env up -d postgres
set -a && . ./.env && set +a          # PowerShellの場合は各変数を手動でexportしてください
cargo run -p orbisync-server -- migrate
export ORBISYNC_PASSWORD_DENYLIST_FILE=deploy/dev-password-denylist.txt
cargo run -p orbisync-server -- doctor
cargo run -p orbisync-server
```

> [!IMPORTANT]
> **パスワードdenylistは起動に必須です。** `PasswordPolicy::production` はちょうど10,000件の corpus を要求し、無い場合サーバーは起動しません。OrbiSyncはこのcorpusを同梱していません。有用なリストは第三者のデータで独自のライセンスがあり、同梱すると運用者に代わってライセンス上の判断をすることになるためです。
>
> `scripts/gen-dev-password-denylist.sh` が生成するのは**開発用のプレースホルダで、実在するパスワードを1つも弾きません**。実際に人がログインする環境へ出す前に、本物の一般的パスワード一覧へ差し替えてください。差し替えを怠ると、利用者が選んだありふれたパスワードがすべて通ります。

`scripts/bootstrap-dev.sh` が上記と検査コマンドをまとめて実行します。

`scripts/compose-e2e.sh` は、Compose一式を起動して migrate・管理者作成・ログイン・realtime ticket発行まで通ることを検証します。CIの必須ジョブです。

`doctor` は起動前の非破壊診断です。設定、必須secretの存在（値は表示しない）、PostgreSQL接続、適用済みmigrationとchecksum、checkpoint table、password denylist、bind addressを検査します。DBへは読み取りqueryだけを実行し、bind確認で開いたlistenerも直ちに閉じます。全項目合格で終了コード0、必須項目の失敗で1を返します。

```shell
cargo run -p orbisync-server -- doctor
cargo run -p orbisync-server -- doctor --json
```

新規DBではmigration未適用を正しく失敗として報告するため、`migrate` 後、サーバー起動前に実行してください。JSON出力は `ok` と、各検査の `name` / `status` / `detail` を返すので、ローカルスクリプトやデプロイ前検査にも利用できます。

起動後、次のendpointが応答します。

```shell
curl http://localhost:8080/health/live     # プロセス生存。常に200
curl http://localhost:8080/health/ready    # DB接続確立時のみ200、それ以外は503
curl http://localhost:8080/version         # service名・build version・protocol major
```

公開しているREST endpointの完全な一覧は [`openapi/orbisync-v1.yaml`](openapi/orbisync-v1.yaml) が正本です（37 path / 46 operations。`scripts/validate_openapi_routes.py` が実Routerとの一致を検証します）。運用系の `/health/live` `/health/ready` `/version` `/metrics` も同じ契約に含まれます。Realtime WebSocketは同一handlerを `/ws` と `/v1/realtime/ws` の2つのpathで提供し、realtime ticketによる接続を受け付けます。

`GET /v1/admin/diagnostics` は `admin.diagnostics.read` 権限を要求します。最新checkpoint時刻、checkpoint失敗数、queueの直近観測値と上限、active connection、rate-limit拒否数、extension DLQ、各retention workerの最終成功時刻を返します。secret、payload、user/instance IDは返しません。新規bootstrap管理者には権限が付与されます。既存環境では管理者ロールへこの権限を追加してください。

access tokenとrealtime ticketはaudienceで分離しており、互いに流用できません。`/ws` または `/v1/realtime/ws` へaccess tokenを提示しても、REST endpointへrealtime ticketを提示しても拒否されます。

### 最初の管理者を作る

migrate直後のDBにはユーザーが1人もいないため、まず管理者を作ります。

```shell
cargo run -p orbisync-server -- bootstrap-admin \
  --login-id admin --display-name "Administrator" \
  --password-denylist deploy/dev-password-denylist.txt \
  --password-output ./admin-password.txt
```

一時パスワードは**この1回しか取得できません**。出力先が端末でない場合は `--password-output` が必須です。パイプやCIログへ平文が流れるのを防ぐためで、指定したファイルは `0600` で新規作成されます。

**このコマンドは権限チェックを行わないため、ユーザーが1人でも存在する場合は実行を拒否します。** 拒否は資格情報を生成する前に行われるので、失敗しても未使用のパスワードは残りません。

以降のユーザーは、この管理者のaccess tokenで `POST /v1/users` から作成します。一時パスワードは既定のレスポンスには含まれず、`Accept: application/vnd.orbisync.user-credential+json` を明示した場合にのみ返ります。

### 管理画面を起動する

[`apps/admin-console-web`](apps/admin-console-web) は実サーバーの REST API に接続します。初期設定とパスワード復旧用の管理画面は [`apps/admin-web`](apps/admin-web) にあります。Compose 開発設定は Vite の既定 origin を CORS 許可済みです。ホスト上でサーバーを起動する場合は、起動前に次も設定してください。

```shell
export ORBISYNC_CORS_ALLOWED_ORIGINS=http://localhost:5173,http://127.0.0.1:5173,http://localhost:5174,http://127.0.0.1:5174
```

別の端末で管理画面を起動します。

```shell
npm --prefix apps/admin-console-web ci
npm --prefix apps/admin-console-web run dev
# http://localhost:5174 を開き、上で作成した管理者でログイン
```

管理画面では health/version、ユーザー作成・表示名編集・状態変更、ロール作成・編集・削除・割り当て、World、Instance、参加者、監査ログを確認・操作できます。access token と refresh token はブラウザのメモリ内だけに保持され、再読み込みまたはログアウトで破棄されます。本番では管理画面と API を HTTPS で提供し、CORS には実際の管理画面 origin だけを指定してください。

[`apps/reference-console-web`](apps/reference-console-web) は同じ実サーバーとTypeScript SDKへ接続する利用者向けsmoke画面です。`npm --prefix apps/reference-console-web ci && npm --prefix apps/reference-console-web run dev` で起動し、ログイン、World/Instance作成・起動、Realtime参加、Entity作成・更新・移動・削除、SDKの自動resumeを確認できます。fake dataやstub WebSocketへのfallbackはありません。元の3Dゲームは [`apps/reference-web`](apps/reference-web) にあります。

管理画面だけの回帰確認は `npm --prefix apps/admin-console-web run test:browser` で実行できます（初回は `apps/admin-console-web` で `npx playwright install chromium` が必要です）。実 Chromium とモック REST API を使うため、DB やサーバーを起動せず主要操作と送信契約を確認できます。

起動には次の環境変数が必要です。未設定の場合、設定検証の段階で起動を中止します（仕様 §28.1）。secretは設定ファイルへ書かず、環境変数で注入します。

- `DATABASE_URL`
- `ORBISYNC_TOKEN_SIGNING_KEY`
- `ORBISYNC_PAGINATION_HMAC_KEY`
- `ORBISYNC_REFRESH_TOKEN_HMAC_KEY`
- `ORBISYNC_REALTIME_TICKET_HMAC_KEY`
- `ORBISYNC_IDEMPOTENCY_HMAC_KEY`
- `ORBISYNC_EXTENSION_TOKEN_HMAC_KEY`（32バイト以上の独立した鍵）

拡張用tokenの発行・失効とCommand APIの利用方法は[拡張APIガイド](docs/guides/extension-command-api.md)を参照してください。

### 設定

設定の優先順位はCLI引数 > 環境変数 > 設定ファイル > 安全なデフォルト値です（仕様 §28.3）。環境変数は `ORBISYNC_<SECTION>_<KEY>` 形式です。

```shell
cargo run -p orbisync-server -- --config orbisync.toml.example --bind 127.0.0.1:9090
```

全設定キーと既定値は [`orbisync.toml.example`](orbisync.toml.example) を参照してください。

### 検証コマンド

PR CIと同じ内容をローカルで実行できます。

```shell
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
python scripts/check_architecture.py      # crate依存規則と循環検査
python scripts/check_source_hygiene.py    # 文字化け・エンコーディング検査
python scripts/validate_design.py --review
python scripts/validate_openapi.py        # pip install openapi-spec-validator pyyaml
```

TypeScript SDKはNode 24.xを使用し、protobufの生成コードをcommitしないため、clone後とproto更新後は検査の前に生成が必要です。ルートの`buf.gen.yaml`はTypeScriptとRustの両pluginを要求します。次のplugin導入は環境準備として一度行い、両コマンドをPATHへ通してください（CIと同じpluginバージョン）。

```shell
npm install --global @bufbuild/protoc-gen-es@2.13.0
cargo install protoc-gen-prost --locked --version 0.5.0
npx --yes @bufbuild/buf generate
cargo build -p orbisync-e2e-helper  # SDKの実サーバー接続テスト用。CIも事前にbuildします
cd sdk/typescript && npm ci && npm run check && npm test
```

参照Webは別のTypeScript専用生成手順をdev/build/check前に自動実行するため、Node 22以上でRust pluginなしに起動できます。[Webの起動手順](apps/reference-web/README.md)と[SDK開発手順](sdk/typescript/DEVELOPMENT.md)を区別してください。

PostgreSQLを使うintegration testは `DATABASE_URL` を設定した場合のみ実行されます。未設定の場合はskipされます。

```shell
DATABASE_URL=postgres://orbisync:orbisync-local-dev@localhost:5432/orbisync \
  cargo test -p orbisync-integration-tests
```


## 開発状況

「文書に設計があること」と「実装済みであること」を混同しません。同じ理由で、「crateに実装があること」と「サーバーに接続されていること」も区別します。次の表は後者の基準、つまり実際に動作する経路として検証済みかどうかで分類しています。

### 動作する

integration testが本番コードの経路を通ることを確認済みです。

| 機能 | 備考 |
|---|---|
| 設定・logging・health/version | 起動時に必須設定を検証して中止します |
| 運用診断 | `GET /v1/admin/diagnostics`。`admin.diagnostics.read` が必要で、healthには詳細を混ぜません |
| ログインとaccess token発行 | Argon2によるパスワード検証、Ed25519署名 |
| Realtime ticket発行と検証 | 60秒の寿命、audienceでaccess tokenと分離 |
| 最初の管理者の作成とユーザー作成API | `bootstrap-admin` と `POST /v1/users`。RBACで `admin.users.create` を要求 |
| World/Instance管理 | REST（World: 一覧・作成・参照・PATCH更新・archive。Instance: 一覧・作成・参照・start・stop・kick・members）、PostgreSQL永続化 |
| 管理Web UI | 実REST APIへ接続し、health/version、ユーザー、ロール、World、Instance、参加者、監査ログを操作。PostgreSQL 17を使うブラウザsmoke testで確認 |
| WebSocket接続とInstance参加 | ticket検証を経ない参加は拒否されます |
| Transform同期と検証 | 速度・距離の上限で不正な更新を拒否します |
| Snapshot / State Delta配信 | 参加時のsnapshotと以降の差分配信 |
| Interest Management | 均一グリッド、ヒステリシス付き購読、可視性ポリシー |
| Entityの生成・更新・削除 | `EntityCommand`経由。所有権とrevisionを検証し、結果はreliableイベントとして可視性判定を通してから配信 |
| 送信集約（backpressure） | 接続ごとのキュー。詰まった受信者へは最新の位置だけを送り、reliableイベントは間引かない |
| 切断からのResume | サーバー保持の単一使用トークン。同じPresenceで復帰し、履歴が足りなければ`ResyncRequired`で再同期へ移行 |
| バックアップからの復元 | `scripts/restore-drill.sh`で復元先ログインまで検証。`run_restore_verification.py`は実backup/checksumにも対応し、成功・失敗、backup ID、所要時間をJSONL履歴へ記録 |
| Heartbeatとrate limit | タイムアウト検出、カテゴリ別の流量制限 |
| TypeScript SDK | Envelopeのencode/decode、CIでtype checkとround-trip testを実行 |

### Entityの所有権移転

ADR-027に基づき、reliable WebSocket `EntityCommand`の
`operation = "transfer_ownership"`として公開済みです。TypeScript SDKでは
`instance.transferEntityOwnership(...)`を使用できます。

現owner（`entity.update.own`保持者）または`entity.update.any`保持者だけが実行でき、
移譲先は適用時点で同じInstanceに参加中でなければなりません。owner解除、同一ownerへの
移転、ownerなしEntityのクライアントによる取得は拒否します。成功時はEntityと管理監査を
同じPostgreSQL transactionで保存し、`entity.ownership_transferred` webhook eventも
発行します。再送は既存の`command_id`重複排除契約に従います。

詳細な引数、エラー、配信範囲は
[`ADR-027`](docs/adr/ADR-027-entity-ownership-transfer.md)を参照してください。

### 未実装

| 機能 | 備考 |
|---|---|
| 公開container image | Dockerfileと開発用Docker Composeはあります |
| 24時間耐久・1,000接続の確定評価 | 本コピーの作成時に負荷試験は実施していません。接続数や継続時間の保証はありません |

MVPは、仕様とトレーサビリティ文書に定義された18項目をすべて検証できた時点で完了とします。項目ごとの対応状況は [`docs/design/roadmap-and-traceability.md`](docs/design/roadmap-and-traceability.md) を参照してください。

受入条件と対応テストは[テスト対応表](docs/design/acceptance-traceability/)を参照してください。内部の作業メモ・レビュー記録・実行ログは配布物に含めていません。

## Contributing

Issue、設計レビュー、ADR、文書修正、実装への貢献を歓迎します。作業前に [CONTRIBUTING.md](CONTRIBUTING.md) を確認してください。

セキュリティ上の問題は公開Issueへ投稿せず、[SECURITY.md](SECURITY.md) の非公開報告手順を使用してください。

## License

OrbiSyncは、利用者の選択により次のいずれかの条件で提供されます。

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

SPDX expression: `MIT OR Apache-2.0`

Copyright © 2026 avistoria
