# OrbiSync ドメインモデル設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §9 を、集約・Entity・Value Object・ID の設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: 仕様書で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項

モジュール所有権は `architecture.md` §3 に従う。本書は所有モジュールを再定義しない。

## 1. 設計原則

**[SPEC]** Primitive Obsession を避け、ID は newtype で表現する（仕様 §31.3）。異なる ID を同じ `Uuid` 引数で取り違えない。

**[REC]** ドメインモデルは以下の DDD 概念で構成する：

- **Aggregate Root**: 整合性境界の単位。外部からの参照は ID を介す
- **Entity**: 同一性を持つオブジェクト。Aggregate Root も Entity の一種
- **Value Object**: 同一性を持たず、属性の等価性で比較する不変オブジェクト
- **Domain Event**: 既に成立した事実。所有モジュールのみが発行する

**[SPEC]** ドメイン層はフレームワーク crate、DB 行型、HTTP/Protocol DTO を import しない（仕様 §7.3）。

## 2. ID 体系

**[SPEC]** 以下の ID を newtype として定義する（仕様 §31.3）：

| ID 型 | 内部表現 | 所有モジュール | 用途 |
|---|---|---|---|
| `UserId` | UUIDv7 | `identity_access` | ユーザーの内部不変 ID |
| `RoleId` | UUIDv7 | `identity_access` | ロール識別 |
| `WorldId` | UUIDv7 | `world_directory` | ワールド定義識別 |
| `InstanceId` | UUIDv7 | `world_directory` | ワールドインスタンス識別 |
| `EntityId` | UUIDv7 | `instance_runtime` | ワールド内エンティティ識別 |
| `AuthSessionId` | UUIDv7 | `identity_access` | Identity Session 識別 |
| `RealtimeConnectionId` | UUIDv7 | `realtime_presence` | WebSocket 接続識別 |
| `PresenceId` | UUIDv7 | `realtime_presence` | インスタンス参加状態識別 |
| `Revision` | `u64` | 複数 | 楽観的並行制御・差分追跡 |

**設計前提** ADR-001でIDの内部表現をUUIDv7に確定した。RustではIDごとのnewtype、PostgreSQLではnative `UUID`、公開transportではcanonical lowercase hyphenated UUID stringを使用する（`docs/adr/ADR-001-naming-identifiers.md`）。

**[REC]** `LoginId` は `String` の newtype とし、管理者が変更可能な外部表示用識別子として `UserId` と分離する（仕様 §9.1）。

## 3. 集約と Entity

### 3.1 User 集約

**所有モジュール:** `identity_access`

**[SPEC]** 仕様 §9.1 に基づく：

```text
Aggregate Root: User
├── id: UserId（内部不変 ID）
├── login_id: LoginId（管理者変更可能）
├── display_name: String
├── status: UserStatus
├── must_change_password: bool
├── created_at: Timestamp
└── updated_at: Timestamp

Entity（別集約として分離）: Credential
├── user_id: UserId
├── password_hash: PasswordHash（Value Object）
├── password_changed_at: Timestamp
├── failed_login_count: u32
└── locked_until: Option<Timestamp>
```

**[SPEC]** 認証情報は User 本体と論理的に分離する（仕様 §9.1）。削除より無効化を優先する。

**[REC]** `UserStatus` は `Active` / `Disabled` を最小構成とする。追加状態は用途側がロールで表現する。

**[ADR]** ログイン試行制限の閾値、ロック期間、解除条件はセキュリティ ADR で決める。

### 3.2 Role / Permission

**所有モジュール:** `identity_access`

**[SPEC]** 用途固有ロール（Student、Teacher、Employee 等）をコアへ埋め込まない（仕様 §9.2）。コアは権限文字列を扱う。

```text
Entity: Role
├── id: RoleId
├── name: String
├── permissions: Vec<Permission>
└── description: Option<String>

Value Object: Permission
└── 名前空間付き文字列（例: "users.read", "entities.update.own"）
```

**[SPEC]** 初期版は RBAC。User は複数 Role を持てる。Deny rule は初期版では持たず、allow-only（仕様 §20.1）。

**[REC]** Permission 文字列の命名規則は `<resource>.<action>` または `<resource>.<action>.<scope>` とする。

**[ADR]** 管理 API 権限とワールド内権限の命名空間統合または分離は認可 ADR で決める（仕様 §20.2）。

### 3.3 WorldDefinition 集約

**所有モジュール:** `world_directory`

**[SPEC]** 仕様 §9.3 に基づく：

```text
Aggregate Root: WorldDefinition
├── id: WorldId
├── name: String
├── description: Option<String>
├── status: WorldStatus
├── default_spawn: Transform（Value Object）
├── capacity: u32
├── metadata: Metadata（Value Object、コアは内容を解釈しない）
├── revision: Revision
├── created_at: Timestamp
└── updated_at: Timestamp
```

**[SPEC]** ワールド定義は描画アセットを含まない。`metadata` はクライアント固有情報を格納できるが、コアは内容を解釈しない。最大サイズを設定する（仕様 §9.3）。

**[ADR]** `metadata` の最大サイズ、許可する JSON 深度、バリデーション方針は未決定。

### 3.4 WorldInstance 集約

**所有モジュール:** `world_directory`

**[SPEC]** 仕様 §9.4 に基づく：

```text
Aggregate Root: WorldInstance
├── id: InstanceId
├── world_id: WorldId
├── lifecycle: InstanceLifecycle
├── capacity: u32
├── created_at: Timestamp
├── started_at: Option<Timestamp>
└── revision: Revision
```

**[SPEC]** 同一 WorldDefinition から複数 Instance を生成できる（仕様 §9.4）。

**[REC]** `InstanceLifecycle` は `Created → Running → Stopping → Stopped` の状態遷移とする。

**[ADR]** インスタンスの自動生成規則、最大同時実行数、停止時の参加者扱いは未決定（仕様 §42.7）。

### 3.5 Entity 集約

**所有モジュール:** `instance_runtime`

**[SPEC]** 仕様 §9.6 に基づく：

```text
Aggregate Root: Entity
├── id: EntityId
├── instance_id: InstanceId
├── kind: EntityKind
├── owner: Option<UserId>
├── transform: Option<Transform>（Value Object）
├── state: EntityState
├── visibility: VisibilityPolicy（Value Object）
└── revision: Revision
```

**[SPEC]** ユーザーアバターもエンティティの一種として扱えるが、ユーザー識別情報とエンティティ状態は分ける（仕様 §9.6）。

**[SPEC]** クライアントの自己申告 owner を信用しない。サーバーが所有権を確定する（仕様 §20.3）。

**[REC]** `EntityKind` は `Avatar` / `Object` / `Trigger` 等を想定するが、用途固有の意味をコアが解釈しない。

### 3.6 セッションと参加状態

**[SPEC]** 認証セッションとワールド接続セッションを分離する（仕様 §9.5）：

| 概念 | 所有モジュール | 識別子 | ライフサイクル |
|---|---|---|---|
| **Auth Session** | `identity_access` | `AuthSessionId` | ログイン → token 失効/期限切れ |
| **Realtime Connection** | `realtime_presence` | `RealtimeConnectionId` | WSS 接続確立 → 切断 |
| **Instance Membership** | `realtime_presence` | `PresenceId` | JoinInstance → connection close/Kick/Timeout（内部Leave command） |

**[SPEC]** WebSocket が切断されても、短い猶予時間内は Membership を保持できる（仕様 §9.5）。

**[REC]** 3 つのライフサイクルは独立して遷移する。`RealtimeConnectionId` の切断が `AuthSessionId` を失効させない。`PresenceId` の終了が `AuthSessionId` を失効させない。逆方向も同様。

**[ADR]** 猶予時間（resume grace）の具体値、切断中の Membership 可視性、二重参加の検知方式は再接続 ADR で決める。

## 4. Value Object

### 4.1 Transform

**[SPEC]** 仕様 §9.7 に基づく：

```text
Value Object: Transform
├── position: Vec3 { x: f32, y: f32, z: f32 }
├── rotation: Quaternion { x: f32, y: f32, z: f32, w: f32 }
└── scale: Vec3（任意機能、ユーザー移動では通常送信しない）
```

**[SPEC]** 単位系のデフォルトは 1 unit = 1 meter とする（仕様 §9.7）。

**[REC]** 座標系は右手系、Y-up、Quaternion 要素順は `x, y, z, w` とする（仕様 §9.7 の推奨）。

**[SPEC]** NaN、Infinity、範囲外値を拒否する。scale は任意機能とし、ユーザー移動では通常送信しない（仕様 §9.7）。

**[REC]** Transform の構築時（constructor）に有限性と正規化を検証し、不正値を含むインスタンスの生成を防止する。Quaternion の正規化許容誤差は **[ADR]**。

### 4.2 EntityState / Component

**[SPEC]** 仕様 §9.8 に基づく。初期実装では以下の 2 方式を併用：

1. **コア標準コンポーネント**: `core.transform`、`core.velocity`、`core.animation`、`core.presence`
2. **名前空間付きカスタムコンポーネント**: `com.example.door`、`org.school.example.board`

**[SPEC]** カスタム状態はサイズ上限、更新頻度上限、権限を持つ（仕様 §9.8）。

**[REC]** コンポーネントは `(namespace: String, key: String, payload: bytes)` の Value Object として表現する。コアは payload の内容を解釈しない。

**[ADR]** カスタムコンポーネントの最大サイズ、最大数、更新頻度上限の具体値は未決定（仕様 §42.8）。

### 4.3 VisibilityPolicy

**[SPEC]** 仕様 §17.4 に基づく：

```text
Value Object: VisibilityPolicy
├── Global
├── Spatial { radius: f32 }
├── OwnerOnly
├── RoleRestricted { roles: Vec<RoleId> }
├── Explicit { users: Vec<UserId> }
└── Custom { tag: String }
```

**[REC]** `Custom` の解釈は `interest` モジュールの policy evaluator に委ねる。コアは tag を透過的に保持する。

### 4.4 Timestamp / Revision

**[SPEC]** DB 保存は UTC、API は RFC 3339、realtime protocol は Unix milliseconds 等、明示形式（仕様 §31.4）。

**[REC]** ドメイン層では `Timestamp` Value Object を使用し、transport 固有の時刻形式を直接持たない。テストでは Clock trait を注入する（仕様 §31.4）。

**[SPEC]** `Revision(u64)` はワールド定義、インスタンス、エンティティの楽観的並行制御に使用する。古い revision のクライアント更新は拒否または再同期させる（仕様 §14.3）。

## 5. ドメインイベント

**[REC]** 主要なドメインイベントの例（所有モジュールが発行）：

| イベント | 発行元 | 主な受信者 |
|---|---|---|
| `UserCreated` | `identity_access` | audit, extension |
| `UserDisabled` | `identity_access` | audit, extension, session revoke |
| `InstanceStarted` | `world_directory` | audit, extension |
| `InstanceStopped` | `world_directory` | audit, extension |
| `MemberJoined` | `realtime_presence` | audit, instance_runtime, extension |
| `MemberLeft` | `realtime_presence` | audit, instance_runtime, extension |
| `EntitySpawned` | `instance_runtime` | interest, delivery, audit |
| `EntityUpdated` | `instance_runtime` | interest, delivery |
| `EntityDeleted` | `instance_runtime` | interest, delivery, audit |
| `OwnershipTransferred` | `instance_runtime` | audit |

**[REC]** イベントは既に成立した事実として表現し、所有モジュールのみが発行する（`architecture.md` §3.2）。

**[REC]** 表中の「主な受信者」は論理的な消費先であり、直接呼出しを意味しない。`instance_runtime` は `interest` や `realtime_delivery` を直接呼ばず、coordinator が state view / event を受け取って配信を調停する（`architecture.md` §3.1 DAG）。

**[ADR]** イベントの内部配送方式（同一プロセス typed event / outbox）は ARC-04 で決める。

## 6. 集約間の参照規則

**[REC]** 集約間の参照は ID のみで行い、オブジェクト参照を保持しない：

- `WorldInstance` は `WorldId` を持つが `WorldDefinition` オブジェクトを保持しない
- `Entity` は `InstanceId` と `Option<UserId>` を持つが、各集約オブジェクトを保持しない
- `PresenceId` は `AuthSessionId` を保持しない。認証済み主体 ID（`UserId`）のみを binding する

**[REC]** モジュール間で相手の repository 実装や DB テーブルを直接利用しない（`architecture.md` §2.1）。

## 7. 要 ADR 事項

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| DM-01 | ID 内部表現 | **Accepted: UUIDv7** | ADR-001。PostgreSQL native UUID、Rust ecosystem、生成順序を考慮 |
| DM-02 | metadata のサイズ/深度制限 | 64 KiB、JSON 深度 10 | DoS 防止。具体値は負荷試験で調整 |
| DM-03 | Quaternion 正規化許容誤差 | 1e-6 | 数値精度と実用上の許容範囲 |
| DM-04 | カスタムコンポーネント上限 | 1 entity あたり 16 個、各 4 KiB | 仕様 §42.8。具体値は負荷試験で調整 |
| DM-05 | ログイン試行制限 | 5 回失敗で 15 分ロック | OWASP 推奨。セキュリティ ADR と統合 |
| DM-06 | インスタンス自動生成規則 | 管理者が明示的に作成 | 仕様 §42.7。初期は自動生成なし |
| DM-07 | resume grace 期間 | 60 秒 | 仕様 §28.2 例。具体値は ADR で確定 |
