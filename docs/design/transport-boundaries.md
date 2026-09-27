# OrbiSync 通信境界設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §13（通信方式）を、REST と WebSocket の責務分離、protocol/domain DTO 変換、失敗・整合性境界の設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: 仕様書で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項

モジュール所有権と DAG は `architecture.md` §3 に従う。

## 1. 通信方式の責務分離

### 1.1 管理・制御 API（REST）

**[SPEC]** HTTPS REST API を使用する（仕様 §13.1）。

**所有モジュール:** `http_api`（inbound adapter）

| 責務 | 対象操作 |
|---|---|
| 認証 | ログイン、トークン更新、ログアウト、パスワード変更 |
| ユーザー管理 | CRUD、有効化/無効化、パスワードリセット、CSV インポート |
| ロール管理 | CRUD、ユーザーへのロール割当 |
| ワールド管理 | CRUD、アーカイブ |
| インスタンス管理 | 作成、開始、停止、kick、メンバー一覧 |
| 監査 | イベント参照 |
| 運用 | ヘルスチェック、メトリクス、バージョン |

**[SPEC]** すべて `/v1` 配下とする（仕様 §21）。ただし、バージョン非依存の運用 endpoint である `/health/live`、`/health/ready`、`/version` はこの規約の例外とし、非 `/v1` のまま提供する。

**[REC]** REST API は要求-応答の同期モデルであり、状態のプッシュ配信には使用しない。

### 1.2 リアルタイム通信（WebSocket）

**[SPEC]** WebSocket Secure を標準とする（仕様 §13.2）。

**所有モジュール:** `realtime_gateway`（inbound adapter、socket 所有）、`realtime_delivery`（outbound queue 調停）

| 責務 | 対象操作 |
|---|---|
| ハンドシェイク | ClientHello / ServerHello、version negotiation |
| 入退室 | JoinInstance / JoinAccepted |
| 状態同期 | TransformInput、Snapshot、StateDelta |
| エンティティ操作 | EntityCommand（spawn/update/delete/transfer_ownership） |
| イベント | DomainEvent 配信 |
| 接続維持 | Heartbeat / HeartbeatAck |
| 再接続 | ResumeSession / ResumeAccepted / ResyncRequired |
| エラー | ErrorMessage |

**[SPEC]** 将来、WebTransport/QUIC を実験的 transport として追加してよいが、WebSocket 互換性を失ってはならない（仕様 §13.2）。

### 1.3 REST と WebSocket の境界

**[REC]** 両 transport の責務分離：

| 観点 | REST（`http_api`） | WebSocket（`realtime_gateway`） |
|---|---|---|
| 通信モデル | 要求-応答 | 双方向ストリーム |
| 認証 | Bearer Access Token（header） | ClientHelloの単一使用Realtime接続ticketを検証（subject/AuthSessionIdを解決） |
| 認可 | リソース単位 | インスタンス・エンティティ単位 |
| 状態変更 | 永続データ（ユーザー、ワールド等） | 一時状態（位置、プレゼンス等） |
| エラー形式 | JSON error envelope（§21.8） | Protobuf ErrorMessage |
| 監査 | 全管理操作 | 入室・退室・kick・所有権移譲 |
| 冪等性 | **[ADR]** | sequence / latest-wins |

**[SPEC]** 公式フロントエンドで実行できる操作は、すべて公開 API または公開プロトコルの少なくとも一方から実行できなければならない（仕様 §3.2）。フロントエンド専用で、いずれの公開面にも存在しない操作を作ってはならない。

**[REC]** 各操作は上記の責務分離に従い REST または WebSocket のいずれか一方へ配置する。両 transport への重複配置は要求しない。

## 2. プロトコル形式

### 2.1 本番形式

**[SPEC]** 本番モードは Protocol Buffers を標準とする（仕様 §13.3）。

- スキーマを言語非依存に保てる
- 多言語 SDK を生成しやすい
- JSON よりサイズを抑えやすい
- field number を維持することで互換性を管理できる

### 2.2 デバッグ形式

**[SPEC]** デバッグ用に JSON サブプロトコルを提供してよい（仕様 §13.3）。ただし、JSON と Protobuf で意味論を変えてはならない。

**[REC]** JSON debug mode は本番初期値で無効とする。有効化は設定ファイルまたは環境変数で行う。

**[ADR]** JSON debug mode を正式提供するか、開発時のみに限定するかは未決定（仕様 §42.6）。

### 2.3 WebSocket サブプロトコル

**[SPEC]** 仕様 §13.4 の例：

```text
Sec-WebSocket-Protocol: metaverse.v1.protobuf
Sec-WebSocket-Protocol: metaverse.v1.json
```

**[SPEC]** サーバーは未対応サブプロトコルを明示的に拒否する（仕様 §13.4）。

**[REC]** サブプロトコル名は `<project>.v<major>.<encoding>` の形式とする。

**設計前提** ADR-001により、WebSocket subprotocolを`orbisync.v1.protobuf`、開発時限定のJSON debug modeを採用する場合は`orbisync.v1.json`とする。Protocol Buffers packageは`orbisync.v1`とする（`docs/adr/ADR-001-naming-identifiers.md`）。

## 3. Protocol / Domain DTO 変換

### 3.1 変換の原則

**[REC]** Protocol DTO と domain model の相互変換は adapter 境界に閉じ込める（`architecture.md` §2.1）。

**[SPEC]** 生成コードへ手編集してはならない（仕様 §30.3）。

**[SPEC]** protocol 型と domain 型は明示的に変換する（仕様 §7.3）。

### 3.2 変換箇所

**[REC]** 変換は以下の境界で行う：

```text
Inbound:
  realtime_gateway: Protobuf bytes → protocol DTO → application command
  http_api: JSON body → REST DTO → application command

Outbound:
  application result → protocol DTO → Protobuf bytes (realtime_gateway)
  application result → REST DTO → JSON body (http_api)
```

**[REC]** 変換コードの所有：

| 変換 | 所有モジュール | 方向 |
|---|---|---|
| Protobuf ↔ application command | `realtime_gateway` | inbound |
| application result → Protobuf | `realtime_gateway` | outbound |
| JSON ↔ application command | `http_api` | inbound |
| application result → JSON | `http_api` | outbound |
| protocol DTO ↔ domain type | `protocol` crate（変換ユーティリティ） | 両方向 |

**[SPEC]** Application の公開関数は HTTP/DB/WebSocket 型を引数・戻り値にしない（仕様 §7.3、`architecture.md` §2.1）。

### 3.3 変換時の検証

**[REC]** 変換は検証の機会として使用する：

- **Inbound 変換時**: 構文的検証（フィールド存在、型、サイズ上限）
- **Domain 変換時**: 意味的検証（NaN/Infinity、値域、権限）は domain 層が実施
- **Outbound 変換時**: 機密情報の除外（パスワードハッシュ、内部 ID 等）

**[SPEC]** NaN、Infinity、範囲外値を拒否する（仕様 §9.7）。

### 3.4 互換性管理

**[SPEC]** field number を再利用しない。削除 field は reserved にする（仕様 §34.4）。

**[SPEC]** unknown field を許容する。enum 追加を想定する（仕様 §34.4）。

**[REC]** 変換コードは unknown field を silently drop せず、ログまたはメトリクスで観測する。

**設計前提** ADR-004によりprotocol major不一致は接続拒否、minorは互換範囲へnegotiationする。

## 4. 失敗・整合性境界

### 4.1 Transport 層の失敗

**[REC]** transport 層で扱う失敗と応答：

| 失敗 | 検知箇所 | 応答 |
|---|---|---|
| 不正なサブプロトコル | `realtime_gateway`（handshake） | HTTP 400 / 接続拒否 |
| メッセージサイズ超過 | `realtime_gateway` | ErrorMessage + 接続切断 |
| 不正な Protobuf | `realtime_gateway`（decode） | ErrorMessage |
| 認証 token 無効 | `http_api` / `realtime_gateway` | HTTP 401 / ErrorMessage |
| 権限不足 | coordinator（application） | HTTP 403 / ErrorMessage |
| rate limit 超過 | `realtime_gateway` / `http_api` | HTTP 429 / 接続切断 |

### 4.2 Application 層の失敗

**[REC]** application 層で扱う失敗：

| 失敗 | 検知箇所 | 応答 |
|---|---|---|
| ドメイン検証失敗 | `instance_runtime` | ErrorMessage（状態変更なし） |
| 所有権違反 | `instance_runtime` | ErrorMessage |
| リソース不存在 | 各 application service | HTTP 404 / ErrorMessage |
| 競合（revision 不一致） | 各 application service | HTTP 409 / ResyncRequired |
| 内部エラー | 各 application service | HTTP 500 / ErrorMessage（詳細非公開） |

### 4.3 整合性境界

**[SPEC]** Transport エラーは公開エラーコードへ変換し、内部 DB/ライブラリエラーを露出しない（仕様 §21.8、`architecture.md` §6）。

**[REC]** エラー情報の境界：

| 公開してよいもの | 公開してはならないもの |
|---|---|
| 機械可読エラーコード | 内部スタックトレース |
| 人間向けメッセージ（互換性非保証） | DB クエリ詳細 |
| request_id | 内部モジュール名 |
| 検証失敗のフィールド名（管理 API） | token / パスワード |

**[SPEC]** エラーフォーマット（REST、仕様 §21.8）：

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

**[SPEC]** `code` は機械可読で安定させる。`message` は人間向けであり互換性を保証しない（仕様 §21.8）。

### 4.4 WebSocket 接続の失敗隔離

**[SPEC]** 遅いクライアント 1 台がインスタンス全体を遅延させてはならない（仕様 §18.1）。

**[REC]** 接続ごとの隔離：

- 送信キューは接続別に独立（`realtime_delivery`）
- 受信検証の失敗は当該接続のみに影響
- 1 接続の panic が他接続やインスタンス runtime に伝播しない

**[SPEC]** 接続単位 rate limit、ユーザー単位 rate limit、IP 単位ログイン rate limit、インスタンス単位総入力上限を設定する（仕様 §18.4）。

**[ADR]** rate limit の具体値（毎秒メッセージ数、バイト数）は負荷試験で決める。

### 4.5 REST と WebSocket の障害伝播

**[REC]** 一方の transport の障害が他方に伝播しないことを保証する：

- REST の DB 遅延が WebSocket の heartbeat を遅延させない
- WebSocket の接続殺到が REST の管理 API を応答不能にしない
- 共有するのは application service の command/query 入口まで

**[ADR]** REST と WebSocket で Tokio runtime を分離するか、同一 runtime で priority を付けるかは負荷試験で決める。

## 5. メッセージ制限

**[SPEC]** 初期推奨値（仕様 §14.4）：

| 種別 | 上限 |
|---|---|
| 通常リアルタイムメッセージ | 16 KiB 以下 |
| カスタムイベント | 64 KiB 以下 |
| スナップショット | 分割送信可能 |

**[SPEC]** 圧縮後・展開後の両方に上限を設定する（仕様 §14.4）。

**[SPEC]** 1 接続あたり毎秒メッセージ数を制限する（仕様 §14.4）。

**[SPEC]** 巨大データやアセットを WebSocket へ流してはならない（仕様 §14.4）。

**設計前提** ADR-004によりv1初期は圧縮無効とし、測定後にpermessage-deflateをopt-in追加できる。

## 6. 要 ADR 事項

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| TB-01 | REST API の冪等性 | POST 以外は冪等、POST は Idempotency-Key header | 管理操作の再試行安全性 |
| TB-02 | JSON debug mode の提供範囲 | 開発時のみ、本番は無効 | 情報露出と処理負荷の回避 |
| TB-03 | サブプロトコル名 | **Accepted: `orbisync.v1.protobuf`** | ADR-001。JSON debug採用時は`orbisync.v1.json` |
| TB-04 | version negotiation 失敗時 | 接続拒否 + ErrorMessage | 互換性のないクライアントの早期排除 |
| TB-05 | 圧縮方式 | **Accepted: v1初期は無効** | 測定後にpermessage-deflateをopt-in候補とする |
| TB-06 | rate limit 具体値 | 100 msg/s（通常）、10 msg/s（カスタムイベント） | 負荷試験で調整 |
| TB-07 | REST/WS の runtime 分離 | 初期は同一 runtime | 複雑性回避。負荷試験で分離を検討 |
