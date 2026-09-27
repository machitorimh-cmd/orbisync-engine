# OrbiSync 状態分類と実行モデル設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §10（状態分類）、§11（サーバー権威）、§12（インスタンス実行モデル）を、実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: 仕様書で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項

モジュール所有権と DAG は `architecture.md` §3 に従う。

## 1. 状態分類

### 1.1 一時状態（Ephemeral）

**[SPEC]** 主にメモリ上で管理し、通常は毎更新 DB へ保存しない（仕様 §10.1）：

| 状態 | 所有モジュール | 更新頻度 | 消失時の影響 |
|---|---|---|---|
| 現在位置・向き・速度 | `instance_runtime` | 高（10 Hz 以上） | 最新スナップショットで復旧 |
| アニメーション状態 | `instance_runtime` | 高 | 同上 |
| 接続状態 | `realtime_presence` | 中 | 再接続で再確立 |
| 短期的プレゼンス | `realtime_presence` | 中 | 同上 |
| 一時的エンティティ | `instance_runtime` | 可変 | インスタンス再起動で消失 |
| Interest 購読状態 | `interest` | 中 | 再計算可能 |
| 送信キュー内容 | `realtime_delivery` | 高 | 接続再確立で再構築 |

**[SPEC]** 位置情報のような高頻度状態を毎回 DB へ書き込んではならない（仕様 §10.3）。

### 1.2 永続状態（Persistent）

**[SPEC]** PostgreSQL へ保存する（仕様 §10.2）：

| 状態 | 所有モジュール | 保存トリガー |
|---|---|---|
| ユーザー・資格情報 | `identity_access` | 管理操作 |
| ロール・権限 | `identity_access` | 管理操作 |
| ワールド定義 | `world_directory` | 管理操作 |
| 永続エンティティ定義 | `instance_runtime` → `persistence` | spawn/delete、checkpoint |
| 永続コンポーネント | `instance_runtime` → `persistence` | 更新、checkpoint |
| インスタンス設定 | `world_directory` | 管理操作 |
| 管理操作履歴 | `audit_observability` | 操作の都度 |
| 監査ログ | `audit_observability` | イベントの都度 |
| 拡張登録情報 | `extension_gateway` | 管理操作 |

**[REC]** 永続化の書き込みは `persistence` モジュールが所有する repository port を介して行う。`instance_runtime` は DB へ直接書き込まない。

### 1.3 チェックポイント

**[SPEC]** 必要に応じ、ワールドインスタンスの状態をチェックポイントとして保存できる（仕様 §10.3）：

- 手動保存
- 一定時間ごとの保存
- インスタンス終了時保存
- 永続化対象コンポーネントのみ保存

**[REC]** チェックポイントは `instance_runtime` が正準状態のスナップショットを生成し、`persistence` port へ渡す。一時状態のうち位置・速度等はチェックポイント対象に含めない。

**[ADR]** チェックポイントの間隔、保持数、復元手順、対象コンポーネントの選定基準は ARC-08 で決める。

### 1.4 状態分類の判断基準

**[REC]** 新たな状態を追加する際の分類基準：

| 基準 | 一時 | 永続 |
|---|---|---|
| プロセス再起動後に必要か | 不要（再計算/再取得可能） | 必要 |
| 更新頻度 | 高（秒間複数回） | 低（管理操作・イベント） |
| 整合性要求 | 最新値のみ意味を持つ | 履歴・監査が必要 |
| 消失許容度 | 許容（スナップショットで復旧） | 不許容 |

## 2. サーバー権威モデル

### 2.1 原則

**[SPEC]** サーバーがワールドの正しい状態を決定する（仕様 §11.1）。クライアントは状態を確定するのではなく、入力または状態変更要求を送る。

```text
Client: 「この位置へ移動したい」（入力/要求）
Server: セッション、権限、速度、範囲を検証
Server: 正式状態を更新
Server: 関係クライアントへ更新配信
```

**[REC]** 権威状態の正本は `instance_runtime` が所有する。他モジュールは `instance_runtime` の command 入口を経由せずに状態を変更しない（`architecture.md` §3.1 DAG 遵守）。

### 2.2 入力検証境界

**[SPEC]** 最低限、以下を検証する（仕様 §11.2）：

| 検証項目 | 検証箇所 | 失敗時の応答 |
|---|---|---|
| 認証済みセッションか | `identity_access` → coordinator | ErrorMessage（認証エラー） |
| 対象インスタンスへ参加中か | `realtime_presence` → coordinator | ErrorMessage（不参加） |
| 対象エンティティを操作できるか | `instance_runtime`（所有権） | ErrorMessage（権限不足） |
| 数値が有限か | `instance_runtime`（Transform 検証） | ErrorMessage（不正値） |
| 最大速度を超えていないか | `instance_runtime` | ErrorMessage（`world.speed_acceleration_check_enabled`で無効化可、ADR-024） |
| 最大更新頻度を超えていないか | `realtime_gateway` / `instance_runtime` | 警告、rate limit |
| 最大移動距離を超えていないか | `instance_runtime` | ErrorMessage または clamp |
| ワールド境界を超えていないか | `instance_runtime` | ErrorMessage または clamp |
| メッセージサイズ上限内か | `realtime_gateway` | 接続切断 |
| プロトコルバージョンが互換か | `realtime_gateway` | 接続拒否 |

**[ADR]** 上表のうち「認証済みセッションか」「対象インスタンスへ参加中か」「対象エンティティを操作できるか」「数値が有限か」「最大更新頻度を超えていないか」「メッセージサイズ上限内か」「プロトコルバージョンが互換か」はCoreが常に保証する不変条件であり、用途によらず無効化しない。「最大速度を超えていないか」（最大加速度を含む、`instance_runtime`内では単一の速度判定式に統合されている）は、外部の物理エンジンなど用途固有ロジックが妥当性を担保するケースを想定し、`world.speed_acceleration_check_enabled`（既定`true`）で無効化できる（ADR-024）。「最大移動距離を超えていないか」（teleport防止）と「ワールド境界を超えていないか」は本ADRの対象に含めず、既存の挙動を維持する。

**[REC]** 検証は 2 段階で行う：

1. **Transport 層**（`realtime_gateway`）: 構文的検証（サイズ、形式、version）
2. **Domain 層**（`instance_runtime`）: 意味的検証（権限、物理的妥当性、状態整合性）

Transport 層の検証失敗は即座に接続レベルのエラーを返す。Domain 層の検証失敗は ErrorMessage としてクライアントへ返し、接続は維持する。

**[ADR]** 速度超過・境界超過時に「拒否」するか「clamp（補正）」するかは用途に影響する。初期方針を ADR で決める。

**[ADR]** ADR-025（`extension-mechanism.md` §12）は、entity mutationコマンド（spawn/update/delete相当、transform updateを除く）に対し、オプトインの状態確定前Extension検証フックを追加する。このフックはCoreが専有する上表の不変条件を迂回しない。フックはCoreが許可した操作への追加の拒否権のみを持ち、Coreが拒否する操作を許可へ変える権限を持たない。フックは`instance_runtime`のcommand入口（mailbox）へ到達する前の同期問い合わせであり、actor内（本節が前提とする検証箇所）では実行しない。

### 2.3 物理・衝突判定

**[SPEC]** 本コアは汎用物理エンジンを内蔵しない（仕様 §11.3）。

初期版で提供するもの：

- **[REC]** AABB 等による任意の簡易境界検証を追加可能な hook
- **[SPEC]** ワールド全体の座標範囲制限
- **[SPEC]** 最大速度、最大加速度、テレポート権限
- **[REC]** 用途固有検証サービスへの委譲（外部拡張経由）

**[ADR]** 境界検証 hook の API 形状、登録方式、実行タイミングは未決定。

### 2.4 所有権モデル

**[SPEC]** エンティティは owner を持てる（仕様 §20.3）：

- owner のみ更新可能
- 任意権限保持者は更新可能（`entities.update.any`）
- サーバーのみ更新可能
- 所有権移譲イベントを監査可能

**[SPEC]** クライアントの自己申告 owner を信用しない（仕様 §20.3）。

**[REC]** 所有権検証は `instance_runtime` の command 処理内で行う。coordinator は所有権判定を `instance_runtime` に委譲し、transport 層で先取りしない。

## 3. インスタンス実行モデル

### 3.1 Actor 形式

**[SPEC]** 各 World Instance は、単一の論理所有者が状態更新を直列化する Actor 形式を推奨する（仕様 §12.1）。

**[REC]** `instance_runtime` はインスタンスごとに単一の command 入口（mailbox）を持ち、全状態遷移を直列化する（`architecture.md` §10 ARC-02）。

```text
InstanceCommand:
├── Join(JoinRequest)
├── Leave(LeaveRequest)
├── Resume(ResumeRequest)
├── UpdateTransform(TransformUpdate)
├── UpdateEntity(EntityUpdate)
├── SpawnEntity(SpawnRequest)
├── DeleteEntity(DeleteRequest)
├── TransferOwnership(TransferRequest)
├── PublishEvent(PublishEvent)
├── Admin(AdminCommand)
├── Tick
└── Shutdown
```

**[SPEC]** Actor 形式の利点（仕様 §12.1）：
- ワールド状態全体を多数の Mutex で保護せずに済む
- 状態更新順序を理解しやすい
- テストで決定論的に再現しやすい
- 将来、インスタンス単位で別プロセスへ移動しやすい

### 3.2 Mailbox

**[SPEC]** bounded channel を使用する。キュー上限を設定する（仕様 §12.2）。

**[SPEC]** 位置更新は同一ユーザーの古い要求を集約できる（仕様 §12.2）。

**[SPEC]** 管理イベントや永続イベントを位置更新と同じ優先度にしない（仕様 §12.2）。

**[SPEC]** キュー飽和時の挙動をメトリクスへ記録する（仕様 §12.2）。

**[REC]** mailbox の設計：

| キュー種別 | 優先度 | 集約 | 飽和時動作 |
|---|---|---|---|
| 管理command（内部Join/Leave/Admin。Leaveはwire payloadではない） | 高 | なし | 接続切断 |
| 位置更新（TransformInput） | 通常 | 同一 entity の最新のみ | 古い更新を drop |
| エンティティ操作（Spawn/Delete） | 通常 | なし | 送信側へエラー |
| Tick | 内部 | — | — |
| Shutdown | 最高 | — | 即時処理 |

**[ADR]** mailbox の容量上限値、優先度キューの実装方式（単一 channel + 優先度 vs 複数 channel）は負荷試験で決める。

### 3.3 Tick

**[SPEC]** インスタンスは固定または適応 tick を使用できる（仕様 §12.3）。

**[SPEC]** 推奨初期値（仕様 §12.3）：
- サーバー状態 tick: 10〜20 Hz
- 停止中ユーザーの更新: 1 Hz 以下
- クライアント描画: コアの責務外

**[SPEC]** Tick 値は管理画面へ露出させず、設定ファイルまたは上級者向け設定とする（仕様 §12.3）。

**[REC]** Tick の責務：
- 集約された位置更新の確定と配信準備
- Interest 再計算のトリガー（移動があった場合のみ）
- タイムアウト検知（idle connection、resume grace 期限）
- チェックポイント判定

**設計前提** ADR-006によりactive 20Hz固定tick、idle 1Hz、管理→reliable→latest-wins→timeout/checkpointの処理順を採用する。

### 3.4 状態更新の配信フロー

**[REC]** `instance_runtime` 内の状態更新から配信までのフロー（`architecture.md` §5.3 準拠）：

```text
1. command が mailbox から取り出される
2. domain validation（§2.2 の検証）
3. 正準状態の更新（revision 増加）
4. state view / domain event を coordinator へ返す
5. coordinator が interest へ view を渡し recipient IDs を得る（I/O なし）
6. coordinator が realtime_delivery へ recipient IDs + delta/event を渡す
7. realtime_delivery が接続別 queue へ投入
8. realtime_gateway が socket へ書き出す
```

**[REC]** `instance_runtime` は `interest` や `realtime_delivery` を直接呼ばない（`architecture.md` §3.1 DAG）。

### 3.5 失敗と整合性境界

**[REC]** 失敗時の挙動：

| 失敗箇所 | 影響 | 復旧方針 |
|---|---|---|
| 入力検証失敗 | 当該 command のみ拒否 | ErrorMessage を返す。状態変更なし |
| 正準状態更新中の panic | 当該インスタンスの状態破損可能性 | インスタンスを停止し、checkpoint から復元 |
| persistence port 失敗 | 永続化遅延 | 一時状態は継続。永続化は再試行 |
| interest 計算失敗 | 配信対象不明 | フォールバックとして広域配信またはエラー |
| delivery queue 飽和 | 当該接続の配信遅延 | latest-wins drop、slow consumer 切断 |
| 外部拡張配送失敗 | 拡張機能の遅延 | instance runtime へ伝播させない。再試行/DLQ |

**[SPEC]** 接続単位の不正 frame や遅い受信者が、インスタンス全体を停止させない隔離境界を置く（仕様 §18.1、`architecture.md` §6）。

**[REC]** panic は要求/connection/task 境界で観測し、正準状態の破損可能性がある runtime を無条件に継続しない（`architecture.md` §6）。

### 3.6 並行性と隔離

**[SPEC]** ネットワーク受信順を、そのまま複数 task から共有状態へ適用してはならない（仕様 §11）。同一インスタンス内の競合を一つの順序決定点へ集約する。

**[REC]** 隔離単位：

- **インスタンス間**: 独立した mailbox と状態。一方の panic が他方に伝播しない
- **接続間**: 独立した送信キュー。slow consumer が他接続を遅延させない
- **拡張間**: 独立した配送 worker。外部障害が runtime を停止させない

**[ADR]** 複数インスタンスを同一 Tokio runtime で実行するか、runtime を分離するかは負荷特性で決める。

## 4. 要 ADR 事項

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| SR-01 | 速度/境界超過時の初期方針 | 拒否（ErrorMessage） | クライアント補正より安全性優先。用途で clamp 拡張可能 |
| SR-02 | 境界検証 hook の API | trait + 登録制 | 用途固有検証をコアに埋め込まない |
| SR-03 | mailbox 容量と優先度実装 | 単一 bounded channel + 種別タグ | 初期は簡潔に。負荷試験で分離を検討 |
| SR-04 | 固定 tick vs 適応 tick | 固定 10 Hz | 初期は予測可能性優先。負荷試験で適応を検討 |
| SR-05 | チェックポイント間隔と保持 | 5 分間隔、3 世代保持 | 復旧目標と書込負荷の均衡 |
| SR-06 | panic 時のインスタンス復旧 | 停止 + 最新 checkpoint から再起動 | 状態破損リスクを許容しない |
| SR-07 | Tokio runtime 分離 | 初期は単一 runtime | 複雑性回避。負荷試験で分離を検討 |
