# OrbiSync 技術選定と意思決定候補

## 1. 判断の扱い

本書は `metaverse_core_specification.md` §8 の推奨技術スタックを、採用理由、代替案、制約、確定に必要な ADR へ展開する。仕様で指定された技術を不用意に再選定しない一方、仕様が選択肢を残す事項は確定扱いにしない。各節の状態欄または段落冒頭のラベルで確定度を判別する。

| 状態 | 意味 |
|---|---|
| [SPEC] 仕様由来の確定事項 | 仕様書で指定され、初期設計の前提とする |
| [REC] 設計上の推奨 | 要求から妥当な初期案だが、レビューまたは後続仕様との照合が必要 |
| [ADR] ADR 待ち | 互換性、セキュリティ、運用に影響し、実装前の決定記録が必要 |

## 2. 採用技術

### TD-01 Rust / Edition 2024

**状態:** [SPEC]

**理由:** 単一バイナリ配布、予測可能な性能、メモリ安全性、強い型による protocol/domain 境界の表現、async I/O に適する。モジュラーモノリスの依存規則を可視性と型で強制しやすい。

**代替案:** Go は配布と並行処理が簡潔、JVM/Kotlin は成熟した業務基盤、TypeScript は SDK と言語統一が可能。ただし、いずれも原仕様の実装言語指定を変更するため初期案では採用しない。

**制約:** Edition 2024 対応 toolchain を明記し、最小サポート Rust バージョン（MSRV）を別途決める。`unsafe` の利用方針、ビルド target、パニック方針は未決定。

**コーディング規約（仕様 §31）:**
- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`
- `unsafe` は原則禁止。必要な場合は ADR、SAFETY コメント、専用テスト必須
- `unwrap()`/`expect()` を本番 path で原則禁止
- `panic!` を入力エラー処理に使わない
- public item へ rustdoc
- error は型付きで扱う

### TD-02 Tokio

**状態:** [SPEC]

**理由:** Axum/SQLx と整合する非同期実行基盤であり、多数接続と I/O 中心の処理に適する。

**代替案:** async-std、smol。採用スタックとの統合コストと ecosystem の一貫性から初期候補外とする。

**制約:** 非同期 task の無制限生成、blocking 処理の runtime worker 上実行、unbounded channel を禁止する設計規約が必要。CPU 集約処理（Argon2id ハッシュ等）の隔離方法と runtime 設定は負荷試験で決める。

### TD-03 Axum（HTTP / WebSocket）

**状態:** [SPEC]

**理由:** Tokio と統合でき、REST と WebSocket の入口を同一サーバー技術で構成できる。tower middleware により認証、trace、timeout、body limit 等を入口へ一貫適用しやすい。

**代替案:** Actix Web、Warp、Poem。性能だけでなく保守性、middleware、OpenAPI 生成との整合を比較対象にできるが、原仕様から変更する根拠は現在ない。

**制約:** Axum 型を adapter 外へ漏らさない。WebSocket は WSS 公開を必須とするが、TLS 終端の配置は要 ADR。request/frame size、timeout、origin、rate limit の値も別途確定する。

### TD-04 Protocol Buffers（本番リアルタイム通信）

**状態:** [SPEC]

**理由:** 言語非依存、コンパクトなバイナリ、schema から複数 SDK を生成可能で、互換性規則を明示できる。4G/5G 環境での帯域効率に寄与する。

**代替案:** MessagePack、CBOR、FlatBuffers、JSON。JSON は可観測性と手動デバッグに優れるが帯域効率が劣るため、本番正本ではなく任意の debug mode に限定する（仕様 §13.3）。

**制約:** `.proto` を公開契約の正本とする。field number の再利用禁止、unknown field、enum evolution、破壊的変更判定、生成器バージョン、WebSocket subprotocol/version negotiation はプロトコル ADR で決める。

**メッセージ上限（仕様 §14.4 推奨初期値）:**
- 通常リアルタイムメッセージ: 16 KiB 以下
- カスタムイベント: 64 KiB 以下
- スナップショット: 分割送信可能
- 圧縮後・展開後の両方に上限を設定

### TD-05 JSON + OpenAPI（管理 REST API）

**状態:** [SPEC]

**理由:** 管理ツール、外部統合、手動検証との相互運用性が高く、OpenAPI からドキュメントとクライアント生成材料を提供できる。

**代替案:** 管理 API も gRPC、GraphQL。初期のセルフホスト運用や広いクライアント互換性に対して複雑性が増すため採用しない。

**制約:** Rust 型から OpenAPI を生成するか、OpenAPI-first で生成するかは未決定。error envelope（仕様 §21.8）、pagination、idempotency、API versioning と破壊的変更検査を ADR 化する。

**エラーフォーマット（仕様 §21.8）:**
- `code` は機械可読で安定させる
- `message` は人間向けであり互換性を保証しない
- 内部スタックトレースを返さない

### TD-06 PostgreSQL

**状態:** [SPEC]

**理由:** ユーザー、権限、ワールド定義、監査等の整合性を必要とする永続データに適し、小規模の Docker Compose 運用と将来の運用拡張の双方に対応しやすい。

**代替案:** SQLite は単一ホストで簡単だが、接続並行性と将来分離の標準基盤にしにくい。NoSQL/KV は一部の一時状態に適し得るが、初期の追加 datastore は運用負担を増やす。Redis を初期必須にしない。

**制約:** 一時状態を無差別に DB へ書き込まない（仕様 §10.1）。schema migration、バックアップ/復元、対応 PostgreSQL version、connection pool、トランザクション境界を決める必要がある。

**マイグレーション方針（仕様 §22.6）:**
- 前方移行を基本とする
- 破壊的変更は複数リリースに分ける
- Expand → Migrate → Contract 方式を推奨
- リリース前に既存 DB からの移行テストを行う

### TD-07 Argon2id

**状態:** [SPEC]

**理由:** パスワード保存に memory-hard な方式を用い、ローカル ID・パスワード認証を安全に実装する原仕様の指定である。

**代替案:** bcrypt、scrypt、PBKDF2。既存ハッシュ移行の入力にはなり得るが、新規保存形式の初期選択を変えない。

**制約:** salt はユーザーごとに安全な乱数を用いる。pepper の要否、parameter、再ハッシュ判定、最大入力長、secret 管理、認証 rate limit はセキュリティ ADR が必要。具体 parameter は配備環境の計測なしに固定しない。

**[REC]** パスワードハッシュ処理を async worker で直接実行せず、CPU blocking 処理として隔離する（`spawn_blocking` 等）。

### TD-08 `tracing`

**状態:** [SPEC]

**理由:** 非同期処理を span と相関 ID で追跡し、ログと trace の文脈を統合しやすい。

**代替案:** `log` facade のみ、独自 logging。非同期の要求・接続・instance command の関連付けに不足するため採用しない。

**制約:** パスワード、token、cookie、authorization header、不要な個人情報を記録しない（仕様 §26.4, §27.1）。ログ形式、trace exporter、sampling、correlation ID の信頼境界を決める。

**構造化ログ必須フィールド（仕様 §27.1）:** timestamp, level, service, version, request_id, connection_id, session_id, instance_id, user_id, event, duration_ms, error_code

### TD-09 Prometheus 互換メトリクス

**状態:** [SPEC]

**理由:** セルフホストで一般的な pull 型監視へ接続でき、特定 SaaS を必須にしない。

**代替案:** OpenTelemetry metrics のみ、vendor SDK。export adapter としては将来追加可能だが、初期の公開互換面を置き換えない。

**制約:** UserId、ConnectionId、WorldInstanceId など高 cardinality 値を label にしない。metrics endpoint の認証/公開範囲、命名、bucket は運用 ADR で確定する。

**最低限のメトリクス（仕様 §27.2）:** http_requests_total, websocket_connections_current, instance_members_current, instance_command_queue_depth, outbound_queue_depth, state_updates_dropped_total, resume_attempts_total, auth_login_failures_total, db_query_duration_seconds 等

### TD-10 OCI イメージ / Docker Compose

**状態:** [SPEC]

**理由:** 単一バイナリと PostgreSQL を再現可能に配布し、新規コントリビューターと小規模セルフホスト環境の起動を容易にする。

**代替案:** ネイティブパッケージ、Kubernetes chart。将来追加できるが、初期標準運用を複雑化するため必須にしない。

**制約:** non-root、read-only root filesystem の可否、multi-arch、image provenance/SBOM、healthcheck、graceful shutdown、設定/secret 注入方式を決める。Compose は本番の TLS/backup/monitoring を自動的に保証しない。

## 3. データアクセス技術

### TD-11 SQLx

**状態:** [REC]（仕様は「SQLx 等」）

**推奨理由:** Tokio 対応、明示 SQL、compile-time query check を利用でき、repository adapter の SQL を見通しよく保てる。

**代替案:**

- Diesel: 強い query builder/schema 管理。同期モデルまたは async 周辺の設計を比較する必要がある
- SeaORM: CRUD 開発速度は高いが、domain model と ORM entity の混同を避ける追加規律が必要
- tokio-postgres: 低レベルで制御可能だが、migration/query 検査を別途構成する必要がある

**推奨制約:** SQLx を採用しても DB row を domain model と兼用しない。offline metadata の CI 運用、migration tool、動的 query の検査方法を ADR で確定する。

## 4. SDK と契約ツールチェーン

### TD-12 TypeScript 公式 SDK

**状態:** [REC]

**理由:** Web と Node.js の参照実装を低い導入障壁で提供でき、protocol/OpenAPI の利用例として適する。

**代替案:** Dart、C#、Swift、Kotlin、Rust、Go。将来追加対象であり、需要とメンテナー確保に基づき選ぶ。

**制約:** SDK を仕様の正本にしない（仕様 §8.2）。生成コードと手書きの接続状態機械を分離し、`.proto`、OpenAPI、状態遷移、test vector に対する CI を置く。npm package 名、対応 runtime、semantic versioning は要 ADR。

**SDK の責務（仕様 §24.1）:** REST 認証、Token 更新、WebSocket 接続、ClientHello、入退室、Protobuf encode/decode、自動再接続、Resume、Snapshot/Delta 適用、sequence 管理、heartbeat、イベント購読、latest-wins 送信キュー、エラー型

### TD-13 Protocol/OpenAPI 生成器

**状態:** [ADR]

**推奨案:** リポジトリに schema と生成設定を置き、CI で再生成差分と互換性を検査する。生成物を commit するか release artifact のみにするかは、利用者の導入容易性と差分レビュー性で決める。

**候補:** Prost/Tonic build toolchain、Buf 等の schema lint/breaking change 検査、OpenAPI generator 系。特定製品の採用は保守・ライセンス・再現性を評価後に確定する。

## 5. 依存関係とサプライチェーン方針

**[SPEC]** crate version は仕様書で固定せず、`Cargo.lock` と自動更新、定期リリースで管理する（仕様 §8.1）。

**[REC]** 実装開始時の推奨方針は次のとおり。

- server binary の `Cargo.lock` は commit し、CI/release の再現性を確保する
- 更新 PR ごとに unit/contract/integration/compatibility tests を実行する
- security advisory と license を検査する（`cargo deny` 等）
- major update と protocol/storage に影響する update は自動 merge しない
- build/code generation tool の version も pin する
- リリース image は source revision と依存情報へ追跡可能にする

Dependabot と Renovate のどちらを採用するか、更新頻度、許容 license、脆弱性 SLA はリポジトリ運用 ADR が必要である。

## 6. 横断的な技術制約

### 6.1 セキュリティ（仕様 §26.3）

**[SPEC]** 以下は仕様 §26.3 に基づく確定要件である。

- Internet からは HTTPS/WSS のみを公開する
- 全管理操作と状態変更は認証・認可を application 境界で実施する
- 入力のサイズ、頻度、値域を transport と domain の両方で検証する
- secret と資格情報を image、設定ファイル既定値、telemetry に含めない
- 第三者コードをコアプロセス内へロードしない
- SQL injection 対策、CSRF 対策（Cookie 認証時）、CORS デフォルト拒否、origin 検証

### 6.2 性能と信頼性

- unbounded queue、無制限 task、無制限 payload を採用しない
- **[SPEC]** モバイル回線の切断と遅延から復帰できる設計とする（仕様 §16）
- **[REC]** 重複と順序変化への耐性は sequence / idempotency ADR で protocol ごとに評価する
- Prometheus label と log field の cardinality を制御する
- 1000 人同一空間の保証値は置かず、負荷試験結果と SLO を別途記録する
- 外部拡張と exporter の latency を正準状態更新のクリティカルパスへ入れない案を優先する
- DB をリアルタイム hot path へ置かない（仕様 §25.4）

**レイテンシ目標（仕様 §26.2、同一地域内アプリケーション処理）:**
- P50: 10 ms 以下
- P95: 50 ms 以下
- P99: 100 ms 以下

### 6.3 可搬性

- **[SPEC]** 少なくとも Linux server 上の単一バイナリと OCI image / Docker Compose 構成で検証・配布可能にする
- **[ADR]** 他 OS/architecture のサポート範囲、CI matrix、サポート期間は別途決める
- PostgreSQL 以外の追加インフラを初期必須にしない
- クライアント契約は特定言語、UI、renderer、asset format に依存させない

### 6.4 時刻（仕様 §31.4）

**[SPEC]** 以下は仕様 §31.4 に基づく確定要件である。

- DB 保存は UTC
- API は RFC 3339
- realtime protocol は Unix milliseconds 等、明示形式
- テストでは Clock trait を注入する

## 7. ADR バックログ

優先度は、公開契約または不可逆な永続形式への影響を基準とする。

| 優先度 | ADR | 決める内容 | 決定タイミング |
|---|---|---|---|
| Accepted | ADR-001 命名と識別子 | `OrbiSync`、`orbisync-server`、`orbisync-*`、`orbisync.v1`、UUIDv7 | 2026-07-31決定 |
| Accepted | ADR-002 認証セッション | 15分Ed25519 JWT、30日rotating opaque refresh、60秒Realtime ticket | 2026-07-31決定 |
| Accepted | ADR-003 API versioning | OpenAPI-first、`/v1`、cursor、24h idempotency、`If-Match` | 2026-07-31決定 |
| Accepted | ADR-004 Realtime protocol evolution | Protobuf/Buf、major拒否、開発時JSON、初期圧縮なし | 2026-07-31決定 |
| Accepted | ADR-005 永続化境界 | SQLx、PostgreSQL 16/17、module-local transaction、outbox | 2026-07-31決定 |
| Accepted | ADR-006 Instance concurrency | actor、bounded mailbox 4096、20Hz、30秒drain | 2026-07-31決定 |
| Accepted | ADR-006A Sequence/idempotency | 接続別sequence、UUIDv7 message/command ID、24h dedup | 2026-07-31決定 |
| Accepted | ADR-007 拡張 API | Webhook + REST、HMAC、retry 5回、DLQ 7日 | 2026-07-31決定 |
| Accepted | ADR-008 観測・監査 | JSON log、Prometheus、OTLP、PG append-only audit | 2026-07-31決定 |
| Accepted | ADR-009 TLS と配置 topology | 1 deployment=1 organization、proxy TLS、1 process/DB | 2026-07-31決定 |
| P1 | ADR-010 依存更新 | Dependabot/Renovate、MSRV、license/security SLA | CI 構築時 |
| P2 | ADR-011 SDK release | TypeScript package、生成物、runtime support、version coupling | SDK 公開前 |
| P2 | ADR-012 将来分離基準 | metrics と閾値、remote contract の導入条件 | 性能計測後 |

## 8. 現時点で採用しないもの

初期アーキテクチャでは、内部 gRPC、service mesh、Kubernetes 必須化、Redis 必須化、message broker 必須化、plugin runtime（WASM 含む）、外部 IdP、multi-tenant datastore を採用しない。これらを禁止するのではなく、現時点の目標に必要性がなく、運用面と障害面を増やすためである。実測または具体ユースケースが生じた場合に ADR で再評価する（仕様 §41）。
