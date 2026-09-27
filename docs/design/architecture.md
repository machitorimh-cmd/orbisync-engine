# OrbiSync アーキテクチャ設計

## 0. 表記規則

本書では、判断の根拠と確定度を次のラベルで区別する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` で要求・禁止される事項
- **[REC] 設計上の推奨**: 要求を実装へ落とす案であり、レビューまたは後続仕様との照合対象
- **[ADR] ADR 待ち**: 複数案が成立し、まだ確定しない事項

表では状態列またはセル内ラベル、本文では段落冒頭のラベルを用いる。

## 1. アーキテクチャ目標

**[SPEC]** 初期版は単一 Rust プロセスのモジュラーモノリスとする（仕様 §7.1）。目的は、単一バイナリで容易に配布・運用しつつ、変更理由、データ所有権、依存方向をモジュール単位で明確にし、実測に基づく将来分離を可能にすることである。

**[REC]** マイクロサービス化、内部 gRPC、分散トランザクションは初期構成へ持ち込まない。論理境界は Rust の型、trait、コマンド/イベントで表現する。

**[SPEC]** 公式フロントエンドで実行できる操作は、すべて公開 API または公開プロトコルからも実行できなければならない（仕様 §3.2）。

## 2. レイヤーと依存方向

```text
┌────────────────────────────────────────────────────────────┐
│ Inbound adapters                                           │
│ REST / WebSocket / management / generated protocol types   │
└──────────────────────────┬─────────────────────────────────┘
                           │ DTO → command/query
                           ▼
┌────────────────────────────────────────────────────────────┐
│ Application                                                │
│ use cases / authorization orchestration / transaction      │
│ boundaries / ports                                         │
└──────────────────────────┬─────────────────────────────────┘
                           │ domain types
                           ▼
┌────────────────────────────────────────────────────────────┐
│ Domain                                                     │
│ entities / value objects / policies / domain errors/events │
└────────────────────────────────────────────────────────────┘
                           ▲
                           │ implements application/domain ports
┌──────────────────────────┴─────────────────────────────────┐
│ Outbound adapters                                          │
│ PostgreSQL / extension delivery / telemetry / clock / ids  │
└────────────────────────────────────────────────────────────┘
```

許可する依存は `adapter → application → domain` である（仕様 §7.3）。実装上の共通型は、上位レイヤーが Axum、SQLx、PostgreSQL、WebSocket に逆依存しない位置へ置く。

### 2.1 強制するルール

- Domain はフレームワーク crate、DB 行型、HTTP/Protocol DTO を import しない。
- Application の公開関数は Axum extractor、HTTP status、SQLx transaction/row、WebSocket frame を引数・戻り値にしない。
- Inbound adapter は入力を構文検証した後、application command/query へ明示変換する。
- Outbound adapter は application/domain が所有する port を実装する。
- Protocol DTO と domain model の相互変換は adapter 境界に閉じ込める。
- モジュール間で相手の repository 実装や DB テーブルを直接利用しない。
- 循環依存を禁止する。共有が必要な小さな概念は安易な `common` に集約せず、所有モジュールまたは明示的な kernel に置く。

## 3. モジュール境界

以下は **[REC]** の論理モジュールであり、初期は同一プロセスへリンクする。crate 分割数は **[ADR]** だが、境界と依存規則はディレクトリ構成に関係なく維持する。

| モジュール | 所有する責務・状態 | 公開する主な境界 | 呼出方向/依存してよい対象 |
|---|---|---|---|
| `identity_access` | 認証、ユーザー、ロール、権限、Identity Session、token/失効。`AuthSessionId` を所有 | authenticate、validate/revoke auth session、authorize、user/role commands/queries | 自身の domain、必要最小限の ports |
| `world_directory` | ワールド定義、インスタンスの作成/列挙/ライフサイクル調停 | world/instance commands/queries、instance locator | identity の主体/権限結果、ports |
| `realtime_presence` | Realtime Connection / Presence のメタデータ、入退室、resume binding。`RealtimeConnectionId` と `PresenceId` を所有し、socket と Identity Session は所有しない | bind/unbind/resume presence commands、presence events/queries | coordinator から渡された認証済み主体、ports |
| `instance_runtime` | ワールドインスタンスの正準な一時状態、入力直列化、状態遷移、Entity/Transform の権威管理 | runtime command mailbox、snapshot/read model、domain events | domain policy、persistence ports。`interest` や delivery を呼ばない |
| `interest` | state/visibility view から recipient ID 集合を計算する I/O なしの policy/domain service。Uniform Spatial Grid を標準とする（仕様 §17.2） | calculate recipients | domain value/view のみ。connection queue、socket、gateway に依存しない |
| `realtime_delivery` | 接続別 bounded queue と delivery command の調停。socket 自体は所有しない。latest-wins 集約と reliable イベントの分離を行う（仕様 §16.5, §18） | enqueue/drop/query pressure | Application が所有する outbound sink port |
| `eventing` | 汎用イベントの内部配布と外部配送要求 | internal event publisher、outbox request | ports。ドメインを逆参照しない |
| `persistence` | PostgreSQL adapter、repository/outbox 実装、マイグレーション。一時状態と永続状態の分離を遵守する（仕様 §10） | 各所有モジュールが定義する port の実装 | SQLx/PostgreSQL、port の型 |
| `audit_observability` | 監査記録 port、ログ/メトリクス/トレース adapter | audit sink、telemetry facade | tracing/metrics adapter、安定した監査 DTO |
| `extension_gateway` | 外部サービスとの認証、配送、タイムアウト、再試行。Out-of-process Extension のみ（仕様 §23） | extension request/event port | HTTP/gRPC 等の adapter、eventing |
| `http_api` | REST/OpenAPI、認証コンテキスト、管理/制御 API adapter | HTTP routes | application use cases |
| `realtime_gateway` | WSS socket の所有、Protocol Buffers/任意 debug JSON、接続 I/O | protocol handshake、inbound frame mapping、Application-owned outbound sink port の実装 | Application の contracts/use cases。`realtime_delivery` 実装には依存しない |
| `load_test_tooling` | Core 実行サーバー外から公開 API/protocol を駆動する負荷試験ツール | scenario、load profile、測定結果 | 公開 OpenAPI / `.proto` / 状態遷移のみ。サーバー内部 module に依存しない |

### 3.1 所有権と非循環の呼出方向

**[REC]** Feature module 同士を直接 orchestration させず、Application coordinator が use case の順序を所有する。

```text
HTTP / Realtime Gateway
          │
          ▼
Application coordinator
  ├──► identity_access       (REST access token / Realtime ticket を検証)
  ├──► world_directory
  ├──► realtime_presence     (RealtimeConnectionId / PresenceId を binding)
  ├──► instance_runtime      (正準状態を更新し view/event を返す)
  ├──► interest              (view → recipient IDs、I/O なし)
  └──► realtime_delivery     (recipient IDs → queue → gateway outbound sink)
```

矢印の逆向き呼出しは禁止する。特に `interest` は `realtime_presence`、`realtime_delivery`、`realtime_gateway` を呼ばず、`instance_runtime` も `interest` や delivery を呼ばない。この DAG により recipient 計算と socket 配信の循環を作らない。

compile-time の依存も `realtime_gateway (adapter) → Application contracts ← realtime_delivery` とし、Gateway と Delivery は相互 import しない。実行時の socket write は Application-owned outbound sink port の呼出しとして表現し、Gateway がその adapter を実装する。

Identity SessionとRealtimeの呼出方向は`REST ticket endpoint → identity_access.validateAccessToken → issue one-time ticket`、続いて`Realtime Gateway → Application coordinator → identity_access.consumeRealtimeTicket → authenticated subject + AuthSessionId → coordinator → realtime_presence.bind(RealtimeConnectionId, PresenceId, authenticated subject)`とする（ADR-002）。raw token/ticketは`realtime_presence`へ渡さない。`realtime_presence`から`identity_access`を呼ばず、`AuthSessionId`をRealtimeの主キーとして再利用しない。

### 3.2 境界をまたぐ型

モジュール間契約は次の種類に限定する。

- **Command**: 状態変更の意図。送り手の transport 型を含まない
- **Query**: 読み取り要求。DB row を返さない
- **Event**: 既に成立した事実。所有モジュールのみが発行する
- **Port**: 永続化、時刻、ID、外部配送など副作用の抽象
- **Stable ID / value**: `UserId`、`WorldId`、`WorldInstanceId`、`ConnectionId` 等

**[SPEC]** Primitive Obsession を避け、`UserId(Uuid)`、`InstanceId(Uuid)`、`LoginId(String)`、`Revision(u64)` 等の newtype を使用する（仕様 §31.3）。同一の構造に見えても protocol 型、application 型、domain 型、persistence 型を暗黙に兼用しない。変換コードを互換性変更の検知点とする。

### 3.3 データ所有権

各永続テーブル/一時状態には所有モジュールを一つだけ割り当てる。他モジュールは所有者の query/port を通じて参照し、直接更新しない。

**[SPEC]** 一時状態（位置、向き、速度、接続状態、Interest 購読）は主にメモリで管理し、通常は毎更新 DB へ保存しない（仕様 §10.1）。永続状態（ユーザー、ロール、ワールド定義、監査ログ）は PostgreSQL へ保存する（仕様 §10.2）。

**[ADR]** ADR 確定前は、module 所有をまたぐ atomic transaction を一般要件としない。原則は各 module 所有データの local transaction である。

**[REC]** 厳密な横断 atomicity が必要と確認された use case に限り、Application が所有する Unit of Work port を設計候補とする。許可対象は ADR で列挙し、module が他 module の repository/table を直接操作することは許可しない。外部副作用を同一 DB transaction に含めず、outbox の要否も同 ADR で決める。

## 4. 実行モデル

### 4.1 プロセス構成

```text
Tokio runtime
├── HTTP listener / REST request tasks
├── WebSocket connection tasks
├── Instance runtime tasks (logical owner per world instance)
├── outbound delivery tasks
├── persistence/outbox workers
└── telemetry/export tasks
```

**[SPEC]** 各 World Instance は、単一の論理所有者が状態更新を直列化する Actor 形式を推奨する（仕様 §12.1）。bounded channel を使用し、キュー上限を設定する（仕様 §12.2）。

**[REC]** 共有状態を無制限な `Arc<Mutex<_>>` で横断共有せず、インスタンス単位のコマンド入口を設ける。

**[ADR]** 順序保証、mailbox 上限、tick の有無と値は後続仕様と ADR に委ねる。仕様 §12.3 はサーバー状態 tick 10〜20 Hz を推奨初期値とする。

### 4.2 サーバー権威モデル

**[SPEC]** サーバーがワールドの正しい状態を決定する（仕様 §11.1）。クライアントは状態を確定するのではなく、入力または状態変更要求を送る。最低限の入力検証（仕様 §11.2）として以下を実施する：

- 認証済みセッションか
- 対象インスタンスへ参加中か
- 対象エンティティを操作できるか
- 数値が有限か（NaN/Infinity 拒否）
- 最大速度・最大更新頻度・最大移動距離を超えていないか
- ワールド境界を超えていないか
- メッセージサイズ上限内か
- プロトコルバージョンが互換か

### 4.3 Composition Root

起動処理だけが具体 adapter を選び、port へ注入する。ドメイン/アプリケーションコード内で PostgreSQL pool、HTTP client、グローバル設定を取得しない。

**[REC]** 起動順序は、設定検証 → telemetry 初期化 → DB 接続/マイグレーション確認 → adapter 構築 → background worker 起動 → listener 公開とする。終了時は新規受付停止 → 接続/command の drain → 永続化要求の flush → task 終了とする（仕様 §37.3）。**[ADR]** 具体的な drain 期限は要 ADR。

## 5. 主要処理フロー

### 5.1 認証とトークン発行

```text
HTTP adapter
  → Authenticate command（DTO変換・入力上限）
  → identity_access application service
  → CredentialRepository port
  → Argon2id verifier port
  → Identity Session/Token port（AuthSessionId）
  → Audit port
  → response DTO → JSON
```

**[SPEC]** 認証失敗の外部応答では ID 存在有無を露出しない（仕様 §26.3）。トークン形式、期限、更新方式、失効モデルは仕様 §19.4 で推奨モデルが示されているが、具体値は未決定のため ADR が必要である。

### 5.2 WebSocket 接続と入室

```text
WSS/protocol adapter
  → Application coordinator
  → identity_access: validateAccessToken(token) → authenticated subject (+ AuthSessionId)
  → realtime_presence: bind RealtimeConnectionId / PresenceId
  → world_directory: resolve and authorize target
  → instance_runtime: join command
  → snapshot query/result
  → interest: calculate visible entity/presence set for joining subject（I/O なし）
  → realtime_delivery: enqueue filtered snapshot + presence
  → realtime_gateway: socket write
```

**[SPEC]** 入室時の Snapshot は Interest Management 適用後の状態を送る（仕様 §15.3）。1000人全員分を無条件に送らない。

接続 I/O task は正準ワールド状態を所有しない。入室の成立点と、途中失敗時の補償（presence 取消など）は application service が調停する。

### 5.3 リアルタイム状態更新

```text
binary frame
  → size/version/message validation
  → protocol DTO → application command
  → coordinatorによる identity/realtime connection/ownership authorization
  → instance_runtime mailbox
  → domain validation and canonical state transition
  → state view/domain event を coordinator へ返す
      ├── interest: view → recipient IDs（I/O なし）
      │     └── realtime_delivery → connection queues → gateway socket
      ├── persistence port（永続対象のみ）
      ├── audit port（監査対象のみ）
      └── eventing/extension port（公開対象のみ）
```

**[SPEC]** ネットワーク受信順を、そのまま複数 task から共有状態へ適用してはならない（仕様 §11）。同一インスタンス内の競合を一つの順序決定点へ集約する。

**[SPEC]** 位置更新は送信キュー上で同一エンティティの古い更新を上書きする（latest-wins、仕様 §16.5）。Reliable イベントは破棄してはならない。最新状態とイベントは別キューまたは別優先度で処理する。

### 5.4 再接続

```text
new WSS connection
  → identity_access: validateAccessToken(token) → authenticated subject (+ AuthSessionId)
  → realtime_presence: validate resume binding separately
  → allocate new RealtimeConnectionId and locate prior PresenceId
  → validate world instance availability
  → rebind or create presence
  → delta replay if valid, otherwise full snapshot
  → rebuild interest subscriptions
```

**[SPEC]** 公式 SDK は指数バックオフ + jitter による自動再接続を実装する（仕様 §16.2）。Resume Token は短時間のみ有効、推測困難、サーバー側で無効化可能（仕様 §16.3）。

再接続は最適化であり、復帰材料が無効・期限切れ・不足なら安全な完全再同期へ移行する。復帰材料（Resume Token の寿命・binding・無効化）、replay buffer、差分再同期、heartbeat、Interest 再購読、接続キューと slow consumer 保護の詳細は `mobile-resume-interest-backpressure.md` で定義する。

### 5.5 外部イベント配送

```text
domain/application event
  → public event mapping
  → transactional outbox（推奨）
  → extension delivery worker
  → timeout/retry/idempotency handling
  → success/failure telemetry and audit where required
```

**[SPEC]** 第三者のネイティブ動的ライブラリをコアプロセスへ直接ロードしない（仕様 §23.1）。Out-of-process Extension を採用する。

**[SPEC]** Webhook は HMAC 署名、timestamp、event ID、再送、exponential backoff、dead-letter 記録、重複配信前提、受信側 idempotency を満たす（仕様 §23.4）。

**[REC]** 外部サービスへの同期呼び出しを instance runtime の状態遷移クリティカルパスへ置かない。

## 6. エラーと障害の境界

- **[SPEC]** Transport エラーは公開エラーコードへ変換し、内部 DB/ライブラリエラーを露出しない（仕様 §21.8）。
- **[REC]** Domain rejection は期待された入力拒否として扱い、プロセス障害と区別する。
- **[REC]** DB 障害時に永続更新を成功扱いしない。リアルタイム一時状態を継続できる範囲は状態分類確定後に決める。
- **[SPEC]** 接続単位の不正 frame や遅い受信者が、インスタンス全体を停止させない隔離境界を置く（仕様 §18.1）。
- **[REC]** 外部拡張、監査 exporter、telemetry exporter の停止を instance runtime へ無制限に伝播させない。
- **[REC]** panic は要求/connection/task 境界で観測し、正準状態の破損可能性がある runtime を無条件に継続しない。

## 7. バックプレッシャーと Interest Management

### 7.1 バックプレッシャー

**[SPEC]** 遅いクライアント 1 台がインスタンス全体を遅延させてはならない（仕様 §18.1）。

- 接続ごとに bounded queue、メッセージ種別ごとの優先度、latest-wins 状態の上書き
- reliable イベントの上限、上限超過時は警告後に切断可能
- 接続単位・ユーザー単位・IP 単位・インスタンス単位の rate limit（仕様 §18.4）

### 7.2 Interest Management

**[SPEC]** Uniform Spatial Grid を標準とする（仕様 §17.2）。各エンティティを現在位置のセルへ登録し、クライアントは自身のセルと周辺セルを購読する。

**[REC]** ヒステリシスを設け、subscribe radius と unsubscribe radius を分ける（仕様 §17.5 例: 30m/35m）。

**[SPEC]** 可視性ポリシーとして Global、Spatial、OwnerOnly、RoleRestricted、Explicit、Custom を提供する（仕様 §17.4）。

## 8. 将来の分離点

| 分離候補 | 現在の境界 | 分離を検討する観測条件 | 分離時に必要となる追加契約 |
|---|---|---|---|
| API Gateway | `http_api` / `realtime_gateway` | 接続数、TLS、独立スケールが runtime と乖離 | routing、auth context、rate limit、backpressure |
| Authentication Service | `identity_access` ports | IdP 追加、組織横断運用、独立したセキュリティ更新 | token validation、key rotation、revocation |
| World Directory | `world_directory` | runtime ノードが増え配置解決が必要 | service discovery、lease、instance routing |
| Instance Runtime | command mailbox / events | CPU/メモリ/障害隔離、ワールド別水平配置が必要 | remote command protocol、ordering、ownership、failover |
| Persistence Worker | persistence/outbox ports | 書込が runtime latency を圧迫 | durable queue、idempotency、delivery semantics |
| Audit/Event Exporter | audit/event ports | 保持・検索・外部 SIEM 要件が DB と乖離 | schema versioning、redaction、delivery guarantee |

分離は「将来可能」であり、予定ではない。境界越し呼び出し量、レイテンシ、障害モード、運用負担を計測し、単一プロセスの最適化では目標を満たせない場合に ADR を作成する。

## 9. 検証可能なアーキテクチャ適合条件

実装時には次を CI で検査できる構成を推奨する。

1. Domain package の依存グラフに Axum、SQLx、protocol 生成物が存在しない。
2. Application service の公開 API に HTTP/DB/WebSocket 型が存在しない。
3. モジュール依存グラフに循環がない。
4. Protocol/OpenAPI 変換に contract tests がある。
5. repository/extension/audit port は fake adapter で use case test が可能である。
6. 公開操作が公式参照実装固有の経路を必要としない。
7. モジュール所有外のテーブルを直接更新する query がない。
8. `unsafe`、`unwrap()`/`expect()` の本番 path 使用が lint で検出される（仕様 §31.1）。

## 10. 要 ADR 事項

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| ARC-01 | crate/workspace の分割粒度 | まず境界ごとの top-level module と domain/application/adapter の可視性を厳格化し、独立コンパイル価値が高い protocol/domain から crate 化 | 過剰な crate 分割を避けつつ依存違反を検査可能にする |
| ARC-02 | インスタンス内の状態所有・並行性 | インスタンスごとの単一 command 入口と bounded mailbox | 正準状態の競合点とバックプレッシャー点を一つにする |
| ARC-03 | モジュール横断トランザクション | module-local を原則とし、列挙された use case のみ Application-owned UoW を許可。外部副作用は outbox 候補 | table 所有権を破らず、必要性未確認の横断 atomicity を一般化しない |
| ARC-04 | 内部イベント配送 | 同一プロセス typed event、外部配送のみ durable outbox | 初期の不要なネットワーク境界を避ける |
| ARC-05 | runtime 障害復旧 | 永続/一時状態分類と checkpoint 要件確定後に選択 | 仕様 §10 では分類のみで復元具体策は未定義 |
| ARC-06 | API と runtime の rate/backpressure 方針 | 全入口と connection queue を bounded にし、上限値は負荷試験で決定 | モバイル回線と slow consumer からプロセスを保護する |
| ARC-07 | sequence / idempotency | transport ごとの順序・重複モデル、command/event の idempotency key 適用範囲 | 仕様 §14.3 の sequence と §16 の再接続を整合させる |
| ARC-08 | チェックポイント方式 | 手動・定期・インスタンス終了時の checkpoint、永続化対象コンポーネントのみ保存 | 仕様 §10.3。高頻度状態の毎更新 DB 書込を避ける |

## 11. 実装開始前の設計ゲート

少なくとも以下が確定するまで、公開契約や永続スキーマを固定しない。

- 名前・ID 体系とモジュール語彙
- 認証トークン/セッション/失効方式
- ドメインモデルと一時/永続状態の所有権
- Protocol Buffers の versioning と状態遷移
- インスタンス実行・順序・backpressure モデル
- 外部拡張の配送契約
- 監査対象、保持、機密情報除去

これらは後続設計文書との整合確認後、それぞれ ADR または契約文書として確定する。
