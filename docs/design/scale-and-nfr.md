# OrbiSync スケーラビリティと非機能要件設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §25（1000 人規模への設計）と §26（非機能要件）を、段階目標、tick/interest/delivery/DB の分離と容量モデル、NFR の測定方法・SLO 候補・failure isolation・security の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `SN-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| モジュール所有権、DAG、実行モデル、mailbox | `architecture.md` §3, §4、`state-and-runtime.md` §3 | 前提として参照。所有権と実行モデルを再定義しない |
| Interest Management のアルゴリズム、可視集合計算 | `mobile-resume-interest-backpressure.md`（MRIB 書）§7 | 前提として参照。本書は容量モデルへの影響を整理 |
| バックプレッシャー、接続別キュー、slow consumer | MRIB 書 §8 | 前提として参照。本書はスケール時の挙動を補う |
| 接続状態機械、Envelope、sequence/revision | `realtime-protocol-and-connection.md`（RP 書） | 前提として参照 |
| 一時/永続状態の分類、チェックポイント | `state-and-runtime.md` §1 | 前提として参照。DB 分離の根拠 |
| レイテンシ目標、セキュリティ要件 | `technology-decisions.md` §6.1, §6.2 | 前提として参照。本書は測定方法と SLO を具体化 |
| メトリクス、ログ、トレース | `observability-and-config.md`（本書と同時作成） | 相互参照。観測項目の定義は同書へ委ねる |
| 負荷試験ツール | `architecture.md` §3（`load_test_tooling`） | 前提として参照。ツールは Core 実行サーバー外 |
| 設定値（tick、queue 容量、interest 半径等） | `observability-and-config.md` §2 | 値の参照のみ。再定義しない |

### 0.2 負荷試験ツールの位置づけ

**設計前提** `load_test_tooling` は Core project が提供する責務だが、Core 実行サーバー外から公開 API/protocol を駆動する（`architecture.md` §3）。サーバー内部 module に依存しない。

**[REC]** 負荷試験ツールは公開 OpenAPI / `.proto` / 状態遷移のみを使用する。サーバーの内部メトリクスや DB へ直接アクセスしない。測定結果の収集は Prometheus scrape またログ出力で行う。

## 1. 段階目標

### 1.1 用語の分離

**[SPEC]** 仕様 §25.1 が分離する性能目標：

| 目標 | 意味 | 初期版での扱い |
|---|---|---|
| 1000 同時 TCP/WebSocket 接続 | プロセスが保持する socket 数 | Phase 2 で検証 |
| 1 サーバー全体で 1000 人 | 全インスタンス合計の参加者 | Phase 2 で検証 |
| 1 インスタンスに 1000 人 | 単一インスタンスの参加者 | Phase 3 の設計課題 |
| 各クライアントが 999 人分を受信 | 全員全員配信 | 目標としない（§1.3） |

**[SPEC]** 初期版で目標とするのは、段階的な負荷試験による拡張可能性であり、全員全員配信を保証するものではない（仕様 §25.1）。

### 1.2 Phase 定義

**[SPEC]** 仕様 §25.2 の段階目標：

#### Phase 1

| 項目 | 目標値 |
|---|---|
| 1 インスタンス | 50 人 |
| 1 プロセス合計 | 200 接続 |
| transform 入力 | 10 Hz |
| 耐久試験 | 24 時間 soak test |

**[REC]** Phase 1 は単一インスタンスの基本性能と、メモリリーク・goroutine/task リーク・DB 接続リークのないことを検証する。Interest Management は無効（全員可視）でよい。

#### Phase 2

| 項目 | 目標値 |
|---|---|
| 1 インスタンス | 200 人 |
| 1 プロセス合計 | 1000 接続 |
| Interest Management | 有効 |
| 近距離平均 entity | 50 以下 |

**[REC]** Phase 2 は Interest Management 有効時の配信量削減効果と、複数インスタンスの並行実行を検証する。近距離平均 50 entity 以下は、Interest の可視集合が配信量を支配することを意味する。

#### Phase 3

| 項目 | 目標値 |
|---|---|
| 1 論理ワールド | 1000 人 |
| 構成 | 複数 instance または空間 shard |
| routing | World Directory による routing |
| Runtime Node | 複数 |

**[REC]** Phase 3 は単一プロセスの限界を超える水平分割の設計課題である。初期版の実装対象ではなく、Phase 2 の計測結果に基づき ADR で設計を決定する。

**[ADR]** Phase 3 の空間分割方式、World Directory の routing プロトコル、複数 Runtime Node 間の状態整合は SN-01 で決める。仕様 §25.3 は将来構成の例示であり、確定ではない。

### 1.3 全員全員配信を目標としない根拠

**[SPEC]** 全員へ全状態を送らない（仕様 §25.4）。

**[REC]** 1000 人 × 999 人分の状態を各クライアントへ配信すると、1 秒あたり数百万メッセージの送信が必要になる。これは Interest Management の目的に反し、モバイル回線の帯域とバッテリーを圧迫する。初期版は「各クライアントが必要とする状態だけを配信する」ことを目標とし、全員全員配信は目標としない。

## 2. Tick と状態更新の容量モデル

### 2.1 Tick の設計

**設計前提** インスタンスは固定または適応 tick を使用する。推奨初期値は 10〜20 Hz（`state-and-runtime.md` §3.3、SR-04）。

**[REC]** Phase 1/2 では固定 10 Hz tick を使用する。tick の責務は `state-and-runtime.md` §3.3 が定める（集約された位置更新の確定、Interest 再計算トリガー、タイムアウト検知、チェックポイント判定）。

### 2.2 入力容量モデル

**[REC]** 1 インスタンスあたりの入力容量：

```text
参加者数: N
transform 入力頻度: f_in = 10 Hz（各クライアント）
1 tick あたりの入力数: N × f_in / tick_hz

Phase 1（N=50, tick=10 Hz）:
  50 × 10 / 10 = 50 入力/tick

Phase 2（N=200, tick=10 Hz）:
  200 × 10 / 10 = 200 入力/tick
```

**[REC]** mailbox は bounded channel であり、キュー上限を設定する（`state-and-runtime.md` §3.2）。1 tick で処理しきれない入力は次 tick へ持ち越す。持ち越しが継続する場合は tick 処理の最適化または tick_hz の調整が必要である。

**[ADR]** mailbox の容量上限値、優先度キューの実装方式は SR-03 / 負荷試験で決める（`state-and-runtime.md` §3.2）。

### 2.3 配信容量モデル

**[REC]** 1 インスタンスあたりの配信容量。Interest Management 有効時：

```text
参加者数: N
近距離平均可視 entity 数: V（Phase 2 目標: V ≤ 50）
tick 周波数: tick_hz = 10 Hz
1 tick あたりの StateDelta 配信数: N × V（最大）
1 秒あたりの配信メッセージ数: N × V × tick_hz

Phase 1（N=50, V=50 全員可視, tick=10 Hz）:
  50 × 50 × 10 = 25,000 msg/s（最大、Interest 無効時）

Phase 2（N=200, V=50, tick=10 Hz）:
  200 × 50 × 10 = 100,000 msg/s（最大）
```

**[REC]** 実際には latest-wins 集約（MRIB 書 §5.1）により、同一 entity の複数更新が 1 tick 内で 1 件に集約される。また、移動のない entity は StateDelta を送信しない。実配信量は上記の最大値より大幅に少ない。

**[REC]** バッチ送信（仕様 §25.4）により、1 tick 分の複数の StateDelta を 1 メッセージにまとめて送信してよい。これによりメッセージ数を削減する。

**[ADR]** バッチ送信の方式（1 tick 分の全 Delta を 1 メッセージ vs entity 単位）、バッチサイズの上限は SN-02 で決める。

### 2.4 メモリ容量モデル

**[REC]** 1 インスタンスあたりのメモリ見積もり：

| 項目 | 見積もり | 根拠 |
|---|---|---|
| entity 状態（Transform 等） | ~200 B/entity | position(12B) + rotation(16B) + velocity(12B) + revision(8B) + metadata |
| 空間索引（Uniform Grid） | ~64 B/entity | cell 登録 + 隣接リスト |
| 接続別送信キュー | ~4 KiB/接続（bounded） | MRIB 書 §8.2 の容量上限に依存 |
| 差分履歴バッファ | ~1 KiB/event × 保持数 | MRIB 書 §4.2、MRIB-03 |
| Interest 購読状態 | ~32 B × V × N | recipient × 可視 entity 集合 |

```text
Phase 2（N=200, V=50, entity 数 E=1000）:
  entity 状態: 1000 × 200 B = 200 KiB
  空間索引: 1000 × 64 B = 64 KiB
  送信キュー: 200 × 4 KiB = 800 KiB
  差分履歴: 100 event × 1 KiB = 100 KiB（MRIB-03 の保持数に依存）
  Interest 購読: 200 × 50 × 32 B = 320 KiB
  合計: ~1.5 MiB/インスタンス（概算）
```

**[REC]** 上記は概算であり、実際には Protobuf encode バッファ、Tokio task スタック、TCP バッファ等が加算される。負荷試験で実測し、モデルを補正する。

**[ADR]** 差分履歴バッファの保持量（revision 幅またはイベント数）は MRIB-03 で決める。メモリ上限はインスタンスの entity 数とイベント頻度に依存する。

## 3. Interest Management のスケール特性

### 3.1 配信量削減効果

**設計前提** Uniform Spatial Grid を標準とする（MRIB 書 §7.2）。各エンティティを現在位置のセルへ登録し、クライアントは自身のセルと周辺セルを購読する。

**[REC]** Interest Management の配信量削減は、可視 entity 数 V が参加者数 N より十分小さいときに効果的である：

```text
Interest 無効: 配信量 ∝ N × N（全員全員）
Interest 有効: 配信量 ∝ N × V（V ≪ N）

N=200, V=50 の場合:
  Interest 無効: 200 × 200 = 40,000 配信/tick
  Interest 有効: 200 × 50 = 10,000 配信/tick（75% 削減）
```

**[REC]** V はワールドの空間分布に依存する。全参加者が同一セルに密集する場合、V は N に近づき削減効果は低下する。この場合、配信レベル（Near/Mid/Far/Global、MRIB 書 §7.3）による頻度・情報量の制御が有効である。

### 3.2 空間索引の更新コスト

**設計前提** 索引の更新は coordinator 経由でのみ行う。`interest` は I/O を行わない純粋計算である（MRIB 書 §7.2、`architecture.md` §3）。

**[REC]** 空間索引の更新コスト：

| 操作 | コスト | 頻度 |
|---|---|---|
| entity のセル移動 | O(1)（旧セル削除 + 新セル挿入） | 位置更新ごと |
| 可視集合計算（recipient 1 人） | O(V)（周辺セルの entity 走査） | tick ごと（移動があった場合） |
| 可視集合計算（インスタンス全体） | O(N × V) | tick ごと |

**[REC]** Phase 2（N=200, V=50, tick=10 Hz）では、1 秒あたり 200 × 50 × 10 = 100,000 回の可視性判定が発生する。Uniform Grid の O(1) セル参照により、これは CPU バウンドだが管理可能な範囲である。

**[ADR]** セルサイズ、購読する周辺セルの範囲、索引の更新方式（incremental 更新 vs 再計算）は MRIB-05 で決める。セルサイズは V の目標値（Phase 2: 50 以下）と整合させる。

### 3.3 ヒステリシスのスケール影響

**設計前提** subscribe radius と unsubscribe radius を分け、境界付近の flapping を抑制する（MRIB 書 §7.5、MRIB-07）。

**[REC]** ヒステリシスは購読/解除の頻度を削減し、Interest 計算と DomainEvent（join/leave）の配信量を削減する。スケール時の効果：

- 境界付近で毎 tick 購読/解除が繰り返されると、毎 tick DomainEvent が発生し、reliable キューを圧迫する
- ヒステリシスにより、この flapping が抑制され、reliable キューの安定性が向上する

## 4. Delivery のスケール設計

### 4.1 接続別キューのスケール

**設計前提** 接続ごとに独立した bounded priority queue を持つ（MRIB 書 §8.2）。Control > Reliable > Latest-wins の優先度。

**[REC]** 接続数が増加すると、キューの総メモリ量と socket write の並行性が増加する：

```text
Phase 2（1000 接続）:
  キュー総メモリ: 1000 × 4 KiB = 4 MiB（概算）
  socket write task: 1000 並行（Tokio task）
```

**[REC]** socket write は接続ごとに独立した Tokio task で実行する。1 接続の write 遅延が他接続をブロックしない（MRIB 書 §8.1、`architecture.md` §6）。

### 4.2 バッチ送信

**[SPEC]** バッチ送信は仕様 §25.4 の性能原則である。

**[REC]** 1 tick 分の複数の StateDelta を 1 メッセージにまとめて送信する。バッチの構成：

| バッチ方式 | 利点 | 欠点 |
|---|---|---|
| 1 tick 分の全 Delta を 1 Envelope | メッセージ数最小 | メッセージサイズが大きくなる |
| entity 単位で個別 Envelope | 単純、latest-wins 集約と整合 | メッセージ数が多い |
| recipient 単位で 1 Envelope（複数 entity 含む） | メッセージ数とサイズの均衡 | encode/decode が複雑 |

**[ADR]** バッチ送信の方式とサイズ上限は SN-02 で決める。メッセージ上限（通常 16 KiB、TB 書 §5）を超えないこと。

### 4.3 Slow Consumer のスケール影響

**設計前提** slow consumer の検知と対応は MRIB 書 §8.3 が定める。

**[REC]** 接続数が増加すると、slow consumer の絶対数も増加する。Phase 2（1000 接続）で 1% が slow consumer の場合、10 接続が同時に latest-wins drop または切断の対象となる。

**[REC]** slow consumer の処理は接続ごとに独立しており、他接続やインスタンス runtime に影響しない（MRIB 書 §8.1）。ただし、同時切断数が急増すると、再接続の殺到（thundering herd）を引き起こす可能性がある。バックオフ + jitter（MRIB 書 §2.4）で緩和する。

## 5. DB 分離とホットパス

### 5.1 原則

**[SPEC]** DB をリアルタイム hot path へ置かない（仕様 §25.4）。

**設計前提** 一時状態は主にメモリで管理し、通常は毎更新 DB へ保存しない（`state-and-runtime.md` §1.1）。永続状態は PostgreSQL へ保存する（`state-and-runtime.md` §1.2）。

**[REC]** リアルタイム hot path（transform 入力 → 状態更新 → Interest 計算 → 配信）に DB 読み書きを含めない。具体的には：

| 操作 | DB アクセス | 根拠 |
|---|---|---|
| TransformInput の検証と適用 | なし | メモリ上の正準状態で完結 |
| Interest 可視集合計算 | なし | メモリ上の空間索引で完結 |
| StateDelta の配信 | なし | メモリ上のキューで完結 |
| EntityCommand（spawn/delete） | 永続 entity のみ非同期書込 | 一時 entity は DB 不要 |
| チェックポイント | 定期・非同期 | hot path 外 |
| 認証 token 検証 | 初回接続時のみ | 接続後はメモリ上のセッション |

### 5.2 永続化の非同期分離

**設計前提** 永続化の書き込みは `persistence` モジュールが所有する repository port を介して行う（`state-and-runtime.md` §1.2、`architecture.md` §3）。

**[REC]** 永続化は hot path から非同期に分離する：

```text
instance_runtime（hot path）
  │
  ├── 状態更新（メモリ、同期）
  ├── Interest 計算（メモリ、同期）
  ├── 配信キュー投入（メモリ、同期）
  │
  └── 永続化要求（port へ投入、非同期）
        │
        ▼
  persistence worker（hot path 外）
  ├── DB 書込
  ├── outbox 書込
  └── チェックポイント
```

**[REC]** persistence worker の失敗は hot path へ伝播しない（`architecture.md` §6、`state-and-runtime.md` §3.5）。一時状態は継続し、永続化は再試行する。

### 5.3 DB 接続プール

**設計前提** PostgreSQL を使用する（TD-06）。connection pool の設定は `observability-and-config.md` §2 の設定例に従う。

**[REC]** DB 接続プールは hot path と共有しない。persistence worker 専用のプールを使用する。これにより、DB 遅延がリアルタイム処理をブロックしない。

**[REC]** DB 停止時に ready=false を返し、プロセス生存のみを live で判定する（仕様 §26.1）。DB 停止中でもリアルタイム一時状態の処理は継続してよい。

**[ADR]** DB 接続プールのサイズ、タイムアウト、persistence worker の並行数は SN-03 で決める。設定例の `max_connections = 20` は初期値であり、負荷試験で調整する。

### 5.4 チェックポイントのスケール

**設計前提** チェックポイントは `instance_runtime` が正準状態のスナップショットを生成し、`persistence` port へ渡す（`state-and-runtime.md` §1.3）。

**[REC]** チェックポイントの間隔と保持数は SR-05（推奨: 5 分間隔、3 世代保持）に従う。スケール時の考慮：

- チェックポイントの書込はインスタンスの tick を停止させない（非同期）
- 複数インスタンスのチェックポイントが同時に発生すると DB 書込が集中する。インスタンスごとにオフセットをずらしてよい
- チェックポイントのサイズは entity 数に比例する。Phase 2（entity 1000）で ~200 KiB（概算）

## 6. 水平分割（Phase 3）

### 6.1 将来構成

仕様 §25.3 は将来構成の候補例を示している。以下の topology は確定した具体構成ではなく、Phase 3 以降の設計検討の出発点である。

**[REC]** 仕様 §25.3 が例示する将来構成の候補：

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

**[SPEC]** 1 つの Instance を複数 Node へ空間分割する機能は、Phase 3 以降の独立設計課題とする（仕様 §25.3）。

**[REC]** Phase 3 の設計は Phase 2 の計測結果に基づき ADR で決定する。初期版では単一プロセスの最適化を優先し、分散の複雑性を導入しない。

**[ADR]** Gateway / World Directory / Runtime Node の分離基準、remote command protocol、ownership、failover は SN-01 で決める。`architecture.md` §8 の分離候補表と整合させる。

### 6.2 分離の観測条件

**設計前提** 分離は「将来可能」であり、予定ではない。境界越し呼び出し量、レイテンシ、障害モード、運用負担を計測し、単一プロセスの最適化では目標を満たせない場合に ADR を作成する（`architecture.md` §8）。

**[REC]** Phase 3 の分離を検討する観測条件：

| 分離候補 | 観測条件 |
|---|---|
| Gateway 分離 | 接続数と TLS 処理が runtime CPU を圧迫する |
| Runtime Node 分離 | 単一インスタンスの tick 処理が 1 コアで間に合わない |
| Persistence Worker 分離 | DB 書込が runtime latency を圧迫する |
| World Directory 分離 | 複数 Runtime Node 間の routing が必要になる |

## 7. 非機能要件

### 7.1 可用性

**[SPEC]** 仕様 §26.1 の初期目標：

| 項目 | 要件 |
|---|---|
| graceful shutdown | 単一ノード構成で graceful shutdown |
| 再起動後復元 | 再起動後に永続データ復元 |
| drain | active connection を一定時間 drain |
| DB 停止時 | ready=false |
| live 判定 | プロセス生存のみを live で判定 |

**[REC]** graceful shutdown の手順は `architecture.md` §4.3 が定める（新規受付停止 → 接続/command の drain → 永続化要求の flush → task 終了）。

**[ADR]** drain 期限の具体値は ARC-03 / `architecture.md` §4.3 で決める。推奨初期値は 30 秒。

**[REC]** SLO 候補（可用性）：

| 指標 | SLO 候補 | 測定方法 |
|---|---|---|
| 月間稼働率 | 99.5%（単一ノード、計画停止含む） | `up` metric の月間平均 |
| graceful shutdown 成功率 | 99% | shutdown イベントの成功/失敗ログ |
| 再起動後復元時間 | 30 秒以内（永続データ） | 起動完了までの `duration_ms` ログ |

**[ADR]** SLO の確定値と運用への適用は SN-04 で決める。上記は候補であり、負荷試験と運用実績で調整する。

### 7.2 レイテンシ

**[SPEC]** 仕様 §26.2 のアプリケーション処理目標（同一地域内、ネットワーク往復時間不含）：

| パーセンタイル | 目標 |
|---|---|
| P50 | 10 ms 以下 |
| P95 | 50 ms 以下 |
| P99 | 100 ms 以下 |

**[SPEC]** 負荷試験環境と測定条件を必ず併記する（仕様 §26.2）。

**[REC]** 測定方法：

| 測定対象 | 測定方法 | 計測点 |
|---|---|---|
| REST API レイテンシ | `http_request_duration_seconds` histogram | `http_api` inbound → outbound |
| WebSocket command レイテンシ | command 受信 → 状態更新完了 → 配信キュー投入 | `instance_runtime` mailbox 入口 → 出口 |
| 配信レイテンシ | 状態更新完了 → socket write 完了 | `realtime_delivery` queue 入口 → gateway write |
| tick 処理時間 | tick 開始 → tick 完了 | `instance_runtime` tick handler |
| Interest 計算時間 | 可視集合計算の開始 → 完了 | `interest` 純粋関数 |
| DB クエリレイテンシ | `db_query_duration_seconds` histogram | `persistence` adapter |

**[REC]** SLO 候補（レイテンシ）：

| 指標 | SLO 候補 | 測定条件 |
|---|---|---|
| REST P50 | ≤ 10 ms | Phase 2 負荷、同一地域 |
| REST P95 | ≤ 50 ms | Phase 2 負荷、同一地域 |
| REST P99 | ≤ 100 ms | Phase 2 負荷、同一地域 |
| tick 処理時間 P99 | ≤ 50 ms | Phase 2（200 人、Interest 有効） |
| Interest 計算 P99 | ≤ 5 ms | Phase 2（200 人、V ≤ 50） |
| 配信キュー滞留 P99 | ≤ 20 ms | Phase 2（1000 接続） |

**[ADR]** SLO の確定値は SN-04 で決める。ネットワーク往復時間、クライアント処理時間、モバイル回線の遅延は SLO に含めない。

### 7.3 セキュリティ

**[SPEC]** 仕様 §26.3 の確定要件：

| 項目 | 要件 |
|---|---|
| TLS | 必須。平文 WebSocket を本番で禁止 |
| secrets | リポジトリへ保存しない |
| パスワード | ハッシュ保存（Argon2id） |
| rate limiting | 接続/ユーザー/IP/インスタンス単位 |
| request size limit | REST body / WebSocket message |
| SQL injection 対策 | パラメータ化クエリ |
| 監査ログ | 全管理操作 |
| 依存脆弱性スキャン | CI で実施 |
| 最小権限コンテナ | non-root 実行 |
| CSRF 対策 | Cookie 認証時 |
| CORS | デフォルト拒否 |
| origin 検証 | WebSocket upgrade 時 |

**設計前提** セキュリティの技術選定と制約は `technology-decisions.md` §6.1、`auth-authorization.md`、`transport-boundaries.md` §4 が定める。

**[REC]** セキュリティの測定方法：

| 項目 | 測定方法 |
|---|---|
| TLS 必須 | CI で平文接続の拒否をテスト |
| rate limit 動作 | 負荷試験で閾値超過時の拒否/切断を検証 |
| request size limit | 上限超過メッセージの拒否をテスト |
| SQL injection | パラメータ化クエリの compile-time check（SQLx） |
| 依存脆弱性 | `cargo deny` / `cargo audit` の CI 実行 |
| コンテナ権限 | OCI image の USER 指令と runtime 検証 |

**[REC]** SLO 候補（セキュリティ）：

| 指標 | SLO 候補 | 測定方法 |
|---|---|---|
| 認証失敗率 | 監視対象（SLO ではなく alerting） | `auth_login_failures_total` |
| rate limit 拒否率 | 監視対象 | `rate_limit_rejected_total` |
| 脆弱性修正 SLA | Critical: 7 日、High: 30 日 | 依存スキャンの検出日 → 修正日 |

**[ADR]** 脆弱性修正 SLA の確定値と運用プロセスは SN-05 で決める。

### 7.4 プライバシー

**[SPEC]** 仕様 §26.4 の確定要件：

| 項目 | 要件 |
|---|---|
| 位置履歴 | デフォルトで永続保存しない |
| ログ | パスワード、token、完全な payload を出さない |
| IP 保存期間 | 設定可能 |
| 監査ログ保持期間 | 設定可能 |
| ユーザーデータ | export/delete 方針を文書化 |

**設計前提** ログの機密情報除去は `technology-decisions.md` TD-08、`observability-and-config.md` §3 が定める。

**[REC]** 位置履歴は一時状態としてメモリ上のみで管理し、チェックポイントにも含めない（`state-and-runtime.md` §1.3）。永続化が必要な場合は利用側が拡張機構で実装する。

### 7.5 保守性

**[SPEC]** 仕様 §26.5 の確定要件：

- 循環依存禁止
- 1 crate の責務を明確にする
- public API を最小化
- domain error を安定化
- ADR を残す
- migration と protocol 変更をレビュー必須にする
- 自動生成コードと手書きコードを分離する

**設計前提** 循環依存禁止と依存方向は `architecture.md` §2.1 が強制する。

**[REC]** 保守性の測定方法：

| 項目 | 測定方法 |
|---|---|
| 循環依存 | CI で依存グラフの循環を検査（`architecture.md` §9） |
| public API 最小化 | `cargo doc` の public item 数を目視レビュー |
| domain error 安定性 | error 型の変更を breaking change として管理 |
| ADR 残存 | ADR ディレクトリの存在と更新を PR テンプレートで確認 |

## 8. Failure Isolation

### 8.1 隔離の原則

**設計前提** 接続単位の不正 frame や遅い受信者が、インスタンス全体を停止させない隔離境界を置く（`architecture.md` §6、MRIB 書 §8.1）。

**[REC]** 隔離の階層：

| 隔離単位 | 保護対象 | 伝播しない障害 |
|---|---|---|
| 接続 | 同一インスタンスの他接続 | slow consumer、不正 frame、panic |
| インスタンス | 同一プロセスの他インスタンス | tick 処理の panic、mailbox 飽和 |
| persistence worker | リアルタイム hot path | DB 遅延、DB 停止 |
| 拡張配送 | instance runtime | 外部サービスの遅延・停止 |
| telemetry exporter | 全処理 | exporter の遅延・停止 |

### 8.2 障害モードと挙動

**[REC]** 主要な障害モード：

| 障害 | 影響範囲 | 挙動 | 復旧 |
|---|---|---|---|
| 1 接続の panic | 当該接続のみ | 接続を切断。他接続は継続 | クライアントの再接続 |
| 1 インスタンスの tick panic | 当該インスタンスのみ | インスタンスを停止。checkpoint から復元（SR-06） | 自動再起動 |
| DB 停止 | 永続化のみ | ready=false。一時状態は継続 | DB 復旧後に再接続 |
| 外部拡張の停止 | 拡張機能のみ | 配送を再試行。DLQ へ移動 | 拡張復旧後に再配送 |
| telemetry exporter の停止 | 観測のみ | ログ/metrics をバッファリングまたは drop | exporter 復旧後に再開 |
| 全接続の同時切断 | プロセス全体 | graceful shutdown の手順で drain | 再起動 |

**[REC]** panic は要求/connection/task 境界で観測し、正準状態の破損可能性がある runtime を無条件に継続しない（`architecture.md` §6、`state-and-runtime.md` §3.5）。

### 8.3 Load Shedding

**設計前提** インスタンス単位・グローバルの制限はインスタンス保護が目的であり、制限超過を理由に無関係な接続を切断しない（MRIB 書 §8.4）。

**[REC]** load shedding の優先度：

1. 新規接続の拒否（rate limit）
2. 新規入室の拒否（定員超過）
3. 高コストイベントの拒否（同時実行制限）
4. latest-wins 更新の drop（通常動作）
5. slow consumer の切断（最終手段）

**[REC]** 既存の健全な接続は維持する。shedding は新規/高コスト入力の拒否で行う。

## 9. 負荷試験シナリオ

### 9.1 試験環境

**[REC]** 負荷試験は Core 実行サーバー外から `load_test_tooling` で駆動する（`architecture.md` §3）。試験環境の構成：

```text
負荷試験クライアント（load_test_tooling）
  │
  │ 公開 API / protocol（WSS + HTTPS）
  ▼
OrbiSync サーバー（被测対象）
  │
  ├── PostgreSQL
  └── Prometheus（metrics scrape）
```

**[REC]** 試験環境と測定条件を必ず記録する（仕様 §26.2）。記録項目：

- ハードウェア（CPU、メモリ、ディスク、ネットワーク）
- OS とカーネルバージョン
- OrbiSync のビルド設定（release/debug、最適化レベル）
- PostgreSQL のバージョンと設定
- 負荷試験クライアントの台数と配置
- ネットワークトポロジー（同一ホスト / 同一リージョン / 跨リージョン）

### 9.2 Phase 1 シナリオ

| シナリオ | 手順 | 合格基準 |
|---|---|---|
| P1-01 基本接続 | 50 クライアントが接続・入室・退室を繰り返す | 全クライアントが正常に入室・退室できる |
| P1-02 transform 負荷 | 50 クライアントが 10 Hz で transform を送信 | P99 tick 処理時間 ≤ 50 ms |
| P1-03 soak test | 50 クライアントで 24 時間連続動作 | メモリリークなし（RSS 増加 ≤ 5%）、task リークなし、DB 接続リークなし |
| P1-04 再接続 | 50 クライアントのうち 10% をランダムに切断 | 全クライアントがバックオフ後に再接続・resume 成功 |
| P1-05 エラー耐性 | 不正メッセージ（サイズ超過、不正 Protobuf）を送信 | 当該接続のみ切断。他接続は影響なし |

### 9.3 Phase 2 シナリオ

| シナリオ | 手順 | 合格基準 |
|---|---|---|
| P2-01 Interest 有効 | 200 クライアントが 1 インスタンスに入室。Interest 有効 | 近距離平均可視 entity ≤ 50。配信量が Interest 無効時より 50% 以上削減 |
| P2-02 1000 接続 | 1000 クライアントが複数インスタンスに分散入室 | 全クライアントが正常に動作。P99 REST レイテンシ ≤ 100 ms |
| P2-03 slow consumer | 1000 接続のうち 10 接続を意図的に遅延 | slow consumer のみ latest-wins drop / 切断。他接続の配信レイテンシに影響なし |
| P2-04 DB 停止 | E-4 chaos test（`tests/integration/tests/chaos_e4.rs`）へ統合 | 詳細な注入手順と判定は `docs/design/test-and-ci.md` §2.8 を参照 |
| P2-05 インスタンス panic | E-4 chaos test（`crates/orbisync-world-runtime/src/registry.rs`）へ統合 | 詳細な注入手順と判定は `docs/design/test-and-ci.md` §2.8 を参照 |
| P2-06 同時再接続 | 1000 クライアントを同時に切断 | バックオフ + jitter で再接続が分散。thundering herd がない |
| P2-07 レイテンシ測定 | Phase 2 負荷下でレイテンシを測定 | P50 ≤ 10 ms、P95 ≤ 50 ms、P99 ≤ 100 ms（アプリケーション処理、ネットワーク不含） |

### 9.4 耐久試験

**[REC]** 各 Phase で 24 時間 soak test を実施する。監視項目：

| 項目 | 判定基準 |
|---|---|
| RSS メモリ | 増加率が 5% 以内（24 時間） |
| Tokio task 数 | 増加率が 5% 以内 |
| DB 接続数 | プール上限を超えない |
| GC / アロケーション | 異常な増加なし（Rust では GC なしだが、drop リークを検出） |
| エラーレート | 安定（増加傾向なし） |
| tick 処理時間 | 安定（増加傾向なし） |

## 10. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 10.1 スケール

1. Phase 1: 50 クライアントが 10 Hz で transform を送信し、全クライアントが StateDelta を受信する。tick 処理時間 P99 ≤ 50 ms。
2. Phase 2: 200 クライアントが Interest 有効で入室し、各クライアントの可視 entity 数が 50 以下である。
3. Phase 2: 1000 接続が複数インスタンスに分散し、全接続が正常に動作する。

### 10.2 DB 分離

4. TransformInput の処理中に DB 読み書きが発生しない（計装またはトレースで検証）。
5. DB 停止中にリアルタイム一時状態の処理が継続する。ready=false が返る。
6. チェックポイントは tick を停止させずに非同期で完了する。

### 10.3 Failure Isolation

7. 1 接続の slow consumer が、同一インスタンスの他接続の配信レイテンシに影響しない。
8. 1 インスタンスの panic が、同一プロセスの他インスタンスに影響しない。
9. persistence worker の失敗が、リアルタイム hot path へ伝播しない。

### 10.4 NFR

10. graceful shutdown 時に active connection が drain され、永続化要求が flush される。
11. レイテンシ目標（P50/P95/P99）を負荷試験で測定し、環境と条件を併記する。
12. セキュリティ要件（TLS 必須、rate limit、size limit）のテストが CI で実行される。

## 11. 要 ADR 事項

本書が主担当となる判断を SN ID で管理する。他文書が正本の判断（SR-03、SR-04、SR-05、MRIB-03、MRIB-05、ARC-03 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| SN-01 | Phase 3 の空間分割方式、World Directory routing、複数 Runtime Node 間の状態整合 | Phase 2 計測後に ADR 作成。初期版では単一プロセス最適化を優先 | 仕様 §25.3 は将来構成の例示。`architecture.md` §8 の分離候補と整合 |
| SN-02 | バッチ送信の方式とサイズ上限 | recipient 単位で 1 Envelope（複数 entity 含む）、上限 16 KiB | メッセージ数とサイズの均衡。TB 書 §5 の上限を遵守 |
| SN-03 | DB 接続プールサイズ、persistence worker 並行数 | pool 20、worker 2〜4 | 設定例の初期値。負荷試験で調整 |
| SN-04 | SLO の確定値と運用への適用 | §7 の候補値を基に負荷試験で調整 | 仕様 §26.2 は目標値。SLO は運用実績で確定 |
| SN-05 | 脆弱性修正 SLA と運用プロセス | Critical 7 日、High 30 日 | 依存スキャンの CI 化と整合。具体は運用 ADR |
