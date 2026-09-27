# OrbiSync Client SDK 設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §24（Client SDK 仕様）を、SDK の接続・再接続・シリアライズ・状態適用・スレッディング・エラー契約の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `SDK-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| Envelope、メッセージ分類、sequence/revision、接続状態機械 | `realtime-protocol-and-connection.md`（RP 書） | 前提として参照。SDK 側の状態機械は RP 書 §5.1 のサーバー側と対に定義 |
| 自動再接続、Resume Token、再同期、heartbeat、バックオフ | `mobile-resume-interest-backpressure.md`（MRIB 書） | 前提として参照。SDK は同書のクライアント側契約を実装する |
| REST 認証 API、token 更新、エラー形式 | `rest-api-persistence.md`、`auth-authorization.md` | 前提として参照。SDK の REST 呼び出しは同書の公開契約に従う |
| Protobuf / JSON サブプロトコル、version negotiation | `transport-boundaries.md`（TB 書）、RP 書 §6 | 前提として参照 |
| latest-wins / reliable 分離、送信キュー | MRIB 書 §5、RP 書 §3 | 前提として参照。SDK 送信キューは同書のサーバー側キューと対に定義 |
| TypeScript SDK の技術選定 | `technology-decisions.md` TD-12 | 前提として参照 |
| SDK release、package 名、runtime support | ADR-011 | 本書は設計を提示し、確定は ADR-011 へ委ねる |
| Interest Management のクライアント側影響 | MRIB 書 §7 | 前提として参照。SDK は Interest の結果を受信するのみ |

### 0.2 SDK の位置づけ

**[SPEC]** SDK は仕様の正本ではない（仕様 §8.2）。`.proto` と OpenAPI が公開契約の正本であり、SDK はそれらの参照実装である。

**[SPEC]** 公式フロントエンドで実行できる操作は、すべて公開 API または公開プロトコルからも実行できなければならない（仕様 §3.2）。SDK 専用で公開面に存在しない操作を作ってはならない。

**設計前提** 初期の公式 SDK は TypeScript とする（TD-12）。本書の契約は言語非依存であり、他言語 SDK も同一契約に従う。

## 1. SDK の責務と非責務

### 1.1 責務

**[SPEC]** 仕様 §24.1 が定める SDK の責務：

| 責務 | 対応するサーバー側正本 | 本書の節 |
|---|---|---|
| REST 認証 | `auth-authorization.md`、`rest-api-persistence.md` | §2 |
| Access Token 更新 | `auth-authorization.md`、ADR-002 | §2.2 |
| WebSocket 接続 | RP 書 §5、TB 書 §1.2 | §3 |
| ClientHello | RP 書 §6 | §3.2 |
| 入退室 | RP 書 §5.1、MRIB 書 §2 | §3.3 |
| Protobuf encode/decode | TB 書 §2、TD-04 | §4 |
| 自動再接続 | MRIB 書 §2 | §5 |
| Resume | MRIB 書 §2〜§4 | §5 |
| Snapshot 適用 | RP 書 §7 | §6 |
| Delta 適用 | RP 書 §4.4、MRIB 書 §4 | §6 |
| sequence 管理 | RP 書 §4 | §4.2 |
| heartbeat | MRIB 書 §6 | §5.4 |
| イベント購読 | RP 書 §3.2（DomainEvent） | §6.3 |
| latest-wins 送信キュー | MRIB 書 §5、RP 書 §3.1 | §4.3 |
| エラー型 | TB 書 §4、RP 書 §5.3 | §7 |

### 1.2 非責務

**[SPEC]** 仕様 §24.2 が定める SDK の非責務：

- 3D 描画
- アバター
- アセットロード
- 入力操作
- 補間表示の具体実装
- 音声
- UI

**[REC]** SDK は状態の受信と送信、接続の管理、エラーの通知までを責務とする。受信した状態をどのように描画・補間・表示するかは利用側の責務である。

## 2. 認証とトークン管理

### 2.1 認証フロー

**設計前提** 認証は REST API で行う（`rest-api-persistence.md` §2、`auth-authorization.md`）。SDK は `POST /v1/auth/login` で認証し、access token と refresh token を取得する。

**[REC]** SDK の認証状態機械：

```text
Unauthenticated → Authenticating → Authenticated
                       │                  │
                       │ 失敗             │ token 期限切れ
                       ▼                  ▼
                  AuthFailed         Refreshing → Authenticated
                                        │
                                        │ refresh 失敗
                                        ▼
                                   Unauthenticated（再ログイン要求）
```

**[REC]** `login()` は Promise/async で結果を返す。認証失敗は例外またはエラー型で通知し、SDK の内部状態を `Unauthenticated` に戻す。

### 2.2 Access Token 更新

**[SPEC]** SDK は Access Token 更新の責務を持つ（仕様 §24.1）。

**設計前提** ADR-002によりAccess Tokenは15分の署名JWT、Refresh Tokenは30日有効のopaque tokenとし、rotation・reuse検知・session family失効を行う（`auth-authorization.md`）。

**[REC]** SDK は access token の期限切れを検知したら、refresh 機構（`POST /v1/auth/refresh`）で token を更新する。更新は以下のタイミングで試みる：

1. WebSocket 接続確立前（MRIB 書 §2.5）
2. REST 呼び出し時の 401 応答
3. 期限切れ前の事前更新（proactive refresh）

**[REC]** proactive refreshはAccess Token有効期間の80%経過時に試みる。これによりREST要求とRealtime接続ticket発行が期限切れで失敗する窓を狭める。

**[REC]** refresh 失敗（refresh token 失効、ネットワーク不可等）は、SDK 状態を `Unauthenticated` へ戻し、利用側へ再ログイン要求を通知する。この場合 resume は試行しない（MRIB 書 §2.5）。

**[ADR]** proactive refresh の閾値（80%）、refresh 呼び出しの並行制御（複数 401 が同時に発生した場合の dedup）、token 保存方法（メモリ vs secure storage）は SDK-01 で決める。

### 2.3 token の安全な取り扱い

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4）。

**[REC]** SDK は token をメモリ上にのみ保持し、ログ・console・エラーメッセージへ出力しない。エラー通知に token を含めない。

**[REC]** token の保存方法は利用側の環境に依存する。ブラウザではメモリ推奨、Node.js ではプロセスメモリ推奨。secure storage への永続化は利用側の判断とする。

## 3. WebSocket 接続

### 3.1 接続確立

**設計前提** WebSocket Secureを標準とする。browser互換性のためAccess Tokenをupgradeへ渡さず、`POST /v1/realtime/tickets`で取得した単一使用ticketをClientHelloで提示する（ADR-002、RP書 §6.1）。

**[REC]** 接続確立の手順：

1. Access Tokenの有効性を確認（期限切れなら§2.2のrefresh）
2. Bearer Access Tokenで`POST /v1/realtime/tickets`を呼び、60秒有効・単一使用ticketを取得
3. token/ticketをURLやheaderへ含めず、`Sec-WebSocket-Protocol`だけを指定して`wss://`へ接続
4. upgrade成功後5秒以内にticketを含むClientHelloを送信
5. ServerHelloを受信し、version/圧縮/機能を合意
6. 接続状態を`Ready`へ遷移

**[REC]** サブプロトコル名は TB 書 §2.3 / TB-03 に従う。SDK は Protobuf を本番形式として使用し、JSON debug mode は開発時のみ有効化する（TB 書 §2.2、TB-02）。

### 3.2 ClientHello

**設計前提** ClientHelloの内容はRP書 §6.1が定める（Realtime接続ticket、SDK名/version、protocol major/minor、client種別、対応圧縮・機能flags、任意のResume Token）。

**[REC]** SDK は ClientHello に以下の値を設定する：

| 項目 | SDK の設定 |
|---|---|
| SDK 名 | package 名（例: `@orbisync/client`） |
| SDK バージョン | package version（semver） |
| プロトコル major/minor | SDK が対応する最新 version |
| クライアント種別 | 利用側が指定（`desktop` / `mobile` / `server`）。未指定時は `desktop` |
| 対応圧縮方式 | SDK が実装する圧縮のリスト |
| 対応機能 flags | SDK が実装するオプション機能 |
| resume token | 再接続時のみ設定。初回接続では空 |

**[SPEC]** サーバーはクライアント種別や自己申告値を権限判定に使わない（仕様 §15.2、RP 書 §6.1）。

**[REC]** SDK もこれらを権限の根拠として利用しない。

### 3.3 入退室

**設計前提** 入室だけが`JoinInstance` wire payloadを持つ。`Leave`はwire payloadではなく、接続close、Kick、timeout等をApplication coordinatorが内部`InstanceCommand::Leave`へ変換するdomain操作である（RP書 §5.1）。入室と退室の成立点・補償はcoordinatorが調停する（`architecture.md` §5.2）。

**[REC]** SDK の入室フロー：

1. `JoinInstance` を送信（instance_id を指定）
2. `JoinAccepted` を受信
3. `Snapshot` を受信し、ローカル状態を初期化（§6.1）
4. 接続状態を `Active` へ遷移
5. 利用側へ `snapshot` イベントを発行

**[REC]** 入室拒否（権限不足、定員超過、インスタンス停止）は `ErrorMessage` で通知される。接続は維持され、SDK は `Ready` 状態に戻る（RP 書 §5.3、RP-04）。利用側は別のインスタンスへ再試行できる。

**[REC]** 公開SDKの`instance.leave()`は「現在のInstanceから退室する」意味を持つが、独自wire messageは送らない。SDKはRealtime接続をgraceful closeし、close完了またはtimeout後にローカルInstance状態を破棄して`Disconnected`へ遷移する。サーバーは接続closeを内部`InstanceCommand::Leave`へ変換し、Presence終了と`MemberLeft`を一度だけ成立させる。別Instanceへ参加するには新しい接続を確立して`join()`する。

**[REC]** `connection.disconnect()`は「Realtime transportを閉じる」公開APIである。入室中に呼べば結果として同じ内部leave処理を起動するが、domain上の意図を表す`instance.leave()`とはAPI意味が異なる。両者は冪等で、二重の`MemberLeft`を発行しない。

### 3.4 SDK 接続状態機械

**[REC]** SDK 側の接続状態は RP 書 §5.1 のサーバー側状態機械と対になる。MRIB 書 §2.3 のクライアント側状態を具体化する：

```text
Disconnected → Connecting → AwaitingHello → Ready → Joining → Active
     ▲              │                                    │        │
     │              │ 失敗                               │        │ 切断
     │              ▼                                    │        ▼
     │         Backoff ─────────────────────────────────►│   Reconnecting
     │              ▲                                    │        │
     │              │                                    │        │ resume 成功
     │              │                                    │        ▼
     │              │                                    │     Active
     │              │                                    │
     │              │                                    │ resume 拒否
     │              │                                    ▼
     │              └────────────────────────────── Rejoining → Active
     │
     └── 明示的 disconnect() / token 失効
```

| 状態 | 意味 | 許可される操作 |
|---|---|---|
| Disconnected | 未接続 | `connect()` |
| Connecting | TCP/TLS/WS upgrade 確立中 | —（待機） |
| AwaitingHello | WS 確立済み、ServerHello 待ち | —（待機） |
| Ready | 認証済み・未入室 | `join()`、`resume()`、`disconnect()` |
| Joining | JoinInstance 応答待ち | —（待機） |
| Active | 入室済み・状態交換中 | `sendTransform()`、`sendCommand()`、`leave()`、`disconnect()` |
| Backoff | 再接続待機中 | `disconnect()`（待機の中断） |
| Reconnecting | 再接続確立中（resume 試行予定） | —（待機） |
| Rejoining | resume 拒否後の再入室中 | —（待機） |

**[REC]** 状態遷移は SDK 内部で直列化する。並行な `connect()` / `disconnect()` / `join()` 呼び出しは、現在の状態に応じて拒否またはキューイングする。

公開 TypeScript SDK は内部状態をそのまま固定せず、利用側向けに `connected` / `reconnecting` / `resyncing` / `offline` / `closed` の5段階へ集約する。`getConnectionState()` は immutable snapshot を返し、`onConnectionStateChange()` は現在値を即時通知したうえで解除関数を返す。snapshotには再接続試行回数、最後のclose code/reason、heartbeat RTT、最終適用revisionを含める。resume tokenその他のcredentialは型にも実行時objectにも含めない。listener例外は捕捉し、内部状態遷移や他listenerを停止させない。

## 4. シリアライズと送信

### 4.1 Protobuf encode/decode

**設計前提** 本番形式は Protocol Buffers を標準とする（TB 書 §2.1、TD-04）。`.proto` が公開契約の正本である。

**[REC]** SDK は `.proto` から生成されたコードを使用し、手書きのシリアライズを行わない。生成コードと手書きの接続状態機械は分離する（TD-12）。

**[REC]** encode/decode のエラー（不正な Protobuf、unknown field）は SDK のエラー型（§7）として利用側へ通知する。unknown field は silently drop せず、デバッグログで観測する（TB 書 §3.4）。

### 4.2 sequence 管理

**設計前提** 接続ごとに単調増加する sequence を付与する。送信方向ごとに独立したカウンタを持つ（RP 書 §4.2、RP-01）。

**[REC]** SDK の sequence 管理：

- **送信 sequence**: SDK が送信する各 Envelope に単調増加番号を付与する。接続確立時に 0 から開始する
- **受信 sequence**: サーバーからの各 Envelope の sequence を検証する。期待値より小さい（重複）は drop、gap はエラーログ + 再接続の引き金とする
- **再接続時**: 新しい `RealtimeConnectionId` で sequence を再開する（RP 書 §4.2）。旧接続の sequence を引き継がない

**[REC]** sequence の gap 検知は再接続の十分条件ではない。一時的なネットワーク遅延で gap が生じる場合があるため、連続 gap または heartbeat タイムアウトと組み合わせて判定する。

### 4.3 latest-wins 送信キュー

**設計前提** 位置更新は送信キュー上で同一エンティティの古い更新を上書きする（MRIB 書 §5.1、RP 書 §3.1）。

**[REC]** SDK は送信側に latest-wins 集約キューを持つ：

| キュー種別 | 対象 | 集約 | 飽和時動作 |
|---|---|---|---|
| latest-wins | TransformInput | 同一 entity の最新のみ | 古い更新を drop |
| reliable | EntityCommand、DomainEvent | なし | 送信側へエラー通知 |
| control | Heartbeat | なし | 優先送信 |

**[REC]** latest-wins キューの key は `(entity_id, component)` とする。サーバー側キュー（MRIB 書 §8.2）と同一の key 設計であり、送信側と受信側の両方で latest-wins が機能する。

**[REC]** reliable キューは bounded とする。上限超過時は利用側へエラーを通知し、メッセージを drop しない（サーバーへの送信を待機）。

**[SDK-02 決定済み]** 送信キューの容量上限、reliable キューの飽和時挙動、送信バッチの間隔は SDK-02 で決定済み。`bufferedAmount` 閾値 256 KiB / flush 50 ms / latest-wins 1,024 key（超過時は最古 key を drop）/ reliable 256 件（超過時はエラー通知、drop しない）/ Heartbeat は詰まり中も優先送信。詳細は `docs/adr/SDK-02-send-queue.md`。

### 4.4 送信の直列化

**[REC]** SDK の socket への書き込みは単一の直列化された経路に集約する。並行な `sendTransform()` / `sendCommand()` 呼び出しは内部キューへ投入され、単一の writer が順番に socket へ書き出す。

**[REC]** WebSocket のフレーム分割は SDK の透過的に行う。利用側はメッセージ単位の API のみを使用する。

## 5. 再接続と Resume

### 5.1 切断検知

**設計前提** 切断検知は heartbeat タイムアウトまたは socket close で行う（MRIB 書 §6.2）。

**[REC]** SDK は以下の切断を検知する：

| 検知方法 | 意味 |
|---|---|
| WebSocket `close` イベント | 正常切断またはサーバー側切断 |
| WebSocket `error` イベント | transport エラー |
| HeartbeatAck の連続未受信 | 半開き接続、サーバー応答なし |
| 送信失敗（socket write error） | 接続の破損 |

### 5.2 指数バックオフ + jitter

**設計前提** 公式 SDK は指数バックオフ + jitter による自動再接続を実装する（MRIB 書 §2.4、MRIB-01）。

**[REC]** 推奨初期式（full jitter、MRIB 書 §2.4）：

```text
base = 1 s
cap  = 30 s
attempt n（0 始まり）:
  sleep = random(0, min(cap, base * 2^n))
```

**[REC]** 最大試行回数に到達した場合は、再接続を停止し、利用側へ `connectionLost` イベントで通知する。利用側は明示的な `connect()` で再開できる。

**[ADR]** 最大試行回数、試行上限到達後の挙動（自動停止 vs 無限リトライ + 通知）は SDK-03 で決める。

### 5.3 Resume フロー

**設計前提** Resume のサーバー側状態機械、Resume Token の性質、再同期の分岐は MRIB 書 §2〜§4 が定める。

**[REC]** SDK の resume フロー：

1. 切断検知後、バックオフ待機
2. access token の有効性を確認（期限切れなら refresh、§2.2）
3. 新しいRealtime接続ticketをRESTで取得
4. 新しいWebSocket接続を確立
5. ClientHelloに接続ticketと、任意のバインドヒントとしてresume tokenを含めて送信
6. ServerHelloを受信
7. `ResumeSession`を送信（resume token + last_revision）
8. サーバー応答を待つ：
   - `ResumeAccepted` + 差分 replay → ローカル状態へ適用（§6.2）→ `Active`
   - `ResyncRequired` + Snapshot → ローカル状態を初期化（§6.1）→ `Active`
   - `ErrorMessage`（resume 拒否）→ `Ready` へ降格 → 通常の `join()` フロー

**[REC]** resume 拒否は接続レベルの失敗ではない。SDK は接続を維持したまま通常の入室フローへ移行する（MRIB 書 §2.2）。

**[REC]** resume tokenの処理は機械契約`contracts/realtime-resume-token-policy.json`を正本とする（RP書 §6.1）。SDKがClientHelloにもtokenを設定する場合はResumeSessionと同じ値を設定し、serverの契約結果を通常のresume応答として扱う。

**[REC]** `last_revision` は SDK が最後に受信・適用した instance revision である。Snapshot 適用時（§6.1）と Delta 適用時（§6.2）に更新する。

### 5.4 Heartbeat

**設計前提** application-level heartbeat を使用する。WebSocket ping/pong だけに依存しない（MRIB 書 §6.1）。

**[REC]** SDK の heartbeat 実装：

- 一定間隔で `Heartbeat` メッセージを送信する
- `HeartbeatAck` を受信し、RTT を測定する
- 連続して `HeartbeatAck` を受信しなければ切断扱いにする
- RTT 測定値は `rtt` イベントで利用側へ公開してよい（観測用）

**設計前提** heartbeat 間隔・timeout の推奨初期値は MRIB 書 §6.3 / MRIB-04 が定める（間隔 15〜30 秒、timeout 45〜90 秒）。

**[REC]** SDK は heartbeat 間隔をサーバーの ServerHello または設定で上書き可能とする。モバイル環境では間隔を長くしてバッテリー消費を抑えることができる。

## 6. 状態適用

### 6.1 Snapshot 適用

**設計前提** 入室時に Interest Management 適用後の状態を送る（RP 書 §7.1）。1000 人全員分を無条件に送らない。

**[REC]** SDK の Snapshot 適用：

1. Snapshot メッセージを受信する（分割 chunk の場合は全 chunk を受信・再組み立て）
2. ローカルのエンティティ状態を全クリアする
3. Snapshot のエンティティ・プレゼンス情報をローカル状態へ適用する
4. `instance_revision` を Snapshot の revision で更新する
5. 利用側へ `snapshot` イベントを発行する

**[REC]** Snapshot の分割 chunk（RP 書 §7.2）は、同一の論理メッセージ ID で再組み立てる。chunk の欠落は再接続または ResyncRequired で回復する。

**[REC]** Snapshot 適用は原子的に行う。適用中に受信した StateDelta は、Snapshot の revision より新しい場合のみ適用する。古い Delta は破棄する。

### 6.2 Delta 適用

**設計前提** StateDelta は latest-wins クラス、EntityCommand / DomainEvent は reliable クラス（RP 書 §3.2）。

**[REC]** SDK の Delta 適用：

| メッセージ | 適用方法 |
|---|---|
| StateDelta | 対象 entity の component を最新値で上書き。revision を更新 |
| EntityCommand（spawn） | ローカル状態へ entity を追加 |
| EntityCommand（update） | 対象 entity の component を更新。revision を検証 |
| EntityCommand（delete） | ローカル状態から entity を削除 |
| DomainEvent | 利用側へイベントを発行。ローカル状態は変更しない（イベント種別による） |

**[REC]** StateDelta の適用は latest-wins である。古い revision の Delta が遅延して到着した場合、現在の revision より古ければ破棄する。

**[REC]** EntityCommand の revision 不一致は、サーバーが ErrorMessage / ResyncRequired で応答する（RP 書 §4.4）。SDK は ResyncRequired を受信したら、サーバーへ Snapshot を要求するか、再接続フロー（§5.3）へ移行する。

### 6.3 イベント購読

**[SPEC]** SDK はイベント購読の責務を持つ（仕様 §24.1）。

**[REC]** SDK は利用側へ以下のイベントを公開する：

| イベント名 | 発火タイミング | ペイロード |
|---|---|---|
| `snapshot` | Snapshot 適用完了 | 全エンティティ・プレゼンスの状態 |
| `entityUpdated` | StateDelta / EntityCommand 適用後 | 変更された entity の差分 |
| `entitySpawned` | EntityCommand（spawn）適用後 | 新規 entity の状態 |
| `entityDeleted` | EntityCommand（delete）適用後 | 削除された entity の ID |
| `domainEvent` | DomainEvent 受信後 | イベント種別とペイロード |
| `connectionStateChanged` | 接続状態の遷移 | 新旧状態 |
| `error` | エラー発生 | エラー型（§7） |
| `rtt` | HeartbeatAck 受信後 | RTT 測定値（ms） |

**[REC]** イベントの発行は SDK の内部状態更新の後に行う。利用側がイベントハンドラ内で SDK の状態を参照した場合、更新後の状態が見える。

**[REC]** イベントハンドラ内の例外は SDK の内部状態を破損させてはならない。ハンドラの例外はキャッチし、`error` イベントで通知する。

## 7. エラー契約

### 7.1 エラーの分類

**設計前提** エラーは Transport 層と Domain 層に分類される（RP 書 §5.2、TB 書 §4）。

**[REC]** SDK のエラー型は次の階層とする：

```text
OrbiSyncError
├── ConnectionError          # transport / 接続レベル
│   ├── UpgradeFailed        # WS upgrade 拒否
│   ├── HelloFailed          # ClientHello / ServerHello 失敗
│   ├── HeartbeatTimeout     # heartbeat 連続失敗
│   ├── SocketError          # socket I/O エラー
│   └── ProtocolError        # decode 失敗、version 不一致
├── AuthError                # 認証レベル
│   ├── LoginFailed          # ログイン失敗
│   ├── TokenExpired         # access token 期限切れ
│   ├── RefreshFailed        # refresh 失敗
│   └── Unauthorized         # 認証拒否
├── InstanceError            # インスタンスレベル
│   ├── JoinRejected         # 入室拒否（権限、定員、停止）
│   ├── Kicked               # kick による退室
│   ├── InstanceStopped      # インスタンス停止
│   └── ResyncRequired       # 再同期要求
├── CommandError             # コマンドレベル
│   ├── ValidationFailed     # 入力検証失敗
│   ├── OwnershipViolation   # 所有権違反
│   ├── RevisionConflict     # revision 不一致
│   └── RateLimited          # rate limit 超過
└── InternalError            # SDK 内部エラー
    ├── QueueOverflow        # 送信キュー飽和
    └── InvalidState         # 不正な状態遷移
```

### 7.2 エラーの通知方法

**[REC]** エラーは次の方法で利用側へ通知する：

| エラー種別 | 通知方法 | SDK の状態遷移 |
|---|---|---|
| ConnectionError | `error` イベント + `connectionStateChanged` | 再接続フローへ（§5） |
| AuthError（LoginFailed） | `login()` の Promise 拒否 | `Unauthenticated` |
| AuthError（TokenExpired / RefreshFailed） | `error` イベント + `connectionStateChanged` | `Unauthenticated`（再ログイン要求） |
| InstanceError（JoinRejected） | `join()` の Promise 拒否 | `Ready`（接続維持） |
| InstanceError（Kicked / InstanceStopped） | `error` イベント + `connectionStateChanged` | `Ready`（接続維持）または `Disconnected` |
| InstanceError（ResyncRequired） | 内部的に再同期フローへ移行 | `Active`（自己遷移して再同期） |
| CommandError | `sendCommand()` の Promise 拒否または `error` イベント | `Active`（接続維持） |
| InternalError | `error` イベント | 状態による |

**[SPEC]** Transport エラーは公開エラーコードへ変換し、内部 DB/ライブラリエラーを露出しない（仕様 §21.8、TB 書 §4.3）。

**[REC]** SDK はサーバーの ErrorMessage から機械可読エラーコードを抽出し、SDK のエラー型へマッピングする。サーバーの `message` フィールドは人間向けであり、SDK のエラー型判別には使用しない。

### 7.3 エラーの機密性

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4）。

**[REC]** SDK のエラーオブジェクトに token、パスワード、内部スタックトレースを含めない。エラーメッセージは機密情報を含まない範囲で人間向け説明を提供してよい。

## 8. スレッディングモデル

### 8.1 並行性の原則

**[REC]** SDK は利用側のスレッド/イベントループをブロックしない。すべての I/O と状態更新は非同期で行う。

**[REC]** SDK の内部状態（接続状態、ローカルエンティティ状態、sequence、revision）へのアクセスは直列化する。並行な API 呼び出しが内部状態を競合させてはならない。

### 8.2 TypeScript / JavaScript の場合

**設計前提** 初期の公式 SDK は TypeScript とする（TD-12）。

**[REC]** JavaScript の単一スレッドモデルに従い、SDK の状態更新はイベントループ上で直列化する。WebSocket の `onmessage` と利用側の API 呼び出しが同一イベントループで処理されるため、明示的なロックは不要である。

**[REC]** ただし、非同期操作の完了順序は保証されない。`join()` の完了前に `sendTransform()` を呼び出した場合、SDK は状態に応じて拒否またはキューイングする。

**[REC]** Worker スレッドや React Native の JSI ブリッジをまたぐ利用は、利用側が同期を管理する。SDK は単一のイベントループ上での使用を前提とする。

### 8.3 他言語 SDK の場合

**[REC]** マルチスレッド言語（Rust、C#、Java 等）の SDK では、内部状態を Mutex または actor で保護する。公開 API は thread-safe とする。

**[REC]** 状態更新の通知（イベント）は、利用側が指定したディスパッチャー/スケジューラー上で発行する。SDK 内部の I/O スレッドを直接利用側へ公開しない。

## 9. SDK 互換性

### 9.1 version の分離

**[SPEC]** SDK version と protocol version を分ける（仕様 §24.4）。

**[REC]** SDK の semver は SDK 自体の API 互換性を表す。protocol version は `.proto` の互換性を表す。SDK は対応する protocol major/minor を ClientHello で宣言する。

### 9.2 互換性方針

**[SPEC]** 旧 SDK を即時切断しない（仕様 §24.4）。

**[SPEC]** サポート対象 protocol major を公開する（仕様 §24.4）。

**[SPEC]** protocol major 変更時は移行ガイドを提供する（仕様 §24.4）。

**[REC]** SDK の非互換変更（breaking change）は semver major で発行する。protocol の非互換変更は `.proto` の major version で管理し、SDK は新旧 major の同時対応期間を設けてよい。

**[ADR]** SDK の最小サポート protocol version、非互換変更の非推奨期間、EOL ポリシーは SDK-04 で決める。

## 10. API 例

仕様 §24.3 の API 例は参考であり、確定した API ではない。以下は本書の設計に基づく推奨 API 例である。

**[REC]** 本書の設計に基づく推奨 API 例：

```ts
import { OrbiSyncClient, createUuidV7 } from "@orbisync/client";

const client = new OrbiSyncClient({
  baseUrl: "https://meta.example.org",
});

// 認証
await client.auth.login({
  loginId: "user001",
  password: "temporary-password",
});

// 接続と入室
const connection = await client.connect();
const instanceId = "0192d43d-a18a-7fed-8123-0123456789ab"; // REST API????UUIDv7
const instance = await connection.join(instanceId);
await instance.ready();

// 状態の購読
instance.on("snapshot", (snapshot) => {
  // 利用側が任意の描画状態へ変換する
});

instance.on("entityUpdated", (update) => {
  // Unity、Godot、Three.js 等へ反映する
});

// 入力の送信
const entityId = createUuidV7();
instance.sendEntityCommand({
  entityId,
  operation: "spawn",
  args: { kind: "object", visibility: "public" },
});
instance.sendTransform({
  entityId,
  position: { x: 1, y: 0, z: 3 },
  rotation: { x: 0, y: 0, z: 0, w: 1 },
});

// 退室: graceful closeを行うため、追加のdisconnect()は不要
await instance.leave();
```

**[REC]** API の命名、構造、オプションは ADR-011 と実装で確定する。本書の例は契約の意図を示すものであり、シグネチャの確定ではない。

## 11. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。fake server と決定論的 clock を用いる。

### 11.1 認証とトークン

1. `login()` の成功後、SDK 状態は `Authenticated` になり、access token と refresh token をメモリに保持する。token はログ・console・エラーメッセージに含まれない。
2. Access Token期限切れ時にWebSocket接続を試みると、SDKは自動refresh後にRealtime接続ticketを取得して接続する。
3. refresh 失敗時、SDK 状態は `Unauthenticated` になり、`error` イベントで `RefreshFailed` を通知する。resume は試行しない。

### 11.2 接続と入室

4. WebSocket接続確立後5秒以内に、単一使用ticket、SDK名/version、protocol major/minor、client種別、対応圧縮、対応feature flags、任意のresume tokenを含むClientHelloを送信する。ServerHelloのnegotiated minor、圧縮、enabled featuresを適用後に`Ready`へ遷移し、同じticketの再利用は失敗する。
5. `join()`の成功後、Snapshotを受信・適用し、`Active`へ遷移する。`snapshot`イベントが発行される。
6. 入室拒否（ErrorMessage）時、SDK は `Ready` へ戻り、`join()` の Promise を拒否する。接続は維持される。

### 11.3 シリアライズと送信

7. 同一 entity の TransformInput を連続送信すると、送信キュー上で latest-wins 集約され、socket へは最新値のみが書き出される。
8. EntityCommand は reliable キューへ投入され、latest-wins の drop の影響を受けない。
9. 送信 sequence は接続ごとに単調増加し、再接続後に再開される。

### 11.4 再接続と Resume

10. 切断後、指数バックオフ + jitter の待機を経て再接続する。待機時間は `min(cap, base * 2^n)` の範囲内である。
11. 現Coreのresume成功はResumeAcceptedに続くSnapshotを検証・適用して復帰する。SDKは受信したヘッダだけで適用済みrevisionを進めない。
12. resume拒否時、SDKは`Ready`へ降格し、通常の`join()`フローで新規Snapshotを取得して`Active`へ復帰する。接続は切断されない。
13. HeartbeatAck の連続未受信後、SDK は切断扱いにして再接続を開始する。

### 11.5 状態適用

14. Snapshot 適用後、ローカル状態は Snapshot の内容と一致し、`instance_revision` が更新される。
15. StateDeltaのinstance境界をcomponentごとに比較する。別componentの新しいcommandを観測しても、古いDeltaが持つ未反映componentは統合する。Snapshot floorとdelete/spawn境界以下の通知は除外する。
16. 確定EntityCommand（spawn/update/delete）のinstance_revisionを使って状態を反映する。spawn/updateのexpected_revisionはentity revision、deleteはinstance revisionという既存意味を保つ。spawnのcustom引数echoは保存済み状態と扱わない。DomainEventは通知のみ。
17. Snapshot の分割 chunk は再組み立て後に適用される。chunk 欠落時は再接続で回復する。

### 11.6 エラーと隔離

18. サーバーの ErrorMessage から機械可読エラーコードを抽出し、SDK のエラー型へ正しくマッピングする。エラーオブジェクトに token やパスワードを含まない。
19. イベントハンドラ内の例外は SDK の内部状態を破損させず、`error` イベントで通知される。
20. 並行な API 呼び出し（`join()` 完了前の `sendTransform()` 等）は、状態に応じて拒否またはキューイングされ、内部状態の競合を起こさない。

## 12. 要 ADR 事項

本書が主担当となる判断を SDK ID で管理する。他文書が正本の判断（ADR-002、ADR-004、ADR-011、MRIB-01、MRIB-04 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| SDK-01 | token 保存方法、proactive refresh 閾値、refresh 並行制御 | メモリ保持、80% 閾値、refresh 呼び出しの dedup | セキュリティと UX の均衡。secure storage は利用側判断 |
| SDK-02 | 送信キューの容量上限、reliable 飽和時挙動、送信バッチ間隔 | bounded queue、飽和時はエラー通知、バッチは 1 tick 分 | サーバー側キュー（MRIB 書 §8.2）と整合。具体値は負荷試験で調整 |
| SDK-03 | 最大再接続試行回数、上限到達後の挙動 | 上限あり（例: 20 回）、到達後は自動停止 + 通知 | 無限リトライによるサーバー負荷の回避。具体値は MRIB-01 と整合 |
| SDK-04 | 最小サポート protocol version、非互換変更の非推奨期間、EOL ポリシー | 現行 major の 1 世代前までサポート、非推奨期間 6 か月 | 仕様 §24.4 の互換性方針。具体は ADR-011 と整合 |
