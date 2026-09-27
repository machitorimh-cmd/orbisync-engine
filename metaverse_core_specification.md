# Metaverse Core OSS Backend 仕様書

> **文書状態:** Draft 0.1  
> **想定ライセンス:** Apache License 2.0 または MIT OR Apache-2.0  
> **実装言語:** Rust  
> **対象:** 用途非依存・描画非依存・アセット非依存のリアルタイム共有ワールドバックエンド  
> **名称:** `OrbiSync`

---

## 1. 文書の目的

本書は、学校、企業、イベント、研究、コミュニティ、ゲーム的アプリケーションなど、特定用途に依存しないOSSのメタバース・バックエンドコアについて、設計方針、機能要件、通信仕様、データモデル、セキュリティ、性能、リポジトリ構成、テスト、運用、OSSメンテナンス方針を定義する。

本プロジェクトが提供するのは、完成済みのメタバースアプリケーションではない。複数の認証済みクライアントが同一ワールドへ接続し、ユーザー、エンティティ、位置、向き、状態、イベントをリアルタイムに共有するための**ヘッドレスなサーバー基盤**である。

フロントエンド、3D描画、アバター、アセット、UI、音声、映像、業務固有機能は、利用組織または外部開発者が自由に実装する。

---

## 2. 仕様用語

本書では、要件の強さを次の語で表す。

- **MUST:** 実装上必須であり、満たさない場合は仕様非準拠とする。
- **MUST NOT:** 禁止事項。
- **SHOULD:** 原則として実装する。実装しない場合は合理的理由を文書化する。
- **SHOULD NOT:** 原則として避ける。
- **MAY:** 任意実装。

---

## 3. プロジェクトの基本方針

### 3.1 プロジェクトの定義

`metaverse-core`は、次の機能を提供するOSSバックエンドである。

1. 組織が発行したIDとパスワードによる認証
2. ユーザー、ロール、権限の管理
3. ワールド定義とワールドインスタンスの管理
4. 認証済みユーザーの接続、入室、退室、再接続
5. ユーザーおよび任意エンティティの状態同期
6. サーバー権威型の状態検証
7. 周辺ユーザーだけへ配信するInterest Management
8. 一時状態と永続状態の分離
9. 用途固有機能を外部実装できる拡張API
10. 言語非依存の公開通信プロトコル
11. 運用者向け管理API
12. 監視、監査、障害解析に必要な観測機能

### 3.2 最重要設計原則

> **公式フロントエンドで実行できる操作は、すべて公開APIまたは公開プロトコルからも実行できなければならない。**

コアは特定のUI、ゲームエンジン、レンダラー、アセット形式に依存してはならない。

### 3.3 想定利用形態

初期バージョンでは、以下を標準運用形態とする。

- 1インスタンス = 1組織
- 組織管理者がユーザーを作成
- 匿名ユーザーなし
- 自己登録なし
- Google、Apple、Microsoft等の外部ログインなし
- インターネット越しにHTTPSおよびWebSocket Secureで接続
- 4G/5Gを含む不安定なモバイル回線を想定
- Docker Composeでセルフホスト可能

将来、LDAP、Active Directory、OIDC、SAML、複数組織のマルチテナント対応を追加できる境界は残すが、初期コアの必須機能には含めない。

---

## 4. スコープ

### 4.1 コアが担当する範囲

- ローカルID・パスワード認証
- セッションおよびトークン管理
- ユーザー管理
- ロールおよび権限管理
- ワールド定義管理
- ワールドインスタンス管理
- 接続状態管理
- プレゼンス管理
- ユーザー位置、向き、速度、状態の同期
- 汎用エンティティ状態の同期
- エンティティ所有権および操作権限
- サーバー権威型の入力検証
- スナップショット配信
- 差分更新配信
- Interest Management
- 再接続および状態復元
- イベント配信
- 永続化
- 管理API
- 監査ログ
- メトリクス、ログ、トレース
- プロトコル定義とSDK生成材料
- 外部拡張用API
- 負荷試験ツール

### 4.2 コアが担当しない範囲

以下は明示的にコアの責務外とする。

- 3Dまたは2D描画
- Unity、Godot、Three.js、Babylon.js等のレンダラー実装
- 3Dモデル、テクスチャ、アニメーション、アバター
- アセット配信、CDN、アセット変換
- 音声通話、映像通話、SFU、TURN
- 空間音響
- UIデザイン
- スマートフォン固有UI
- 学校向け出席管理
- 企業向け勤怠管理
- EC、決済、NFT
- フレンド、フォロー、SNS
- ワールドエディタ
- 物理シミュレーションエンジン
- 高度なゲームロジック
- AIキャラクター
- コンテンツモデレーションの業務運用

### 4.3 最小参照実装

OSS利用者が動作確認できるよう、以下の軽量な参照実装を提供してよい。

- ログイン画面
- ワールド一覧
- 入退室
- 接続状態表示
- 参加者一覧
- 2Dまたは簡易3Dによる位置同期デモ
- 任意エンティティの状態変更デモ
- 管理者用の最小ユーザー管理画面

参照実装は製品UIではなく、SDK利用例、疎通確認、E2Eテスト、営業デモを兼ねる。

---

## 5. 目標と非目標

### 5.1 目標

- 軽量な単一バイナリとして配布できる
- 小規模環境では1台のLinuxサーバーで運用できる
- 将来の水平分割を妨げない
- 4G/5G回線で切断や遅延が発生しても復帰できる
- クライアント言語を限定しない
- 公式実装をフォークせず拡張できる
- メンテナーがモジュール単位で変更範囲を把握できる
- プロトコル互換性を自動テストできる
- セキュリティ更新を継続的に提供できる
- 新規コントリビューターがローカルで容易に起動できる

### 5.2 非目標

- 初期版で1000人同一空間を無条件保証すること
- MMORPG相当の物理、戦闘、チート対策を提供すること
- 全機能を単一プロセスへ永久に固定すること
- すべての認証方式を標準実装すること
- 任意の第三者コードをコアプロセス内で安全に実行すること
- アセット仕様や画面デザインを標準化すること

---

## 6. システムコンテキスト

```text
┌──────────────────────────────────────────────────────────────┐
│ 任意クライアント                                               │
│ Web / iOS / Android / Flutter / React Native / Unity / Godot │
└───────────────┬──────────────────────────────────────────────┘
                │ HTTPS + WebSocket Secure
                │ 公開API・公開プロトコル
┌───────────────▼──────────────────────────────────────────────┐
│ metaverse-core                                                │
│                                                              │
│ Auth / Users / Roles / Worlds / Instances / Sessions         │
│ Realtime State / Interest / Events / Persistence / Audit     │
└───────────────┬──────────────────────────────────────────────┘
                │
        ┌───────┴────────┐
        │                │
┌───────▼──────┐  ┌──────▼────────────────┐
│ PostgreSQL   │  │ 外部拡張サービス       │
│ 永続データ    │  │ Webhook / HTTP / gRPC │
└──────────────┘  └───────────────────────┘

範囲外:
- アセットサーバー
- 音声SFU
- 描画エンジン
- 業務固有フロントエンド
```

---

## 7. アーキテクチャ方針

### 7.1 初期構成: モジュラーモノリス

初期版は、複数の論理モジュールを1つのRustプロセスへまとめたモジュラーモノリスとする。

```text
metaverse-core-server
├─ HTTP API
├─ WebSocket Gateway
├─ Authentication
├─ User and Permission Management
├─ World Registry
├─ Instance Runtime
├─ Interest Management
├─ Persistence Adapter
├─ Audit and Observability
└─ Extension Gateway
```

マイクロサービス化を先行させない。ネットワーク境界を増やす前に、ドメイン境界と負荷特性を実測する。

### 7.2 将来の分離可能性

以下のモジュールは、将来別プロセスへ分離可能なAPI境界を持つ。

- API Gateway
- Authentication Service
- World Directory
- Instance Runtime
- Persistence Worker
- Audit/Event Exporter

ただし、初期版で内部通信をgRPC化する必要はない。Rust traitおよび内部コマンド型で境界を定義し、分離が必要になった時点で外部プロトコルへ置き換える。

### 7.3 依存方向

依存方向は内側へ向ける。

```text
Transport / Database / Framework
              ↓
Application Services
              ↓
Domain Model
```

- ドメイン層はAxum、SQLx、PostgreSQL、WebSocketへ依存してはならない。
- アプリケーション層はHTTP型やDB行型を直接受け取ってはならない。
- インフラ層はドメインが定義したport/traitを実装する。
- プロトコル型とドメイン型は明示的に変換する。

---

## 8. 推奨技術スタック

### 8.1 サーバー

- 言語: Rust
- Rust Edition: 2024
- 非同期ランタイム: Tokio
- HTTP/WebSocket: Axum
- シリアライズ:
  - リアルタイム本番プロトコル: Protocol Buffers
  - 管理REST API: JSON
  - デバッグ用リアルタイムモード: JSONをMAYで提供
- PostgreSQLアクセス: SQLx等の非同期ドライバ
- パスワードハッシュ: Argon2id
- ログ/トレース: `tracing`
- メトリクス: Prometheus互換エクスポート
- API仕様: OpenAPI
- コンテナ: OCI互換イメージ

特定crateのバージョンは本書では固定せず、`Cargo.lock`、DependabotまたはRenovate、定期リリースで管理する。

### 8.2 クライアントSDK

初期の公式SDKはTypeScriptを推奨する。ただし、コア仕様の正本はSDKコードではなく、以下とする。

1. `.proto`ファイル
2. REST OpenAPI定義
3. プロトコル状態遷移仕様
4. 互換性テストベクトル

将来的に以下のSDKを追加できる。

- TypeScript
- Dart
- C#
- Swift
- Kotlin
- Rust
- Go

---

## 9. ドメインモデル

### 9.1 User

認証可能な利用者。匿名ユーザーは存在しない。

```rust
pub struct User {
    pub id: UserId,
    pub login_id: LoginId,
    pub display_name: String,
    pub status: UserStatus,
    pub must_change_password: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

制約:

- `id`は内部用の不変IDとする。
- `login_id`は利用者が入力するIDであり、管理者が変更可能としてよい。
- `display_name`と`login_id`を分離する。
- 削除より無効化を優先する。
- 認証情報はUser本体と論理的に分離する。

### 9.2 Role / Permission

学校、企業等の用途固有ロールをコアへ埋め込まない。

コアは権限文字列を扱う。

```text
users.read
users.create
users.update
users.disable
roles.manage
worlds.read
worlds.create
worlds.update
instances.create
instances.join
instances.moderate
entities.spawn
entities.update.own
entities.update.any
entities.delete.own
entities.delete.any
events.publish
admin.audit.read
```

`Student`、`Teacher`、`Employee`等は、利用組織が権限を組み合わせて作るロールである。

### 9.3 World Definition

ワールドの論理定義。描画アセットを含まない。

```rust
pub struct WorldDefinition {
    pub id: WorldId,
    pub name: String,
    pub description: Option<String>,
    pub status: WorldStatus,
    pub default_spawn: Transform,
    pub capacity: u32,
    pub metadata: JsonValue,
    pub revision: u64,
}
```

`metadata`はクライアント固有情報を格納できるが、コアは内容を解釈しない。最大サイズを設定する。

### 9.4 World Instance

実際にユーザーが参加する実行単位。

```rust
pub struct WorldInstance {
    pub id: InstanceId,
    pub world_id: WorldId,
    pub lifecycle: InstanceLifecycle,
    pub capacity: u32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub revision: u64,
}
```

同一World Definitionから複数Instanceを生成できる。

例:

```text
World Definition: classroom
├─ Instance: classroom-2026-07-29-a
├─ Instance: classroom-2026-07-29-b
└─ Instance: classroom-testing
```

### 9.5 Session

認証セッションとワールド接続セッションを分離する。

- **Auth Session:** API利用者としての認証状態
- **Realtime Session:** 特定WebSocket接続の状態
- **Instance Membership:** 特定ワールドインスタンスへの参加状態

WebSocketが切断されても、短い猶予時間内はMembershipを保持できる。

### 9.6 Entity

ワールド内の同期対象。ユーザーアバターもエンティティの一種として扱えるが、ユーザー識別情報とエンティティ状態は分ける。

```rust
pub struct Entity {
    pub id: EntityId,
    pub instance_id: InstanceId,
    pub kind: EntityKind,
    pub owner: Option<UserId>,
    pub transform: Option<Transform>,
    pub state: EntityState,
    pub visibility: VisibilityPolicy,
    pub revision: u64,
}
```

### 9.7 Transform

レンダラー非依存の座標表現を使用する。

```rust
pub struct Transform {
    pub position: Vec3,
    pub rotation: Quaternion,
    pub scale: Vec3,
}
```

規約:

- 単位系のデフォルトは1 unit = 1 meterとする。
- 座標系は仕様で固定する。推奨は右手系、Y-up。
- Quaternionの要素順を明示する。推奨は`x, y, z, w`。
- NaN、Infinity、範囲外値を拒否する。
- scaleは任意機能とし、ユーザー移動では通常送信しない。

### 9.8 Entity State

用途固有状態を拡張可能にする。

初期実装では以下の2方式を併用できる。

1. コア標準コンポーネント
2. 名前空間付きカスタムコンポーネント

```text
core.transform
core.velocity
core.animation
core.presence
com.example.door
org.school.example.board
```

カスタム状態はサイズ上限、更新頻度上限、権限を持つ。

---

## 10. 状態の分類

### 10.1 一時状態

主にメモリ上で管理し、通常は毎更新DBへ保存しない。

- 現在位置
- 現在の向き
- 速度
- アニメーション状態
- 接続状態
- 短期的なプレゼンス
- 一時的エンティティ
- Interest Managementの購読状態

### 10.2 永続状態

PostgreSQLへ保存する。

- ユーザー
- ロール、権限
- ワールド定義
- 永続エンティティ定義
- 永続コンポーネント
- インスタンス設定
- 管理操作
- 監査ログ
- プラグイン登録情報

### 10.3 チェックポイント

必要に応じ、ワールドインスタンスの状態をチェックポイントとして保存できる。

- 手動保存
- 一定時間ごとの保存
- インスタンス終了時保存
- 永続化対象コンポーネントのみ保存

位置情報のような高頻度状態を毎回DBへ書き込んではならない。

---

## 11. サーバー権威モデル

### 11.1 原則

サーバーがワールドの正しい状態を決定する。

クライアントは状態を確定するのではなく、入力または状態変更要求を送る。

```text
Client: 「この位置へ移動したい」
Server: セッション、権限、速度、範囲を検証
Server: 正式状態を更新
Server: 関係クライアントへ更新配信
```

### 11.2 入力検証

最低限、以下を検証する。

- 認証済みセッションか
- 対象インスタンスへ参加中か
- 対象エンティティを操作できるか
- 数値が有限か
- 最大速度を超えていないか
- 最大更新頻度を超えていないか
- 最大移動距離を超えていないか
- ワールド境界を超えていないか
- メッセージサイズ上限内か
- プロトコルバージョンが互換か

### 11.3 物理・衝突判定

本コアは汎用物理エンジンを内蔵しない。

初期版では以下を提供する。

- AABB等による任意の簡易境界検証を追加可能なhook
- ワールド全体の座標範囲制限
- 最大速度、最大加速度、テレポート権限
- 用途固有検証サービスへの委譲

厳密なゲーム物理同期は別拡張とする。

---

## 12. インスタンス実行モデル

### 12.1 Actor形式

各World Instanceは、単一の論理所有者が状態更新を直列化するActor形式を推奨する。

```rust
pub enum InstanceCommand {
    Join(JoinRequest),
    Leave(LeaveRequest),
    Resume(ResumeRequest),
    UpdateTransform(TransformUpdate),
    UpdateEntity(EntityUpdate),
    PublishEvent(PublishEvent),
    Admin(AdminCommand),
    Tick,
    Shutdown,
}
```

利点:

- ワールド状態全体を多数のMutexで保護せずに済む
- 状態更新順序を理解しやすい
- テストで決定論的に再現しやすい
- 将来、インスタンス単位で別プロセスへ移動しやすい

### 12.2 メールボックス

- bounded channelを使用する。
- キュー上限を設定する。
- 位置更新は同一ユーザーの古い要求を集約できる。
- 管理イベントや永続イベントを位置更新と同じ優先度にしない。
- キュー飽和時の挙動をメトリクスへ記録する。

### 12.3 Tick

インスタンスは固定または適応tickを使用できる。

推奨初期値:

- サーバー状態tick: 10〜20 Hz
- 停止中ユーザーの更新: 1 Hz以下
- クライアント描画: コアの責務外

Tick値は管理画面へ露出させず、設定ファイルまたは上級者向け設定とする。

---

## 13. 通信方式

### 13.1 管理・制御API

HTTPS REST APIを使用する。

対象:

- ログイン
- トークン更新
- ログアウト
- ユーザー管理
- ロール管理
- ワールド管理
- インスタンス作成
- 監査ログ参照
- ヘルスチェック

### 13.2 リアルタイム通信

WebSocket Secureを標準とする。

理由:

- Web、モバイル、ゲームエンジンで広く利用可能
- プロキシや企業ネットワークを通過しやすい
- 双方向通信が可能
- 初期実装と運用が比較的単純

将来、WebTransport/QUICを実験的transportとして追加してよいが、WebSocket互換性を失ってはならない。

### 13.3 プロトコル形式

本番モードはProtocol Buffersを標準とする。

- スキーマを言語非依存に保てる
- 多言語SDKを生成しやすい
- JSONよりサイズを抑えやすい
- field numberを維持することで互換性を管理できる

デバッグ用にJSONサブプロトコルを提供してよい。ただし、JSONとProtobufで意味論を変えてはならない。

### 13.4 WebSocketサブプロトコル

例:

```text
Sec-WebSocket-Protocol: metaverse.v1.protobuf
Sec-WebSocket-Protocol: metaverse.v1.json
```

サーバーは未対応サブプロトコルを明示的に拒否する。

---

## 14. リアルタイムプロトコル

### 14.1 Envelope

すべてのメッセージを共通Envelopeへ格納する。

```proto
message Envelope {
  uint32 protocol_major = 1;
  uint32 protocol_minor = 2;
  string message_id = 3;
  uint64 sequence = 4;
  int64 sent_at_unix_ms = 5;
  string instance_id = 6;

  oneof payload {
    ClientHello client_hello = 20;
    ServerHello server_hello = 21;
    JoinInstance join_instance = 22;
    JoinAccepted join_accepted = 23;
    Snapshot snapshot = 24;
    StateDelta state_delta = 25;
    TransformInput transform_input = 26;
    EntityCommand entity_command = 27;
    DomainEvent domain_event = 28;
    Heartbeat heartbeat = 29;
    HeartbeatAck heartbeat_ack = 30;
    ResumeSession resume_session = 31;
    ResumeAccepted resume_accepted = 32;
    ResyncRequired resync_required = 33;
    ErrorMessage error = 34;
  }
}
```

### 14.2 メッセージ分類

#### A. Latest-wins状態

古い更新を破棄できる。

- transform
- velocity
- look direction
- animation state
- presence ping

#### B. Reliableイベント

順序と重複排除が重要。

- join
- leave
- entity spawn
- entity delete
- ownership transfer
- role change
- custom domain event
- moderation action

WebSocket自体は順序保証されるが、アプリケーション層で再接続、再送、重複排除を扱う。

### 14.3 Sequence

- 接続ごとに単調増加するsequenceを付与する。
- インスタンス状態にはrevisionを付与する。
- エンティティ単位にもrevisionを持たせてよい。
- 古いrevisionのクライアント更新は拒否または再同期させる。

### 14.4 メッセージ上限

初期推奨値:

- 通常リアルタイムメッセージ: 16 KiB以下
- カスタムイベント: 64 KiB以下
- スナップショット: 分割送信可能
- 圧縮後・展開後の両方に上限を設定
- 1接続あたり毎秒メッセージ数を制限

巨大データやアセットをWebSocketへ流してはならない。

---

## 15. 接続フロー

### 15.1 初回接続

```text
1. Client → POST /v1/auth/login
2. Server → access token + refresh mechanism
3. Client → WebSocket接続
4. Client → ClientHello
5. Server → ServerHello
6. Client → JoinInstance
7. Server → 権限・定員・状態を検証
8. Server → JoinAccepted
9. Server → Snapshot
10. Server/Client → DeltaおよびCommandの交換
```

### 15.2 ClientHello

含める情報:

- SDK名
- SDKバージョン
- プロトコルmajor/minor
- クライアント種別
- 対応圧縮方式
- 対応機能flags
- 任意のresume token

サーバーはクライアント種別や自己申告値を権限判定に使ってはならない。

### 15.3 Snapshot

入室時に、クライアントが必要とする初期状態を送る。

- 自分のユーザー情報
- インスタンス情報
- 自分の権限
- 周辺エンティティ
- 周辺ユーザー
- 現在revision
- サーバー時刻

1000人全員分を無条件に送らず、Interest Management適用後の状態を送る。

---

## 16. モバイル回線対応

本要件は音声やアセットではなく、コアのWebSocket/REST通信に適用する。

### 16.1 前提とする障害

- 4Gと5Gの切り替え
- 基地局ハンドオーバー
- 短時間の圏外
- IPアドレス変更
- 高遅延
- ジッター
- 一時的なパケットロス
- アプリのバックグラウンド化
- OSによるソケット停止
- TCP接続の半開き

### 16.2 自動再接続

公式SDKは以下を実装する。

1. 切断検知
2. 指数バックオフ + jitter
3. アクセストークン更新
4. 新しいWebSocket確立
5. ResumeSession送信
6. 復帰可能なら差分再開
7. 復帰不可能なら新しいSnapshot取得

### 16.3 Resume Token

Resume Tokenは以下の性質を持つ。

- 短時間のみ有効
- ユーザー、インスタンス、セッションepochへ紐づく
- 推測困難
- サーバー側で無効化可能
- 切断前の最終受信revisionを含められる

### 16.4 再同期

サーバーが差分履歴を保持している場合:

```text
Client last_revision = 1200
Server current_revision = 1230
Server retains 1201..1230
→ 差分だけ送信
```

履歴がない場合:

```text
→ ResyncRequired
→ 新しいSnapshot
```

### 16.5 最新状態優先

位置更新は送信キュー上で同一エンティティの古い更新を上書きする。

```text
x=1.0 → x=1.1 → x=1.2 → x=1.3
送信詰まり発生
→ x=1.3だけを残す
```

Reliableイベントは破棄してはならない。最新状態とイベントは別キューまたは別優先度で処理する。

### 16.6 ハートビート

- Application-level heartbeatを使用する。
- WebSocket ping/pongだけに依存しない。
- RTTを測定する。
- 連続失敗で切断扱いにする。
- モバイル回線向けに過度に短いtimeoutを設定しない。

推奨初期値:

- heartbeat間隔: 15〜30秒
- timeout: 45〜90秒
- 値は環境変数で変更可能

---

## 17. Interest Management

### 17.1 目的

同一ワールドに多数のユーザーが存在しても、各クライアントへ必要な状態だけを配信する。

### 17.2 初期アルゴリズム

Uniform Spatial Gridを標準とする。

```text
World
├─ Cell (0,0)
├─ Cell (0,1)
├─ Cell (1,0)
└─ Cell (1,1)
```

各エンティティを現在位置のセルへ登録し、クライアントは自身のセルと周辺セルを購読する。

### 17.3 配信レベル

例:

- Near: 高頻度、完全状態
- Mid: 低頻度、簡略状態
- Far: 非配信または集計情報
- Global: 管理イベント等、距離に関係なく配信

コアは描画LODを管理しない。配信する情報量と頻度のみを管理する。

### 17.4 可視性ポリシー

```rust
pub enum VisibilityPolicy {
    Global,
    Spatial { radius: f32 },
    OwnerOnly,
    RoleRestricted(Vec<RoleId>),
    Explicit(Vec<UserId>),
    Custom(String),
}
```

### 17.5 ヒステリシス

境界付近で購読と解除が頻発しないよう、参加半径と離脱半径を分けてよい。

- subscribe radius: 30m
- unsubscribe radius: 35m

---

## 18. バックプレッシャー

### 18.1 原則

遅いクライアント1台がインスタンス全体を遅延させてはならない。

### 18.2 接続ごとの送信キュー

- bounded queue
- メッセージ種別ごとの優先度
- latest-wins状態の上書き
- reliableイベントの上限
- 上限超過時は警告後に切断可能

### 18.3 Slow Consumer

以下をメトリクス化する。

- キュー長
- ドロップした状態更新数
- reliable queue overflow数
- 書き込み遅延
- slow consumer切断数

### 18.4 インスタンス保護

- 接続単位rate limit
- ユーザー単位rate limit
- IP単位ログインrate limit
- インスタンス単位総入力上限
- 高コストイベントの同時実行制限

---

## 19. 認証仕様

### 19.1 アカウント発行

- 自己登録は提供しない。
- 管理者がアカウントを作成する。
- CSV一括作成をMAYで提供する。
- 初期パスワードを発行できる。
- 初回ログイン時のパスワード変更を強制できる。
- メールアドレスは必須ではない。

### 19.2 パスワード

- 平文保存禁止
- Argon2idでハッシュ
- 組織ポリシーに応じた最小長
- 弱い既知パスワード拒否をMAYで提供
- 管理者も現在のパスワードを閲覧できない

### 19.3 パスワードリセット

メールリセットを前提としない。

```text
利用者 → 管理者へ連絡
管理者 → 一時パスワード発行
利用者 → 次回ログインで変更
```

### 19.4 トークン

推奨モデル:

- 短命Access Token
- 長命Refresh Token
- Refresh TokenはDBへハッシュ保存
- Refresh Token rotation
- ログアウト時失効
- ユーザー無効化時に全セッション失効

ブラウザ参照実装はHttpOnly Secure Cookieを使用できる。ネイティブSDKはOSの安全な資格情報ストレージを利用する責任を持つ。

### 19.5 外部認証

初期版では実装しない。ただし以下のtrait境界を定義してよい。

```rust
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn authenticate(&self, request: AuthRequest) -> Result<AuthIdentity, AuthError>;
    async fn disable_identity(&self, user_id: UserId) -> Result<(), AuthError>;
}
```

ローカル認証が唯一の公式実装となる。

---

## 20. 認可仕様

### 20.1 RBAC

初期版はRole-Based Access Controlを採用する。

- Userは複数Roleを持てる
- Roleは複数Permissionを持つ
- Permissionは名前空間付き文字列
- Deny ruleは初期版では持たず、allow-onlyを推奨

### 20.2 ワールド内権限

管理API権限とワールド内権限を分離する。

例:

```text
admin.users.create
admin.audit.read
world.join
world.instance.create
entity.spawn
entity.update.own
entity.update.any
moderation.kick
```

### 20.3 所有権

エンティティはownerを持てる。

- ownerだけ更新可能
- 任意権限保持者は更新可能
- サーバーのみ更新可能
- 所有権移譲イベントを監査可能

クライアントの自己申告ownerを信用しない。

---

## 21. REST API概要

すべて`/v1`配下とする。

### 21.1 Authentication

```text
POST   /v1/auth/login
POST   /v1/auth/refresh
POST   /v1/auth/logout
POST   /v1/auth/change-password
GET    /v1/auth/me
```

### 21.2 Users

```text
GET    /v1/users
POST   /v1/users
GET    /v1/users/{user_id}
PATCH  /v1/users/{user_id}
POST   /v1/users/{user_id}/disable
POST   /v1/users/{user_id}/enable
POST   /v1/users/{user_id}/reset-password
POST   /v1/users/import
```

### 21.3 Roles

```text
GET    /v1/roles
POST   /v1/roles
GET    /v1/roles/{role_id}
PATCH  /v1/roles/{role_id}
DELETE /v1/roles/{role_id}
PUT    /v1/users/{user_id}/roles
```

### 21.4 Worlds

```text
GET    /v1/worlds
POST   /v1/worlds
GET    /v1/worlds/{world_id}
PATCH  /v1/worlds/{world_id}
POST   /v1/worlds/{world_id}/archive
```

### 21.5 Instances

```text
GET    /v1/instances
POST   /v1/instances
GET    /v1/instances/{instance_id}
POST   /v1/instances/{instance_id}/start
POST   /v1/instances/{instance_id}/stop
POST   /v1/instances/{instance_id}/kick/{user_id}
GET    /v1/instances/{instance_id}/members
```

### 21.6 Audit

```text
GET    /v1/audit-events
GET    /v1/audit-events/{event_id}
```

### 21.7 Operations

```text
GET    /health/live
GET    /health/ready
GET    /metrics
GET    /version
```

### 21.8 エラーフォーマット

```json
{
  "error": {
    "code": "USER_LOGIN_ID_CONFLICT",
    "message": "The login ID is already in use.",
    "request_id": "req_01...",
    "details": {}
  }
}
```

- `code`は機械可読で安定させる。
- `message`は人間向けであり互換性を保証しない。
- 内部スタックトレースを返さない。

---

## 22. データベース設計案

### 22.1 テーブル一覧

```text
users
user_credentials
roles
permissions
role_permissions
user_roles
auth_sessions
refresh_tokens
world_definitions
world_instances
persistent_entities
persistent_entity_components
instance_checkpoints
audit_events
extension_registrations
schema_migrations
```

### 22.2 users

```sql
CREATE TABLE users (
    id UUID PRIMARY KEY,
    login_id TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    status TEXT NOT NULL,
    must_change_password BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
```

### 22.3 user_credentials

```sql
CREATE TABLE user_credentials (
    user_id UUID PRIMARY KEY REFERENCES users(id),
    password_hash TEXT NOT NULL,
    password_changed_at TIMESTAMPTZ NOT NULL,
    failed_login_count INTEGER NOT NULL DEFAULT 0,
    locked_until TIMESTAMPTZ NULL
);
```

### 22.4 world_definitions

```sql
CREATE TABLE world_definitions (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NULL,
    status TEXT NOT NULL,
    capacity INTEGER NOT NULL,
    default_spawn JSONB NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
```

### 22.5 audit_events

```sql
CREATE TABLE audit_events (
    id UUID PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL,
    actor_user_id UUID NULL,
    action TEXT NOT NULL,
    target_type TEXT NULL,
    target_id TEXT NULL,
    request_id TEXT NULL,
    source_ip INET NULL,
    result TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'
);
```

### 22.6 DBマイグレーション

- 前方移行を基本とする。
- 破壊的変更は複数リリースに分ける。
- Expand → Migrate → Contract方式を推奨する。
- リリース前に既存DBからの移行テストを行う。
- 自動バックアップなしに破壊的migrationを実行しない。

---

## 23. 拡張機構

### 23.1 方針

第三者のネイティブ動的ライブラリをコアプロセスへ直接ロードしない。

理由:

- ABI互換性
- メモリ安全性
- コア全体のクラッシュ
- 依存衝突
- セキュリティ境界の不明確化
- アップデート困難化

### 23.2 初期版の拡張方式

Out-of-process Extensionを採用する。

```text
metaverse-core
├─ Webhook/Event delivery
├─ Signed HTTP callbacks
├─ Extension Command API
└─ Scoped service tokens

External Extension
├─ 学校向け機能
├─ 企業向け機能
├─ 独自監査
└─ 独自ワークフロー
```

### 23.3 イベント例

```text
user.created
user.disabled
instance.started
instance.stopped
member.joined
member.left
entity.spawned
entity.updated
entity.deleted
moderation.user_kicked
```

### 23.4 Webhook要件

- HMAC署名
- timestamp
- event ID
- 再送
- exponential backoff
- dead-letter記録
- 重複配信を前提
- 受信側のidempotency
- タイムアウト
- 宛先ごとの遮断機構

### 23.5 将来のWASM

サンドボックス化されたWASMプラグインは将来候補とする。ただし、初期版では以下が未確定のため必須にしない。

- ABI
- capability model
- CPU/memory制限
- 非同期I/O
- 永続ストレージ
- バージョン互換性

---

## 24. Client SDK仕様

### 24.1 SDKの責務

- REST認証
- Access Token更新
- WebSocket接続
- ClientHello
- 入退室
- Protobuf encode/decode
- 自動再接続
- Resume
- Snapshot適用
- Delta適用
- sequence管理
- heartbeat
- イベント購読
- latest-wins送信キュー
- エラー型

### 24.2 SDKの非責務

- 3D描画
- アバター
- アセットロード
- 入力操作
- 補間表示の具体実装
- 音声
- UI

### 24.3 API例

```ts
const client = new MetaverseClient({
  baseUrl: "https://example.org",
});

await client.auth.login({
  loginId: "user001",
  password: "temporary-password",
});

const instance = await client.instances.join("instance-id");

instance.on("snapshot", snapshot => {
  // 利用側が任意の描画状態へ変換する
});

instance.on("entityUpdated", update => {
  // Unity、Godot、Three.js等へ反映する
});

instance.sendTransform({
  position: { x: 1, y: 0, z: 3 },
  rotation: { x: 0, y: 0, z: 0, w: 1 },
});
```

### 24.4 SDK互換性

- SDK versionとprotocol versionを分ける。
- 旧SDKを即時切断しない。
- サポート対象protocol majorを公開する。
- protocol major変更時は移行ガイドを提供する。

---

## 25. 1000人規模への設計

### 25.1 用語を分ける

以下は別の性能目標である。

- 1000同時TCP/WebSocket接続
- 1サーバー全体で1000人
- 1インスタンスに1000人
- 各クライアントが999人分を受信

初期版で目標とするのは、段階的な負荷試験による拡張可能性であり、全員全員配信を保証するものではない。

### 25.2 段階目標

#### Phase 1

- 1インスタンス: 50人
- 1プロセス合計: 200接続
- 10 Hz transform入力
- 24時間soak test

#### Phase 2

- 1インスタンス: 200人
- 1プロセス合計: 1000接続
- Interest Management有効
- 近距離平均50entity以下

#### Phase 3

- 1論理ワールド: 1000人
- 複数instanceまたは空間shard
- World Directoryによるrouting
- 複数Runtime Node

### 25.3 水平分割

将来構成:

```text
Load Balancer
      ↓
Gateway Nodes
      ↓
World Directory
├─ Runtime Node A: Instance 1, 2
├─ Runtime Node B: Instance 3
└─ Runtime Node C: Instance 4, 5
```

1つのInstanceを複数Nodeへ空間分割する機能は、Phase 3以降の独立設計課題とする。

### 25.4 性能上の原則

- 全員へ全状態を送らない
- 差分配信
- binary protocol
- latest-wins
- bounded queue
- spatial index
- バッチ送信
- 不要なcloneを避ける
- DBをリアルタイムhot pathへ置かない
- メトリクスで実測する

---

## 26. 非機能要件

### 26.1 可用性

初期目標:

- 単一ノード構成で graceful shutdown
- 再起動後に永続データ復元
- active connectionを一定時間drain
- DB停止時にready=false
- プロセス生存のみをliveで判定

### 26.2 レイテンシ

同一地域内のサーバーを前提としたアプリケーション処理目標:

- P50: 10 ms以下
- P95: 50 ms以下
- P99: 100 ms以下

ネットワーク往復時間は含めない。負荷試験環境と測定条件を必ず併記する。

### 26.3 セキュリティ

- TLS必須
- 平文WebSocketを本番で禁止
- secretsをリポジトリへ保存しない
- パスワードハッシュ
- rate limiting
- request size limit
- WebSocket message size limit
- SQL injection対策
- 監査ログ
- 依存脆弱性スキャン
- 最小権限コンテナ
- non-root実行
- CSRF対策（Cookie認証時）
- CORSをデフォルト拒否
- origin検証

### 26.4 プライバシー

- 位置履歴をデフォルトで永続保存しない
- ログへパスワード、token、完全なpayloadを出さない
- IP保存期間を設定可能にする
- 監査ログ保持期間を設定可能にする
- ユーザーデータexport/delete方針を文書化する

### 26.5 保守性

- 循環依存禁止
- 1crateの責務を明確にする
- public APIを最小化
- domain errorを安定化
- ADRを残す
- migrationとprotocol変更をレビュー必須にする
- 自動生成コードと手書きコードを分離する

---

## 27. 観測可能性

### 27.1 Structured Logging

JSON形式を標準とし、以下を含める。

- timestamp
- level
- service
- version
- request_id
- connection_id
- session_id（必要時に匿名化）
- instance_id
- user_id（必要時に匿名化）
- event
- duration_ms
- error_code

### 27.2 Metrics

最低限:

```text
http_requests_total
http_request_duration_seconds
websocket_connections_current
websocket_connections_total
websocket_disconnects_total
instance_members_current
instance_commands_total
instance_command_queue_depth
outbound_queue_depth
state_updates_dropped_total
resume_attempts_total
resume_success_total
snapshot_bytes_total
delta_bytes_total
auth_login_failures_total
db_query_duration_seconds
```

### 27.3 Tracing

- REST request
- WebSocket handshake
- join flow
- snapshot generation
- extension delivery
- DB query

すべてのtransform updateを個別span化すると負荷が高いため、samplingまたは集約を使用する。

---

## 28. 設定仕様

### 28.1 原則

- 環境変数を標準とする。
- YAML/TOML設定ファイルをMAYで提供する。
- secretsと通常設定を分ける。
- 起動時に全設定を検証する。
- 不明な設定キーは警告またはエラーにする。

### 28.2 設定例

```toml
[server]
bind = "0.0.0.0:8080"
public_url = "https://meta.example.org"

[database]
url_env = "DATABASE_URL"
max_connections = 20

[auth]
access_token_ttl_seconds = 900
refresh_token_ttl_seconds = 2592000
password_min_length = 12

[realtime]
heartbeat_interval_seconds = 20
connection_timeout_seconds = 60
max_message_bytes = 65536
outbound_queue_capacity = 256
instance_command_queue_capacity = 4096

[world]
default_capacity = 100
server_tick_hz = 10
resume_grace_seconds = 60

[interest]
strategy = "uniform_grid"
cell_size = 20.0
near_radius = 30.0
unsubscribe_radius = 35.0

[observability]
log_format = "json"
metrics_enabled = true
```

### 28.3 設定の階層

優先順位:

1. CLI引数
2. 環境変数
3. 設定ファイル
4. 安全なデフォルト値

---

## 29. リポジトリ構成

Cargo Workspaceを使用したmonorepoを推奨する。プロトコル、サーバー、SDK、参照実装、負荷試験、ドキュメントを同一リポジトリで互換性テストできる利点がある。

```text
metaverse-core/
├─ Cargo.toml
├─ Cargo.lock
├─ rust-toolchain.toml
├─ rustfmt.toml
├─ clippy.toml
├─ deny.toml
├─ README.md
├─ LICENSE-APACHE
├─ LICENSE-MIT
├─ CHANGELOG.md
├─ CONTRIBUTING.md
├─ CODE_OF_CONDUCT.md
├─ SECURITY.md
├─ GOVERNANCE.md
├─ MAINTAINERS.md
├─ CODEOWNERS
├─ .editorconfig
├─ .gitignore
├─ .dockerignore
│
├─ .github/
│  ├─ workflows/
│  │  ├─ ci.yml
│  │  ├─ security.yml
│  │  ├─ protocol-compat.yml
│  │  ├─ load-smoke.yml
│  │  ├─ release.yml
│  │  └─ docs.yml
│  ├─ ISSUE_TEMPLATE/
│  │  ├─ bug.yml
│  │  ├─ feature.yml
│  │  ├─ performance.yml
│  │  └─ security-config.yml
│  ├─ PULL_REQUEST_TEMPLATE.md
│  ├─ dependabot.yml
│  └─ CODEOWNERS
│
├─ crates/
│  ├─ domain/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ user.rs
│  │     ├─ auth.rs
│  │     ├─ role.rs
│  │     ├─ permission.rs
│  │     ├─ world.rs
│  │     ├─ instance.rs
│  │     ├─ entity.rs
│  │     ├─ transform.rs
│  │     ├─ session.rs
│  │     ├─ event.rs
│  │     └─ error.rs
│  │
│  ├─ application/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ ports/
│  │     │  ├─ mod.rs
│  │     │  ├─ user_repository.rs
│  │     │  ├─ world_repository.rs
│  │     │  ├─ token_service.rs
│  │     │  ├─ audit_sink.rs
│  │     │  └─ extension_sink.rs
│  │     ├─ auth/
│  │     ├─ users/
│  │     ├─ roles/
│  │     ├─ worlds/
│  │     ├─ instances/
│  │     └─ audit/
│  │
│  ├─ protocol/
│  │  ├─ Cargo.toml
│  │  ├─ build.rs
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ generated/
│  │     ├─ codec.rs
│  │     ├─ version.rs
│  │     ├─ validation.rs
│  │     └─ conversion.rs
│  │
│  ├─ realtime/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ gateway.rs
│  │     ├─ connection.rs
│  │     ├─ connection_registry.rs
│  │     ├─ outbound_queue.rs
│  │     ├─ heartbeat.rs
│  │     ├─ resume.rs
│  │     └─ rate_limit.rs
│  │
│  ├─ world-runtime/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ actor.rs
│  │     ├─ command.rs
│  │     ├─ state.rs
│  │     ├─ snapshot.rs
│  │     ├─ delta.rs
│  │     ├─ entity_store.rs
│  │     ├─ ownership.rs
│  │     ├─ validation.rs
│  │     └─ lifecycle.rs
│  │
│  ├─ interest/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ policy.rs
│  │     ├─ spatial_index.rs
│  │     ├─ uniform_grid.rs
│  │     ├─ subscription.rs
│  │     └─ visibility.rs
│  │
│  ├─ auth/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ password.rs
│  │     ├─ access_token.rs
│  │     ├─ refresh_token.rs
│  │     ├─ session.rs
│  │     ├─ lockout.rs
│  │     └─ local_provider.rs
│  │
│  ├─ storage-postgres/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ pool.rs
│  │     ├─ users.rs
│  │     ├─ roles.rs
│  │     ├─ worlds.rs
│  │     ├─ entities.rs
│  │     ├─ sessions.rs
│  │     ├─ audit.rs
│  │     └─ transaction.rs
│  │
│  ├─ transport-http/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ router.rs
│  │     ├─ middleware/
│  │     ├─ extractors/
│  │     ├─ response.rs
│  │     └─ routes/
│  │        ├─ auth.rs
│  │        ├─ users.rs
│  │        ├─ roles.rs
│  │        ├─ worlds.rs
│  │        ├─ instances.rs
│  │        ├─ audit.rs
│  │        └─ operations.rs
│  │
│  ├─ extensions/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ registry.rs
│  │     ├─ webhook.rs
│  │     ├─ signer.rs
│  │     ├─ retry.rs
│  │     └─ delivery_log.rs
│  │
│  ├─ observability/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ logging.rs
│  │     ├─ metrics.rs
│  │     ├─ tracing.rs
│  │     └─ health.rs
│  │
│  ├─ config/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ model.rs
│  │     ├─ loader.rs
│  │     └─ validation.rs
│  │
│  ├─ testkit/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ fixtures.rs
│  │     ├─ fake_clock.rs
│  │     ├─ fake_repositories.rs
│  │     ├─ protocol_client.rs
│  │     └─ instance_harness.rs
│  │
│  └─ server/
│     ├─ Cargo.toml
│     └─ src/
│        ├─ main.rs
│        ├─ bootstrap.rs
│        ├─ shutdown.rs
│        └─ version.rs
│
├─ proto/
│  ├─ buf.yaml
│  ├─ buf.gen.yaml
│  └─ metaverse/
│     └─ v1/
│        ├─ common.proto
│        ├─ envelope.proto
│        ├─ handshake.proto
│        ├─ instance.proto
│        ├─ entity.proto
│        ├─ transform.proto
│        ├─ event.proto
│        ├─ error.proto
│        └─ admin.proto
│
├─ openapi/
│  ├─ metaverse-core-v1.yaml
│  └─ examples/
│
├─ migrations/
│  ├─ 0001_initial.sql
│  ├─ 0002_roles.sql
│  └─ README.md
│
├─ sdk/
│  ├─ typescript/
│  │  ├─ package.json
│  │  ├─ tsconfig.json
│  │  ├─ src/
│  │  │  ├─ index.ts
│  │  │  ├─ client.ts
│  │  │  ├─ auth.ts
│  │  │  ├─ connection.ts
│  │  │  ├─ instance.ts
│  │  │  ├─ reconnect.ts
│  │  │  ├─ state-store.ts
│  │  │  ├─ generated/
│  │  │  └─ errors.ts
│  │  └─ test/
│  └─ test-vectors/
│     ├─ protocol/
│     ├─ snapshots/
│     └─ errors/
│
├─ apps/
│  ├─ reference-web/
│  │  ├─ README.md
│  │  ├─ package.json
│  │  └─ src/
│  ├─ admin-web/
│  │  ├─ README.md
│  │  ├─ package.json
│  │  └─ src/
│  └─ load-generator/
│     ├─ Cargo.toml
│     └─ src/
│        ├─ main.rs
│        ├─ scenario.rs
│        ├─ virtual_user.rs
│        └─ report.rs
│
├─ tests/
│  ├─ integration/
│  ├─ e2e/
│  ├─ protocol-compat/
│  ├─ migrations/
│  ├─ load/
│  ├─ soak/
│  ├─ chaos/
│  └─ security/
│
├─ deploy/
│  ├─ docker/
│  │  ├─ Dockerfile
│  │  └─ Dockerfile.dev
│  ├─ compose/
│  │  ├─ compose.yml
│  │  ├─ compose.dev.yml
│  │  └─ .env.example
│  ├─ systemd/
│  │  └─ metaverse-core.service
│  └─ kubernetes/
│     └─ README.md
│
├─ docs/
│  ├─ index.md
│  ├─ getting-started.md
│  ├─ architecture/
│  │  ├─ overview.md
│  │  ├─ domain-model.md
│  │  ├─ dependency-rules.md
│  │  ├─ realtime-flow.md
│  │  └─ scaling.md
│  ├─ protocol/
│  │  ├─ overview.md
│  │  ├─ lifecycle.md
│  │  ├─ compatibility.md
│  │  ├─ resume.md
│  │  └─ error-codes.md
│  ├─ operations/
│  │  ├─ deployment.md
│  │  ├─ configuration.md
│  │  ├─ backup.md
│  │  ├─ upgrade.md
│  │  ├─ rollback.md
│  │  ├─ observability.md
│  │  └─ incident-response.md
│  ├─ security/
│  │  ├─ threat-model.md
│  │  ├─ authentication.md
│  │  ├─ hardening.md
│  │  └─ disclosure.md
│  ├─ contributing/
│  │  ├─ setup.md
│  │  ├─ coding-style.md
│  │  ├─ testing.md
│  │  ├─ release-process.md
│  │  └─ good-first-issue.md
│  └─ adr/
│     ├─ README.md
│     ├─ 0001-rust.md
│     ├─ 0002-modular-monolith.md
│     ├─ 0003-websocket-protobuf.md
│     └─ 0004-no-inprocess-native-plugins.md
│
├─ examples/
│  ├─ minimal-client-typescript/
│  ├─ custom-extension/
│  ├─ local-auth-import/
│  └─ custom-entity-state/
│
├─ scripts/
│  ├─ bootstrap-dev.sh
│  ├─ check.sh
│  ├─ generate-protocol.sh
│  ├─ create-migration.sh
│  ├─ release.sh
│  └─ verify-generated.sh
│
└─ tools/
   ├─ protocol-inspector/
   ├─ snapshot-dump/
   └─ migration-checker/
```

---

## 30. Crate責務と依存規則

### 30.1 `domain`

- 純粋なドメイン型
- 値オブジェクト
- ドメインエラー
- ドメインルール
- 外部crate依存を最小化
- SQL、HTTP、Protobuf型を置かない

### 30.2 `application`

- ユースケース
- トランザクション境界
- repository等のport trait
- 認可呼び出し
- ドメインイベント生成

### 30.3 `protocol`

- Protobuf生成コード
- version negotiation
- protocol validation
- ドメイン型との変換

生成コードへ手編集してはならない。

### 30.4 `realtime`

- WebSocket接続
- connection registry
- heartbeat
- resume
- outbound queue
- protocol frame処理

ワールドのドメイン状態を直接所有しない。

### 30.5 `world-runtime`

- Instance Actor
- 一時状態
- command処理
- snapshot/delta生成
- 所有権
- lifecycle

HTTPやDB実装へ依存しない。

### 30.6 `interest`

- 空間index
- visibility
- subscription計算
- 配信対象決定

描画LODやアセットを扱わない。

### 30.7 `storage-postgres`

- repository実装
- SQL
- transaction
- DB行とドメイン型の変換

### 30.8 `server`

Composition Rootのみを担当する。

- 設定読込
- 各実装の生成
- dependency wiring
- server起動
- graceful shutdown

ビジネスロジックを置かない。

---

## 31. コーディング規約

### 31.1 Rust

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `unsafe`は原則禁止
- 必要な`unsafe`はADR、SAFETYコメント、専用テスト必須
- `unwrap()`、`expect()`を本番pathで原則禁止
- `panic!`を入力エラー処理に使わない
- public itemへrustdoc
- errorは型付きで扱う
- ログ文字列をエラーコード代わりにしない

### 31.2 モジュールサイズ

- 1ファイルが肥大化したら責務で分割する。
- `mod.rs`へ大量実装を置かない。
- public re-exportを明示的に管理する。
- util/common/genericな雑多crateを作らない。

### 31.3 型設計

Primitive Obsessionを避ける。

```rust
pub struct UserId(Uuid);
pub struct InstanceId(Uuid);
pub struct LoginId(String);
pub struct Revision(u64);
```

異なるIDを同じ`Uuid`引数で取り違えないようにする。

### 31.4 時刻

- DB保存はUTC
- APIはRFC 3339
- realtime protocolはUnix milliseconds等、明示形式
- テストではClock traitを注入する

---

## 32. テスト戦略

### 32.1 Unit Test

対象:

- 値オブジェクト
- 権限計算
- transform validation
- sequence/revision
- Interest Management
- queue集約
- resume判定

### 32.2 Integration Test

- PostgreSQL repository
- migration
- REST API
- WebSocket handshake
- login → join → state update → leave
- reconnect → resume
- user disable → session revoke

Testcontainers等により実DBを使用する。

### 32.3 Protocol Compatibility Test

- 旧クライアントfixtureを保存
- 新サーバーでdecode可能か
- reserved field numberを再利用していないか
- breaking changeをCIで検出
- Golden test vectorsをSDK間で共有

### 32.4 Property Test

- 任意の有限Transform
- encode/decode round trip
- Spatial Gridの包含条件
- 権限組合せ
- sequence order

### 32.5 Fuzz Test

- Protobuf decoder
- WebSocket frame処理
- custom component validation
- login ID parser
- configuration parser

### 32.6 Load Test

シナリオ:

1. 大量同時ログイン
2. 段階的接続増加
3. 全員が10 Hzで移動
4. 10%が頻繁に再接続
5. slow consumer混在
6. 1インスタンス集中
7. 複数インスタンス分散

記録:

- CPU
- RSS memory
- network throughput
- P50/P95/P99 latency
- dropped update
- queue depth
- disconnect
- snapshot size

### 32.7 Soak Test

24〜72時間連続で実行し、以下を確認する。

- memory leak
- connection leak
- task leak
- revision overflow兆候
- DB connection枯渇
- queue増大

### 32.8 Chaos Test

- DB一時停止
- ネットワーク遅延
- パケットロス
- プロセスSIGTERM
- client強制切断
- reverse proxy再起動
- clock skew

---

## 33. CI/CD

### 33.1 Pull Request CI

必須:

1. format
2. clippy
3. unit tests
4. integration tests
5. documentation build
6. dependency license check
7. vulnerability audit
8. protocol compatibility
9. generated code差分確認
10. migration check

### 33.2 Nightly CI

- load smoke test
- fuzz短時間実行
- sanitizer可能範囲
- unused dependency check
- documentation link check
- container scan

### 33.3 Release CI

- tag検証
- changelog検証
- reproducible buildに近い手順
- Linux amd64/arm64 image
- SBOM生成
- checksum
- container signingを推奨
- GitHub Release生成
- migration notes添付

---

## 34. リリースと互換性

### 34.1 Semantic Versioning

- MAJOR: 公開APIまたはprotocol majorの破壊的変更
- MINOR: 後方互換機能追加
- PATCH: バグ修正、セキュリティ修正

### 34.2 バージョンを分ける

以下を独立管理する。

- Server Version
- REST API Version
- Realtime Protocol Version
- Database Schema Version
- TypeScript SDK Version
- Extension API Version

### 34.3 Deprecation

- 非推奨化を先に行う
- 少なくとも1 MINOR期間の移行猶予を推奨
- 削除予定バージョンを明記
- telemetryで旧機能利用を確認可能にする

### 34.4 Protocol変更ルール

- field numberを再利用しない
- 削除fieldはreservedにする
- required相当の後付けを避ける
- unknown fieldを許容する
- enum追加を想定する
- major negotiationを実装する

---

## 35. OSSメンテナンス方針

### 35.1 必須ドキュメント

- README.md
- CONTRIBUTING.md
- CODE_OF_CONDUCT.md
- SECURITY.md
- GOVERNANCE.md
- MAINTAINERS.md
- CHANGELOG.md
- LICENSE
- Roadmap
- Architecture Decision Records

### 35.2 Governance

初期案:

- Maintainer
- Reviewer
- Contributor

権限昇格条件を文書化する。

例:

- 継続的な質の高い貢献
- レビュー実績
- 行動規範の遵守
- 特定モジュールへの理解
- セキュリティ情報の取扱能力

### 35.3 CODEOWNERS

高リスク領域は明示的なowner承認を必要とする。

```text
/proto/                 @protocol-maintainers
/migrations/            @database-maintainers
/crates/auth/           @security-maintainers
/crates/realtime/       @realtime-maintainers
/.github/workflows/     @release-maintainers
/SECURITY.md            @security-maintainers
```

### 35.4 Issue Labels

```text
area/auth
area/protocol
area/realtime
area/world-runtime
area/interest
area/storage
area/sdk
area/docs
area/operations
kind/bug
kind/feature
kind/performance
kind/security
kind/refactor
priority/critical
priority/high
priority/normal
good-first-issue
help-wanted
breaking-change
needs-rfc
```

### 35.5 変更提案

小変更はIssue/PRで扱う。以下はRFCまたはADR必須とする。

- protocol変更
- DB破壊的変更
- 認証方式変更
- plugin model変更
- 依存方向変更
- 新しいネットワークtransport
- unsafe導入
- ライセンス変更

### 35.6 セキュリティ報告

- 公開Issueへ脆弱性を書かせない
- SECURITY.mdに非公開報告方法を記載
- 受付確認
- 影響調査
- 修正版準備
- advisory公開
- CVE取得を検討

---

## 36. ライセンス案

推奨は以下のいずれか。

### 案A: Apache-2.0

- 商用利用しやすい
- 特許条項が明確
- 企業導入との相性がよい

### 案B: MIT OR Apache-2.0

Rustエコシステムで一般的なデュアルライセンス形式。利用者が選択できる。

AGPLは改変公開を強く求められる一方、企業導入の障壁になり得る。本プロジェクトが導入支援や個別フロント開発を収益源にする場合、Apache-2.0系の方が普及しやすいと考える。

名称、ロゴ、公式サービス名はソフトウェアライセンスと別に商標ポリシーを定めてもよい。

---

## 37. デプロイ仕様

### 37.1 最小構成

```text
Reverse Proxy / TLS
        ↓
metaverse-core-server
        ↓
PostgreSQL
```

### 37.2 Docker Compose

```yaml
services:
  core:
    image: ghcr.io/example/metaverse-core:0.1
    environment:
      DATABASE_URL: postgres://...
      METAVERSE_PUBLIC_URL: https://meta.example.org
    depends_on:
      postgres:
        condition: service_healthy

  postgres:
    image: postgres:17
    volumes:
      - postgres_data:/var/lib/postgresql/data

volumes:
  postgres_data:
```

実際のイメージtagは固定し、`latest`を本番例で推奨しない。

### 37.3 Graceful Shutdown

1. ready=false
2. 新規接続拒否
3. 新規インスタンス作成停止
4. 既存WebSocketへshutdown通知
5. checkpoint対象を保存
6. 接続drain
7. DB pool close
8. 終了

### 37.4 バックアップ

- PostgreSQLバックアップ
- 暗号鍵、設定、secretの安全な保管
- 復元手順を定期テスト
- バックアップ取得だけでなくrestore testを行う

---

## 38. 脅威モデル概要

想定脅威:

- ブルートフォースログイン
- 不正token
- 失効後token再利用
- 大量WebSocket接続
- 巨大メッセージ
- 高頻度transform spam
- NaN/Infinity注入
- 他ユーザーエンティティ操作
- 権限昇格
- snapshot情報漏洩
- webhook SSRF
- 監査ログ改ざん
- 依存ライブラリ脆弱性

対策:

- rate limit
- token rotation/revocation
- size limit
- typed validation
- server authoritative ownership
- permission check
- visibility filter
- webhook宛先制限
- structured audit
- dependency scanning

詳細は`docs/security/threat-model.md`で管理する。

---

## 39. 初期ロードマップ

### Milestone 0: Repository Foundation

- Cargo Workspace
- CI
- lint/format
- 基本文書
- ADR
- Docker開発環境

### Milestone 1: Identity and Administration

- ローカル認証
- ユーザー管理
- ロール、権限
- PostgreSQL migration
- REST API
- 監査ログ

### Milestone 2: Realtime Minimum

- WebSocket handshake
- protocol v1
- Instance作成
- join/leave
- 位置同期
- snapshot/delta
- TypeScript SDK

### Milestone 3: Reliability

- heartbeat
- reconnect
- resume token
- resync
- bounded queue
- rate limiting
- graceful shutdown

### Milestone 4: Generic Entity Runtime

- entity spawn/update/delete
- ownership
- custom component
- persistence checkpoint
- extension events

### Milestone 5: Scaling

- Uniform Grid
- visibility policy
- load generator
- 200〜1000接続試験
- soak test
- performance documentation

### Milestone 6: OSS Usability

- reference web
- minimal admin web
- SDK examples
- deployment guide
- upgrade/rollback guide
- first stable beta

---

## 40. MVP完了条件

MVPは以下をすべて満たした時点とする。

1. 管理者がユーザーを作成できる
2. ユーザーがIDとパスワードでログインできる
3. 匿名接続が拒否される
4. 管理者がWorld DefinitionとInstanceを作成できる
5. 2つ以上の異なるクライアントが同一Instanceへ参加できる
6. 一方のTransform更新が他方へ配信される
7. サーバーが不正Transformを拒否できる
8. 切断後に自動再接続できる
9. ResumeまたはSnapshot再取得で状態復旧できる
10. 最新位置更新が送信詰まり時に集約される
11. エンティティを作成、更新、削除できる
12. 所有権と権限が検証される
13. Docker Composeで起動できる
14. PostgreSQLバックアップ・復元手順がある
15. CIでprotocol互換性を検査できる
16. 負荷試験結果を再現できる
17. SECURITY.mdと脆弱性報告経路がある
18. CONTRIBUTING.mdに開発環境構築手順がある

---

## 41. 将来検討事項

- LDAP/AD/OIDC/SAML adapter
- マルチテナント
- WebTransport/QUIC
- Redis/NATS等の内部message bus
- Instanceの別Node配置
- 空間sharding
- WASM extension runtime
- 永続Event Store
- CRDTを使った一部共同編集
- 管理者向け自動アップデート支援
- 複数地域配置
- SDK追加
- Federation

これらを初期コアへ先行実装しない。

---

## 42. 未決定事項

実装開始前にADRまたはRFCで決定する。

1. 正式プロジェクト名
2. Apache-2.0単独かMIT/Apache-2.0デュアルか
3. Access Token形式
4. Refresh Tokenのブラウザ運用方式
5. Protocol Buffersのコード生成ツール
6. JSONデバッグプロトコルを正式提供するか
7. World Instanceの自動生成規則
8. custom componentの最大サイズ
9. 初期tick rate
10. transform validationのデフォルト値
11. snapshot差分履歴の保持量
12. Webhook拡張をMVPへ含めるか
13. admin-webを同一リポジトリで管理するか
14. 公式TypeScript SDKの対応ランタイム範囲

---

## 43. ADRテンプレート

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

---

## 44. Pull Requestチェックリスト案

```markdown
- [ ] IssueまたはRFCへリンクした
- [ ] 変更範囲を説明した
- [ ] Unit testを追加した
- [ ] Integration testを追加または不要理由を記載した
- [ ] Protocol互換性を確認した
- [ ] DB migrationの後方互換性を確認した
- [ ] セキュリティ影響を確認した
- [ ] メトリクス/ログへの影響を確認した
- [ ] ドキュメントを更新した
- [ ] CHANGELOG対象か確認した
- [ ] Generated codeを再生成した
- [ ] 負荷影響がある場合ベンチマークを添付した
```

---

## 45. 開発者向け最短起動体験

新規コントリビューターは以下で起動できることを目標とする。

```bash
git clone https://example.org/metaverse-core.git
cd metaverse-core
cp deploy/compose/.env.example .env
docker compose -f deploy/compose/compose.dev.yml up -d
cargo run -p metaverse-core-server
```

別ターミナル:

```bash
pnpm install
pnpm --filter reference-web dev
```

`./scripts/bootstrap-dev.sh`で依存確認、DB起動、migration、初期管理者作成まで自動化してよい。

---

## 46. 参考となる公式仕様・ドキュメント

実装時は、最新の公式文書を確認して依存バージョンを決定する。

- Rust Documentation: https://doc.rust-lang.org/stable/
- Cargo Workspaces: https://doc.rust-lang.org/cargo/reference/workspaces.html
- Tokio Tutorial: https://tokio.rs/tokio/tutorial
- Axum Documentation: https://docs.rs/axum/latest/axum/
- Protocol Buffers Programming Guides: https://protobuf.dev/programming-guides/
- OpenAPI Specification: https://spec.openapis.org/oas/latest.html
- Semantic Versioning: https://semver.org/
- OWASP ASVS: https://owasp.org/www-project-application-security-verification-standard/

---

## 47. 最終要約

本プロジェクトは、メタバースの見た目を提供するものではない。

提供するのは、次の責任に限定されたOSSバックエンドコアである。

> 認証済みの複数クライアントが同一の仮想ワールドへ接続し、ユーザーと任意エンティティの状態を、安全に、用途非依存で、リアルタイムに共有するための基盤。

Rust製コアはワールドの正しい状態、認証、権限、接続、再接続、同期、配信対象、永続化を管理する。フロントエンド、描画、アセット、音声、学校・企業固有機能は外部実装へ委ねる。

最初は単一ノードのモジュラーモノリスとして完成度と保守性を優先し、負荷試験に基づいてインスタンス単位の水平分割へ進む。OSSとして長期運用できるよう、プロトコル互換性、ADR、CODEOWNERS、セキュリティ報告、CI、移行手順、負荷試験を初期段階からプロジェクト構造へ含める。
