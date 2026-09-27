# OrbiSync リポジトリ構成・Crate 責務・コーディング規約設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §29（リポジトリ構成）、§30（Crate 責務と依存規則）、§31（コーディング規約）を、workspace/crate 構成、allowed dependency matrix、public contract 生成物、feature/visibility、error/async/clock 規約の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `RC-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| レイヤーと依存方向、強制ルール | `architecture.md` §2, §2.1 | 前提として参照。依存規則の正本 |
| モジュール境界、所有権、DAG | `architecture.md` §3, §3.1 | 前提として参照。crate 構成はモジュール境界へ対応 |
| 境界をまたぐ型（Command/Query/Event/Port/Stable ID） | `architecture.md` §3.2 | 前提として参照 |
| データ所有権、一時/永続状態の分離 | `architecture.md` §3.3、`state-and-runtime.md` §1 | 前提として参照 |
| Composition Root、起動/終了順序 | `architecture.md` §4.3 | 前提として参照 |
| 検証可能なアーキテクチャ適合条件 | `architecture.md` §9 | 前提として参照。本書は CI 検査との対応を補う |
| crate/workspace 分割粒度 | `architecture.md` ARC-01 | 本書は設計候補を提示し、確定は ARC-01 へ委ねる |
| 技術選定（Rust/Tokio/Axum/Protobuf/SQLx 等） | `technology-decisions.md` TD-01〜TD-13 | 前提として参照 |
| Protocol Buffers 生成、互換性規則 | `transport-boundaries.md` §2, §3、ADR-004 | 前提として参照 |
| テスト戦略、CI/CD | `test-and-ci.md`（本書と同時作成） | 相互参照 |
| 設定の source/precedence/validation | `observability-and-config.md` §6 | 前提として参照 |

## 1. リポジトリ構成

### 1.1 Monorepo 方針

**[REC]** Cargo Workspace を使用した monorepo を採用する（仕様 §29 の推奨）。プロトコル、サーバー、SDK、参照実装、負荷試験、ドキュメントを同一リポジトリで互換性テストできる利点がある。

**[REC]** 上記は仕様の推奨であり、確定した構成ではない。本書は仕様 §29 の例示を基に、`architecture.md` のモジュール境界と整合する構成を [REC] として提示する。

### 1.2 トップレベル構成

**[REC]** 推奨するトップレベル構成：

```text
orbisync/
├─ Cargo.toml              # workspace root
├─ Cargo.lock
├─ rust-toolchain.toml
├─ rustfmt.toml
├─ clippy.toml
├─ deny.toml               # cargo-deny 設定
├─ README.md
├─ LICENSE-*
├─ CHANGELOG.md
├─ CONTRIBUTING.md
├─ CODE_OF_CONDUCT.md
├─ SECURITY.md
├─ CODEOWNERS
├─ .editorconfig
├─ .gitignore
├─ .dockerignore
│
├─ .github/                # CI/CD（test-and-ci.md §3）
├─ crates/                 # Rust workspace members（§2）
├─ proto/                  # Protocol Buffers schema（§4）
├─ openapi/                # OpenAPI spec（§4）
├─ migrations/             # DB migration（§5）
├─ sdk/                    # 公式 SDK（§6）
├─ apps/                   # 参照実装・負荷試験（§6）
├─ tests/                  # workspace 横断テスト（test-and-ci.md）
├─ deploy/                 # Docker/Compose 構成
├─ docs/                   # ドキュメント・ADR
├─ examples/               # 利用例
├─ scripts/                # 開発スクリプト
└─ tools/                  # 補助ツール
```

**設計前提** ADR-001によりrepository/公開名を`OrbiSync`、server packageを`orbisync-server`、公開Rust package接頭辞を`orbisync-*`に確定した（`docs/adr/ADR-001-naming-identifiers.md`）。**[REC]** ライセンスはAccepted済みのRL-05に従い、`MIT OR Apache-2.0`とする（`docs/adr/RL-05-license.md`）。

### 1.3 仕様例との対応

仕様 §29 のディレクトリツリーは例示であり、確定したファイル構成ではない。本書の構成は仕様例の構造を継承しつつ、`architecture.md` §3 のモジュール境界との対応を明示する。

## 2. Crate 構成と責務

### 2.1 crate 分割の方針

**設計前提** crate/workspace の分割粒度は ARC-01 で決める。推奨案は、まず境界ごとの top-level module と domain/application/adapter の可視性を厳格化し、独立コンパイル価値が高い protocol/domain から crate 化することである（`architecture.md` ARC-01）。

**[REC]** 初期構成では、`architecture.md` §3 のモジュール境界を crate の候補境界とする。以下の表は仕様 §30 の crate 例と `architecture.md` モジュールの対応である：

| 仕様 §30 の crate 例 | architecture.md モジュール | 本書の crate 候補 | レイヤー |
|---|---|---|---|
| `domain` | 全モジュールの domain 層 | `orbisync-domain` | Domain |
| `application` | 全モジュールの application 層 | `orbisync-application` | Application |
| `protocol` | protocol 生成・変換 | `orbisync-protocol` | Shared/Adapter |
| `realtime` | `realtime_gateway` + `realtime_delivery` + `realtime_presence` | `orbisync-realtime` | Inbound Adapter + Outbound Adapter |
| `world-runtime` | `instance_runtime` | `orbisync-world-runtime` | Application/Domain |
| `interest` | `interest` | `orbisync-interest` | Domain Service |
| `auth` | `identity_access` | `orbisync-identity` | Application/Domain |
| `storage-postgres` | `persistence` | `orbisync-storage-postgres` | Outbound Adapter |
| `transport-http` | `http_api` | `orbisync-transport-http` | Inbound Adapter |
| `extensions` | `extension_gateway` + `eventing` | `orbisync-extensions` | Outbound Adapter |
| `observability` | `audit_observability` | `orbisync-observability` | Outbound Adapter |
| `config` | 設定（起動処理） | `orbisync-config` | Shared |
| `testkit` | テストユーティリティ | `orbisync-testkit` | Test |
| `server` | Composition Root | `orbisync-server` | Composition Root |
| — | `world_directory` | `orbisync-world-directory` | Application/Domain |
| — | `load_test_tooling` | `apps/load-generator`（workspace 外） | Test Tool |

**[REC]** `world_directory` は仕様 §30 の crate 例に明示がないが、`architecture.md` §3 で独立モジュールとして定義されている。crate 候補として追加する。

**[REC]** `load_test_tooling` は Core 実行サーバー外から公開 API/protocol を駆動する負荷試験ツールであり、サーバー内部 module に依存しない（`architecture.md` §3）。workspace の `crates/` ではなく `apps/load-generator` へ配置する。

**[ADR]** 上記の crate 候補を初期からすべて独立 crate とするか、単一 crate 内の module で開始して段階的に分離するかは ARC-01 で決める。本書の依存規則（§3）は crate 分割後も module 分割後も同一に適用する。

### 2.2 各 crate の責務

**[REC]** 各 crate の責務は仕様 §30 と `architecture.md` §3 に従う。以下は仕様 §30 の責務定義を `architecture.md` の用語で整理したものである。

#### `domain`

**設計前提** 純粋なドメイン型、値オブジェクト、ドメインエラー、ドメインルールを所有する（仕様 §30.1、`architecture.md` §2）。

- 外部 crate 依存を最小化する
- SQL、HTTP、Protobuf 型を置かない（`architecture.md` §2.1）
- フレームワーク crate、DB 行型、HTTP/Protocol DTO を import しない（`architecture.md` §2.1）
- 全モジュールの domain 型（`UserId`、`InstanceId`、`Revision`、`Transform`、`VisibilityPolicy` 等）を所有する（`domain-model.md` §2）

#### `application`

**設計前提** ユースケース、トランザクション境界、repository 等の port trait、認可呼び出し、ドメインイベント生成を所有する（仕様 §30.2、`architecture.md` §2）。

- Application の公開関数は Axum extractor、HTTP status、SQLx transaction/row、WebSocket frame を引数・戻り値にしない（`architecture.md` §2.1）
- Feature module 同士を直接 orchestration させず、Application coordinator が use case の順序を所有する（`architecture.md` §3.1）
- port trait の定義を所有し、adapter がそれを実装する（`architecture.md` §2.1）

#### `protocol`

**設計前提** Protobuf 生成コード、version negotiation、protocol validation、ドメイン型との変換を所有する（仕様 §30.3、`transport-boundaries.md` §2, §3）。

- 生成コードへ手編集してはならない（仕様 §30.3）
- protocol DTO ↔ domain 型の変換ユーティリティを所有する（`transport-boundaries.md` §3.2）
- `.proto` を公開契約の正本とする（TD-04）

#### `realtime`

**設計前提** WebSocket 接続、connection registry、heartbeat、resume、outbound queue、protocol frame 処理を所有する（仕様 §30.4、`architecture.md` §3）。

- ワールドのドメイン状態を直接所有しない（仕様 §30.4）
- `realtime_gateway`（socket 所有、接続 I/O）、`realtime_delivery`（接続別 bounded queue、delivery 調停）、`realtime_presence`（Realtime Connection / Presence、resume binding）を含む（`architecture.md` §3）
- socket を所有するのは `realtime_gateway` であり、`realtime_delivery` は socket 自体を所有しない（`architecture.md` §3）

#### `world-runtime`

**設計前提** Instance Actor、一時状態、command 処理、snapshot/delta 生成、所有権、lifecycle を所有する（仕様 §30.5、`architecture.md` §3、`state-and-runtime.md` §3）。

- HTTP や DB 実装へ依存しない（仕様 §30.5）
- `interest` や `realtime_delivery` を直接呼ばない（`architecture.md` §3.1 DAG）
- 正準な一時状態の正本を所有する（`state-and-runtime.md` §2.1）

#### `world-directory`

**設計前提** ワールド定義、インスタンスの作成/列挙/ライフサイクル調停を所有する（`architecture.md` §3）。

- 仕様 §30 に明示の crate 例がないが、`architecture.md` §3 の独立モジュールである
- instance locator を公開する（`architecture.md` §3）

#### `interest`

**設計前提** 空間 index、visibility、subscription 計算、配信対象決定を所有する（仕様 §30.6、`architecture.md` §3、MRIB 書 §7）。

- 描画 LOD やアセットを扱わない（仕様 §30.6）
- I/O を行わず、connection queue / socket / gateway に依存しない（`architecture.md` §3、MRIB 書 §7.1）
- state/visibility view から recipient ID 集合を計算する純粋関数である（`architecture.md` §3）

#### `identity`

**設計前提** 認証、ユーザー、ロール、権限、Identity Session、token/失効を所有する（`architecture.md` §3、`auth-authorization.md`）。

- `AuthSessionId` を所有する（`architecture.md` §3）
- 仕様 §30.7 の `auth` crate 例に対応する

#### `storage-postgres`

**設計前提** repository 実装、SQL、transaction、DB 行とドメイン型の変換を所有する（仕様 §30.7、`architecture.md` §3、`rest-api-persistence.md`）。

- 各 domain モジュールが定義する port の実装を所有する（`architecture.md` §3）
- SQLx / PostgreSQL への依存をこの crate に閉じ込める（`architecture.md` §2.1）

#### `transport-http`

**設計前提** REST/OpenAPI、認証コンテキスト、管理/制御 API adapter を所有する（仕様 §30 には明示がないが `architecture.md` §3 の `http_api` に対応）。

- HTTP routes を所有し、application use cases を呼び出す（`architecture.md` §3）
- Axum への依存をこの crate に閉じ込める（`architecture.md` §2.1）

#### `extensions`

**設計前提** 外部サービスとの認証、配送、タイムアウト、再試行、汎用イベントの内部配布と外部配送要求を所有する（`architecture.md` §3、`extension-mechanism.md`）。

- Out-of-process Extension のみ（仕様 §23、`architecture.md` §3）
- `extension_gateway` と `eventing` を含む

#### `observability`

**設計前提** 監査記録 port、ログ/メトリクス/トレース adapter を所有する（`architecture.md` §3、`observability-and-config.md` §1）。

- telemetry facade を提供し、他モジュールは facade を通じて観測機能を呼び出す（`observability-and-config.md` §1）

#### `config`

**設計前提** 設定の読み込み、検証、構造化を所有する（`observability-and-config.md` §6）。

- 起動時に全設定を検証する（仕様 §28.1）
- secrets と通常設定を分ける（仕様 §28.1）

#### `testkit`

**[REC]** テスト用の fixture、fake adapter、決定論的 clock、protocol client、instance harness を所有する。

- 本番コードから参照されない（dev-dependencies のみ）
- fake adapter により use case test を可能にする（`architecture.md` §9.5）

#### `server`

**設計前提** Composition Root のみを担当する（仕様 §30.8、`architecture.md` §4.3）。

- 設定読込、各実装の生成、dependency wiring、server 起動、graceful shutdown
- ビジネスロジックを置かない（仕様 §30.8）
- 起動処理だけが具体 adapter を選び、port へ注入する（`architecture.md` §4.3）

## 3. 依存規則

### 3.1 許可する依存方向

**設計前提** 許可する依存は `adapter → application → domain` である（`architecture.md` §2）。

```text
┌────────────────────────────────────────────────────────────┐
│ Inbound adapters                                           │
│ transport-http / realtime(gateway) / protocol              │
└──────────────────────────┬─────────────────────────────────┘
                           │ DTO → command/query
                           ▼
┌────────────────────────────────────────────────────────────┐
│ Application                                                │
│ application / world-runtime / world-directory / identity   │
│ interest / extensions / config                             │
└──────────────────────────┬─────────────────────────────────┘
                           │ domain types
                           ▼
┌────────────────────────────────────────────────────────────┐
│ Domain                                                     │
│ domain                                                     │
└────────────────────────────────────────────────────────────┘
                           ▲
                           │ implements ports
┌──────────────────────────┴─────────────────────────────────┐
│ Outbound adapters                                          │
│ storage-postgres / observability / extensions(delivery)    │
└────────────────────────────────────────────────────────────┘
```

### 3.2 Allowed Dependency Matrix

**[REC]** crate 間の許可する依存を行列で定義する。`○` は許可、`—` は禁止、`port` は port trait の実装としての依存（逆方向）を表す。

| 依存元 ↓ \ 依存先 → | domain | application | protocol | realtime | world-runtime | world-directory | interest | identity | storage-postgres | transport-http | extensions | observability | config | testkit | server |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **domain** | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **application** | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **protocol** | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **realtime** | ○ | ○ | ○ | — | — | — | — | — | — | — | — | — | ○ | — | — |
| **world-runtime** | ○ | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **world-directory** | ○ | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **interest** | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **identity** | ○ | ○ | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **storage-postgres** | ○ | port | — | — | — | — | — | — | — | — | — | — | ○ | — | — |
| **transport-http** | ○ | ○ | — | — | — | — | — | — | — | — | — | — | ○ | — | — |
| **extensions** | ○ | ○ | — | — | — | — | — | — | — | — | — | — | ○ | — | — |
| **observability** | ○ | port | — | — | — | — | — | — | — | — | — | — | ○ | — | — |
| **config** | — | — | — | — | — | — | — | — | — | — | — | — | — | — | — |
| **testkit** | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | — | — | — | — | ○ | — | — |
| **server** | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | ○ | — | — |

**[REC]** 行列の読み方：

- `domain` はどの crate にも依存しない（純粋なドメイン型）
- `application` は `domain` のみに依存する
- `interest` は `domain` のみに依存する（I/O なしの純粋計算、`architecture.md` §3）
- `world-runtime` は `interest` や `realtime` に依存しない（DAG、`architecture.md` §3.1）
- `storage-postgres` は `application` の port trait を実装する（`port`）
- `server` は全 crate に依存してよい（Composition Root、`architecture.md` §4.3）
- `testkit` は本番 crate に依存してよいが、本番 crate は `testkit` に依存しない（dev-dependencies のみ）

**[REC]** 上記は crate 分割時の依存規則である。単一 crate 内の module 分割の場合も、同一の依存方向を module 間の `use` 規則として適用する。

**設計前提** `orbisync-protocol`を共有contract/生成code crateとし、adapterごとのdomain変換は各adapterが所有する（ADR-004）。

### 3.3 禁止する依存

**設計前提** 以下の依存は禁止する（`architecture.md` §2.1）：

| 禁止 | 理由 |
|---|---|
| `domain` → フレームワーク crate（Axum, SQLx, Tokio） | ドメインの純粋性 |
| `domain` → DB 行型、HTTP/Protocol DTO | 境界の混同 |
| `application` → Axum extractor、HTTP status、SQLx row | adapter 型の漏洩 |
| `world-runtime` → `interest`、`realtime_delivery` | DAG 違反（`architecture.md` §3.1） |
| `interest` → `realtime_presence`、`realtime_delivery`、`realtime_gateway` | DAG 違反 |
| `realtime_gateway` ↔ `realtime_delivery` の相互 import | 循環依存（`architecture.md` §3.1） |
| 他モジュールの repository 実装や DB テーブルの直接利用 | データ所有権違反（`architecture.md` §3.3） |
| 循環依存 | `architecture.md` §2.1 |

### 3.4 依存規則の強制

**設計前提** 実装時には次を CI で検査できる構成を推奨する（`architecture.md` §9）：

1. Domain package の依存グラフに Axum、SQLx、protocol 生成物が存在しない
2. Application service の公開 API に HTTP/DB/WebSocket 型が存在しない
3. モジュール依存グラフに循環がない
4. Protocol/OpenAPI 変換に contract tests がある
5. repository/extension/audit port は fake adapter で use case test が可能である
6. 公開操作が公式参照実装固有の経路を必要としない
7. モジュール所有外のテーブルを直接更新する query がない
8. `unsafe`、`unwrap()`/`expect()` の本番 path 使用が lint で検出される

**[REC]** 依存規則の強制方法：

| 検査 | 方法 | CI stage |
|---|---|---|
| crate 依存グラフ | `cargo deny check` または `cargo depgraph` の解析 | PR CI |
| module 内の `use` 規則 | `clippy` custom lint または `cargo check` の visibility 制約 | PR CI |
| 循環依存 | `cargo depgraph` の循環検出 | PR CI |
| domain へのフレームワーク混入 | `cargo deny` の依存グラフ検査 + code review | PR CI |

**[ADR]** 依存規則の自動検査ツール（`cargo deny` の設定、custom clippy lint、`cargo machete` 等）の選定は RC-01 で決める。

## 4. Public Contract 生成物

### 4.1 Protocol Buffers

**設計前提** `.proto` を公開契約の正本とする（TD-04）。

**[REC]** `proto/` ディレクトリに schema を配置する：

```text
proto/
├─ buf.yaml               # schema lint / breaking change 検査設定
├─ buf.gen.yaml            # 生成設定
└─ orbisync/
   └─ v1/
      ├─ common.proto      # 共通型（Timestamp, Vector3 等）
      ├─ envelope.proto    # Envelope（RP 書 §2）
      ├─ handshake.proto   # ClientHello / ServerHello
      ├─ instance.proto    # JoinInstance / Snapshot / StateDelta
      ├─ entity.proto      # EntityCommand
      ├─ transform.proto   # TransformInput
      ├─ event.proto       # DomainEvent
      ├─ error.proto       # ErrorMessage
      └─ admin.proto       # 管理用メッセージ
```

**[SPEC]** 生成コードへ手編集してはならない（仕様 §30.3）。

**[SPEC]** field number を再利用しない。削除 field は reserved とする（仕様 §34.4）。

**[SPEC]** unknown field を許容し、enum 追加を想定する（仕様 §34.4）。

**設計前提** ADR-004によりBufでlint/breaking検査を行い、生成codeはcommitせずbuild/CIで生成する。

### 4.2 OpenAPI

**設計前提** 管理 REST API は JSON + OpenAPI を使用する（TD-05）。

**[REC]** `openapi/` ディレクトリに OpenAPI spec を配置する。

**設計前提** ADR-003によりOpenAPI 3.1を正本とするOpenAPI-firstを採用する。

**設計前提** OpenAPI documentは人手で管理する公開契約の正本であり、生成物ではない（ADR-013）。将来SDKをOpenAPIから生成する場合も、生成SDKを正本へ逆流させず、OpenAPI validationとbase branchに対する後方互換性検査をCI gateとする。

### 4.3 生成物の検証

**設計前提** ADR-013 により生成コードは commit せず、CI で次を検証する（仕様 §33.1.9）：

1. `buf lint` で Protocol Buffers の規約違反がないことを確認する
2. `buf breaking --against <base>:proto` で base branch に対する後方互換性の破壊がないことを確認する
3. repository と同じ plugin で `buf generate` を実行し、tracked file の差分がゼロ、かつ ignore されない path の untracked 生成物がゼロであることを確認する
4. いずれかに違反した場合は CI を失敗させる

OpenAPIは上記の生成物差分検査の対象外とし、構文・参照validationとbase branchに対するbreaking-change検査を別途実行する（ADR-003、ADR-013）。

## 5. Migration

**設計前提** PostgreSQL の migration は `persistence` が所有する（`architecture.md` §3、`rest-api-persistence.md`）。

**[REC]** `migrations/` ディレクトリに SQL migration を配置する：

```text
migrations/
├─ 0001_initial.sql
├─ 0002_roles.sql
└─ README.md
```

**[SPEC]** 前方移行を基本とする。破壊的変更は複数リリースに分ける（仕様 §22.6）。

**[REC]** Expand → Migrate → Contract 方式を採用する（仕様 §22.6 の推奨）。

**[SPEC]** リリース前に既存 DB からの移行テストを行う（仕様 §22.6）。

**設計前提** migration toolはADR-005により`sqlx migrate`を採用する。

## 6. SDK と参照実装

### 6.1 SDK

**設計前提** 初期の公式 SDK は TypeScript とする（TD-12）。SDK の契約は `client-sdk.md` が定める。

**[REC]** `sdk/typescript/` に配置する。生成コード（Protobuf）と手書きの接続状態機械を分離する（TD-12）。

**[REC]** `sdk/test-vectors/` に protocol の golden test vectors を配置し、SDK 間で共有する（仕様 §32.3）。

### 6.2 参照実装と負荷試験

**[REC]** `apps/` に配置する：

| ディレクトリ | 用途 |
|---|---|
| `apps/reference-web/` | 最小参照 Web クライアント（疎通・E2E・SDK 利用例） |
| `apps/load-generator/` | 負荷試験ツール（`architecture.md` §3 の `load_test_tooling`） |

**設計前提** `load_test_tooling` は Core 実行サーバー外から公開 API/protocol を駆動する。サーバー内部 module に依存しない（`architecture.md` §3）。

## 7. コーディング規約

### 7.1 Rust 規約

**[SPEC]** 仕様 §31.1 が定める Rust 規約：

| 規約 | 強制方法 |
|---|---|
| `cargo fmt --check` | CI（PR 必須） |
| `cargo clippy --all-targets --all-features -- -D warnings` | CI（PR 必須） |
| `unsafe` は原則禁止 | clippy + code review。必要な場合は ADR、SAFETY コメント、専用テスト必須 |
| `unwrap()` / `expect()` を本番 path で原則禁止 | clippy lint + code review |
| `panic!` を入力エラー処理に使わない | code review + test |
| public item へ rustdoc | `cargo doc` + CI warning |
| error は型付きで扱う | code review |
| ログ文字列をエラーコード代わりにしない | code review |

**設計前提** `unsafe` の利用方針、ビルド target、パニック方針は TD-01 の制約に従う。

### 7.2 モジュールサイズ

**[SPEC]** 仕様 §31.2 が定めるモジュールサイズ規約：

- 1 ファイルが肥大化したら責務で分割する
- `mod.rs` へ大量実装を置かない
- public re-export を明示的に管理する
- util/common/generic な雑多 crate を作らない

**[REC]** ファイルサイズの目安は 500 行超過で分割を検討する。ただし行数は目安であり、責務の単一性を優先する。

### 7.3 型設計

**[SPEC]** Primitive Obsession を避ける（仕様 §31.3）：

```rust
pub struct UserId(Uuid);
pub struct InstanceId(Uuid);
pub struct LoginId(String);
pub struct Revision(u64);
```

**[SPEC]** 異なる ID を同じ `Uuid` 引数で取り違えないようにする（仕様 §31.3）。

**設計前提** IDの内部表現はADR-001でUUIDv7に統一した（`domain-model.md` §2、`docs/adr/ADR-001-naming-identifiers.md`）。

**[SPEC]** protocol 型と domain 型は明示的に変換する（仕様 §7.3）。

**設計前提** 同一の構造に見えても protocol 型、application 型、domain 型、persistence 型を暗黙に兼用しない。変換コードを互換性変更の検知点とする（`architecture.md` §3.2）。仕様 §7.3 の protocol/domain 変換要求を application/persistence 型へ拡張した設計判断である。

### 7.4 時刻

**[SPEC]** 仕様 §31.4 が定める時刻規約：

| 用途 | 形式 |
|---|---|
| DB 保存 | UTC |
| API | RFC 3339 |
| realtime protocol | Unix milliseconds 等、明示形式 |
| テスト | Clock trait を注入する |

**設計前提** Clock trait の注入により決定論的テストを可能にする（`architecture.md` §9、TD-08）。

### 7.5 Error 規約

**[REC]** error の設計規約：

| 規約 | 理由 |
|---|---|
| domain error は安定した型として定義する | 公開契約の一部（仕様 §26.5） |
| Transport エラーは公開エラーコードへ変換する | 内部 DB/ライブラリエラーを露出しない（仕様 §21.8） |
| error は `thiserror` 等で型付き定義する | ログ文字列をエラーコード代わりにしない（仕様 §31.1） |
| panic は要求/connection/task 境界で観測する | 正準状態の破損可能性がある runtime を無条件に継続しない（`architecture.md` §6） |
| 入力エラーに `panic!` を使わない | 仕様 §31.1 |

**[ADR]** error crate の選定（`thiserror`、`anyhow` の使い分け）、domain error の命名規則は RC-02 で決める。

### 7.6 Async 規約

**設計前提** Tokio を非同期実行基盤として使用する（TD-02）。

**[REC]** async の設計規約：

| 規約 | 理由 |
|---|---|
| unbounded channel を使用しない | バックプレッシャーの喪失（TD-02、`architecture.md` §7） |
| 無制限 task 生成をしない | リソース枯渇（TD-02） |
| CPU 集約処理を async worker で直接実行しない | runtime worker のブロッキング（TD-02、TD-07） |
| blocking 処理は `spawn_blocking` 等で隔離する | Argon2id ハッシュ等（TD-07） |
| async fn の公開 API に transport 型を漏らさない | `architecture.md` §2.1 |

### 7.7 Feature / Visibility

**[REC]** Cargo feature と visibility の規約：

| 規約 | 理由 |
|---|---|
| public API を最小化する | 仕様 §26.5 |
| `pub(crate)` をデフォルトとし、公開必要なもののみ `pub` | 意図しない依存の防止 |
| feature flag で optional 機能を制御する | 初期の JSON debug mode 等（TB-02） |
| `#[cfg(test)]` のコードを本番ビルドに含めない | テストコードの分離 |
| feature の組み合わせで compile error にならないことを CI で確認する | feature 整合性 |

**[ADR]** feature flag の粒度（crate 単位 vs module 単位）、JSON debug mode の feature 名は RC-03 で決める。

## 8. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 8.1 依存規則

1. `domain` crate の依存グラフに Axum、SQLx、Tokio、protocol 生成物が存在しない。
2. `application` crate の公開 API に HTTP/DB/WebSocket 型が存在しない。
3. crate 依存グラフに循環がない。
4. `world-runtime` が `interest` または `realtime` を import しない。
5. `interest` が `realtime_presence`、`realtime_delivery`、`realtime_gateway` を import しない。

### 8.2 生成物

6. `buf lint`、base branch の `proto/` に対する `buf breaking`、および `buf generate` 後の tracked 差分ゼロ・ignore されない untracked 生成物ゼロが CI で検証される。
7. 生成コードは repository に tracked file として存在せず、build または code generation のたびに正本から生成されるため、手編集が保存・review・merge の対象にならない。

### 8.3 コーディング規約

8. `cargo fmt --check` が pass する。
9. `cargo clippy --all-targets --all-features -- -D warnings` が pass する。
10. 本番 path に `unwrap()` / `expect()` がない（clippy lint で検出）。
11. public item に rustdoc がある。

### 8.4 型設計

12. `UserId`、`InstanceId` 等の newtype が定義され、異なる ID の取り違えが compile error で検出される。
13. protocol 型と domain 型の変換が adapter 境界に閉じ込められている。

## 9. ADR テンプレート

**[REC]** 仕様 §43 が示す ADR テンプレートを、設計判断の記録に使用する。以下は仕様例であり、プロジェクトの運用に応じて調整してよい。

```markdown
# ADR-XXXX: タイトル

- Status: Proposed / Accepted / Deprecated / Superseded
- Date: YYYY-MM-DD
- Decision Owners:

## Context

何を決定する必要があるか。

## Decision

採用する設計。

## Alternatives

検討した案。

## Consequences

良い影響、悪い影響、運用上の負担。

## Migration

既存実装への移行方法。
```

**[REC]** ADR は `docs/adr/` に配置する（`repo-crate-conventions.md` §1.2）。Status が Accepted の ADR のみ設計判断として有効である。

## 10. 開発者向け最短起動体験

**[REC]** 仕様 §45 が示す新規コントリビューターの最短起動体験を目標とする。以下は仕様例であり、確定したスクリプトや手順ではない。

```bash
git clone https://example.org/orbisync.git
cd orbisync
cp deploy/compose/.env.example .env
docker compose -f deploy/compose/compose.dev.yml up -d
cargo run -p orbisync-server
```

**[REC]** `scripts/bootstrap-dev.sh` で依存確認、DB 起動、migration、初期管理者作成まで自動化してよい（仕様 §45）。

**[REC]** 起動体験の詳細は CONTRIBUTING.md に記載する（`release-maintenance-license.md` §2.1）。

## 11. 要 ADR 事項

本書が主担当となる判断を RC ID で管理する。他文書が正本の判断（ARC-01、ADR-001、ADR-003、ADR-004、ADR-005、TD-13 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| RC-01 | 依存規則の自動検査ツール | `cargo deny` + `cargo depgraph` + clippy | `architecture.md` §9 の検査項目を CI で自動化。具体ツールは評価後に確定 |
| RC-02 | error crate の選定と domain error 命名規則 | `thiserror`（domain/library）、`anyhow`（server binary の最外殻のみ） | 仕様 §31.1 の型付き error。domain error の安定性（仕様 §26.5） |
| RC-03 | feature flag の粒度と JSON debug mode の feature 名 | crate 単位 feature、`json-debug` | TB-02 の JSON debug mode。初期は無効 |
| RC-04 | ファイル分割の目安と public re-export の管理 | 500 行超過で分割検討、`lib.rs` で re-export を明示 | 仕様 §31.2。行数は目安であり責務の単一性を優先 |
