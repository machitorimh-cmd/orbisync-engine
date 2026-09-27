# OrbiSync モバイル再接続・Interest・バックプレッシャー設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §16（モバイル回線対応）、§17（Interest Management）、§18（バックプレッシャー）を、再接続状態機械、Resume Token、再同期、latest-wins/reliable 分離、heartbeat、Interest の純粋可視集合計算、接続単位キューと slow consumer 保護の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書（`architecture.md`、`technology-decisions.md` 等）で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

モジュール所有権と DAG は `architecture.md` §3 に従う。本書は所有モジュールを再定義しない。本書が主担当となる要 ADR 判断は `MRIB-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| Envelope、メッセージ分類、sequence/revision、接続状態機械の骨格 | `realtime-protocol-and-connection.md`（以下RP書） | 前提として参照。serverの`resuming`とバックプレッシャー詳細値を本書で具体化 |
| モジュール所有権、DAG、再接続フロー、単一 socket writer | `architecture.md` §3, §3.1, §5.4 | 前提として参照 |
| Auth/Realtime/Presence のライフサイクル分離、resume grace | `domain-model.md` §3.6, DM-07 | 前提として参照。所有権と grace 推奨値を再定義しない |
| mailbox、tick、失敗隔離、入力検証境界 | `state-and-runtime.md` §2.2, §3 | 前提として参照 |
| rate limit 初期値、失敗・整合性境界 | `transport-boundaries.md` §4, §5, TB-06 | 値を再定義せず、接続キューとの対応を補う |
| メトリクス/ログの最低要件 | `technology-decisions.md` TD-08, TD-09 | 前提として参照。本書の観測項目はこれへ追加する |
| 認証 token 更新、失効モデル | ADR-002 | 本書は再接続時の token 更新要求を整理し、確定は ADR-002 へ委ねる |
| sequence の再接続時採番・replay dedup | ADR-006A（RP 書 RP-01/RP-02） | 前提として参照 |

### 0.2 所有権と DAG の前提

**設計前提** 本書が扱う主要モジュールの所有権は `architecture.md` §3 の設計に従う（本書は所有権を再定義しない）。

| モジュール | 所有するもの | 所有しないもの |
|---|---|---|
| `identity_access` | Identity Session、access/refresh token、`AuthSessionId` | Realtime 接続、Presence |
| `realtime_presence` | Realtime Connection / Presence、resume binding、`RealtimeConnectionId` / `PresenceId`、Interest 購読の binding 情報 | socket、AuthSessionId |
| `instance_runtime` | 正準な一時状態、revision、Entity/Transform 権威 | delivery、interest、socket |
| `interest` | 可視集合計算の policy、空間索引（派生 view）、Interest 購読状態 | I/O、socket、queue、gateway への呼出し |
| `realtime_delivery` | 接続別 bounded queue、delivery 調停 | socket 自体 |
| `realtime_gateway` | WSS socket、接続 I/O | 正準状態、queue 調停 |

**設計前提** `interest` は I/O を行わず、`realtime_presence` / `realtime_delivery` / `realtime_gateway` を呼ばない。`instance_runtime` も `interest` や delivery を呼ばない。呼出しは Application coordinator が調停する（`architecture.md` §3.1 DAG）。

**[SPEC]** 認証セッションとワールド接続セッションは分離する（仕様 §9.5）。

**設計前提** 上記分離に従い、Resume Token は access token でも `AuthSessionId` でもなく、`realtime_presence` が所有する復帰用 binding 情報である（`architecture.md` §3、`domain-model.md` §3.6、RP 書 §1）。

## 1. 障害前提と設計原則

### 1.1 前提とする障害

**[SPEC]** 本要件は音声やアセットではなく、コアの WebSocket/REST 通信に適用する（仕様 §16）。

**[SPEC]** 仕様 §16.1 が前提とする障害：

| 障害 | 設計上の意味 |
|---|---|
| 4G/5G の切り替え | 経路変更。接続断または IP 変更を伴いうる |
| 基地局ハンドオーバー | 短時間のパケットロス・遅延 |
| 短時間の圏外 | 一時的な到達不能。復帰前提 |
| IP アドレス変更 | 同一 TCP 接続の維持不能。再接続が必要 |
| 高遅延 | timeout を短くしすぎない |
| ジッター | 順序・タイミングの揺らぎ。sequence で検知 |
| 一時的なパケットロス | 再送または最新状態優先で吸収 |
| アプリのバックグラウンド化 | 送信停止・heartbeat 間隔の間延び |
| OS によるソケット停止 | 明示的な切断通知なき接続断 |
| TCP 接続の半開き | 片側のみ生存。application-level heartbeat で検知 |

### 1.2 設計原則

**[SPEC]** 公式 SDK はモバイル回線の切断と遅延から復帰できる設計とする（仕様 §16、`technology-decisions.md` §6.2）。

**[REC]** 再接続と resume は最適化である。復帰材料（Resume Token、差分履歴）が無効・期限切れ・不足の場合は、安全な完全再同期（新規 Snapshot）へ移行する。復帰の失敗を接続の致命的失敗にしない（`architecture.md` §5.4）。

**[REC]** 接続の連続性は transport の sequence ではなく state の revision で担保する。再接続では新しい `RealtimeConnectionId` を割り当て、sequence は新接続で再開する（RP 書 §4.2、ADR-006A）。

## 2. 自動再接続と状態機械

### 2.1 再接続手順

**[SPEC]** 公式 SDK は自動再接続を実装する（仕様 §16.2, §24.1）。手順：

1. 切断検知
2. 指数バックオフ + jitter
3. アクセストークン更新
4. 新しい WebSocket 確立
5. ResumeSession 送信
6. 復帰可能なら差分再開
7. 復帰不可能なら新しい Snapshot 取得

**[REC]** 手順と所有モジュールの対応（`architecture.md` §5.4 と整合）：

| 手順 | 主体 | 所有モジュール | 備考 |
|---|---|---|---|
| 切断検知 | client SDK | — | heartbeat タイムアウトまたは socket close |
| 指数バックオフ + jitter | client SDK | — | §2.4 |
| アクセストークン更新 | client SDK → REST | `identity_access` | 期限切れ時のみ。ADR-002 |
| 新しい WebSocket 確立 | client SDK | `realtime_gateway` | 新しい `RealtimeConnectionId` |
| 認証 | coordinator | `identity_access` | `validateAccessToken(token)`（RP 書 §1） |
| ResumeSession 送信 | client SDK | `realtime_presence` | resume token + last_revision |
| resume binding 検証 | coordinator | `realtime_presence` | §3。認証と独立に検証 |
| インスタンス可用性検証 | coordinator | `world_directory` | 停止中は再参加不可 |
| 差分再開 or Snapshot | coordinator | `instance_runtime` | §4 |
| Interest 再購読 | coordinator | `interest` | §7。可視集合を再計算 |

**設計前提** `realtime_presence` の resume binding 検証は、`identity_access` の認証とは独立に実施する（`architecture.md` §5.4）。access token の認証に成功しても、resume binding が無効なら差分再開はできない。

### 2.2 サーバー側の resume 状態機械

**[REC]** RP書 §5.1のserver接続状態`resuming`を本書が具体化する。新接続は`connecting → awaiting_hello → ready`を経た後、resume分岐に入る。

```text
ready（認証済み・resume token提示あり）
   │ ResumeSession 受信
   ▼
┌──────────┐
│ resuming  │  resume token + binding + instance可用性を検証
└──┬─────┬─┘
   │     │
   │     │ token 有効・binding 一致・履歴あり
   │     ▼
   │  ResumeAccepted + 差分 replay ──► active
   │
   │     token 有効・binding 一致・履歴欠落
   ├────────────────────────────► ResyncRequired + Snapshot ──► active
   │
   │     token 無効/期限切れ/binding 不一致/instance 停止
   └────────────────────────────► resume 拒否（ErrorMessage）
                                       │
                                       ▼
                                 readyへ降格（通常のJoinInstanceフローへ）
```

**[REC]** resumeの成否は接続状態を維持したまま決まる。resume拒否は接続レベルの失敗ではなく、通常の入室フロー（RP書 §5.1の`ready → joining → active`）への降格である。接続は切断しない。

**[REC]** serverの`resuming`で許可される受信payloadはResumeSessionとHeartbeatのみとする。検証完了まで状態変更系payloadは保留する。

**[REC]** resume 成功時は `PresenceId` を再 bind して維持する（同一 Membership の継続）。resume 拒否からの再参加は新しい `PresenceId` を生成する。いずれも `RealtimeConnectionId` は新接続のものへ置き換わる。

### 2.3 クライアント側の再接続状態

**[REC]** client SDK の接続状態（コアの責務外だが契約として規定）：

```text
Disconnected → Backoff → Reconnecting → Active
                                      │
                                      └─ resume 拒否/失敗 → Rejoining → Active
```

`Reconnecting`はtransport再確立、ClientHello/ServerHello、ResumeSession応答待ちを含む。公開SDK状態名と遷移の正本は`client-sdk.md` §3.4とする。

**[SPEC]** client SDK は latest-wins 送信キュー、sequence 管理、heartbeat、Snapshot/Delta 適用、自動再接続、Resume を実装する（仕様 §24.1、`technology-decisions.md` TD-12）。

### 2.4 指数バックオフ + jitter

**[REC]** 再接続待機は指数バックオフ + jitter で計算する。同一クライアントの再接続がサーバーへ殺到するのを防ぐ。

**[REC]** 推奨初期式（full jitter）：

```text
base = 1 s
cap  = 30 s
attempt n（0 始まり）:
  sleep = random(0, min(cap, base * 2^n))
```

**[ADR]** base、cap、jitter 方式（full / equal / decorrelated）、最大試行回数、試行上限到達後の挙動（ユーザー通知等）は MRIB-01 で決める。

### 2.5 アクセストークン更新

**[SPEC]** 公式 SDK は Access Token 更新の責務を持つ（仕様 §24.1）。

**[SPEC]** token は短命 Access Token + 長命 Refresh Token、Refresh Token の DB ハッシュ保存と rotation、ログアウト/ユーザー無効化時の失効を推奨モデルとする（仕様 §19.4）。

**[REC]** 再接続時にAccess Tokenが期限切れの場合、client SDKはRESTでrefreshしてから新しいRealtime接続ticketを取得し、WebSocketを確立する。

**設計前提** token形式・期限・rotation・失効とRealtime接続ticketはADR-002で確定した（`docs/adr/ADR-002-authentication-session.md`）。

**[REC]** token 更新の失敗（refresh token 失効等）は、再接続の断念と再ログイン要求を意味する。この場合 resume は試行しない。

## 3. Resume Token

### 3.1 性質

**[SPEC]** Resume Token の性質（仕様 §16.3）：

- 短時間のみ有効
- ユーザー、インスタンス、セッション epoch へ紐づく
- 推測困難
- サーバー側で無効化可能
- 切断前の最終受信 revision を含められる

**所有モジュール:** `realtime_presence`（resume binding、`architecture.md` §3）。

### 3.2 binding と内容

**[REC]** Resume Token は次の binding を server 側で保持する opaque な参照子である：

| binding 項目 | 意味 |
|---|---|
| `UserId` | 認証済み主体 |
| `InstanceId` | 復帰先インスタンス |
| `PresenceId` | 切断前の Membership |
| session epoch | bind 世代の単調増加番号（§3.3） |
| last revision | 切断前の最終受信 instance revision（§4） |
| 有効期限 | 発行時刻 + grace（§3.4） |

**[REC]** token 自体は暗号学的に安全な乱数（推測困難）とし、server 側に binding を格納する。token を自己完結型（signed token）にするか server 格納型にするかは MRIB-02 で決める。

**[SPEC]** サーバー側で無効化可能とする（仕様 §16.3）。

**[REC]** raw token および先頭数文字（prefix）はログ・メトリクス・telemetry へ記録しない（仕様 §26.4、`technology-decisions.md` TD-08）。相関には鍵付き hash（HMAC）または server 側 opaque correlation ID のみを用いる。平文ハッシュは token を復元可能な参照子になりうるため、鍵付きでない hash は用いない。

### 3.3 session epoch

**[REC]** session epoch は `PresenceId` の bind 世代を表す単調増加番号である。新しい接続で rebind するたびに増加する。

**[REC]** ResumeSession が提示した token の epoch が server 側の現行 epoch と一致しない場合、その resume は古いセッション由来として拒否する。これにより、二重参加や、追い出された旧セッションからの復帰試行を検知する。

**[ADR]** 二重参加の検知方式と、同一ユーザーの複数接続を許容するかは DM-07 / 再接続 ADR と合わせて決める（`domain-model.md` §3.6）。

### 3.4 寿命と無効化

**[REC]** Resume Token の有効期限は resume grace 期間と等しくする。推奨初期値は 60 秒（`domain-model.md` DM-07）。

**[ADR]** grace 期間の確定値は DM-07 で決まる。本書は DM-07 の値を token 寿命として再利用し、別途固定しない。

**[REC]** 無効化トリガー：

| トリガー | 効果 |
|---|---|
| 有効期限到達（TTL） | token 失効。以降の resume は拒否 |
| 成功した resume（単一使用） | 使用済み token を消費し、新しい token を発行（回転） |
| 明示的ログアウト / セッション失効 | 当該セッションの token を無効化 |
| kick | 当該 Membership の token を無効化 |
| インスタンス停止 | 当該インスタンスの全 token を無効化 |
| epoch 不一致 | 提示 token を拒否（§3.3） |

**[REC]** Resume Token は単一使用 + 回転とする。resume 成功時に `ResumeAccepted` で新しい token を返し、次回切断に備える。replay による再利用を防止する。

**[REC]** Resume Tokenは認証を兼ねない。新接続は必ず有効なAccess Tokenから発行した単一使用Realtime接続ticketで認証し、Resume TokenはPresence bindingの復元にのみ使用する（ADR-002、§0.2、RP書 §1）。

## 4. 再同期

### 4.1 revision ベースの判定

**[SPEC]** サーバーが差分履歴を保持している場合、差分のみを送信する（仕様 §16.4）：

```text
Client last_revision = 1200
Server current_revision = 1230
Server retains 1201..1230
→ 差分だけ送信
```

**[SPEC]** 履歴がない場合、ResyncRequired から新しい Snapshot へ移行する（仕様 §16.4）：

```text
→ ResyncRequired
→ 新しい Snapshot
```

**[REC]** 判定は instance revision を基準に行う（RP 書 §4）。ResumeSession の `last_revision` を server の履歴窓と比較する。

### 4.2 差分履歴バッファ

**[REC]** `instance_runtime` はインスタンスごとに reliable イベントの差分履歴を bounded ring buffer で保持する。履歴は reliable イベント（EntityCommand、DomainEvent）のみを対象とし、latest-wins 状態の過去値は保持しない。

**[ADR]** バッファの保持量（revision 幅またはイベント数）、保持時間、メモリ上限は MRIB-03 で決める。

### 4.3 再同期の分岐

**[REC]** 分岐ロジック：

| 条件 | 判定 | 応答 |
|---|---|---|
| `buffer.oldest ≤ last_revision ≤ current` | 履歴内で連続 | 差分replay（§4.4）→ `active` |
| `last_revision < buffer.oldest` | 履歴欠落（gap） | ResyncRequired + 新規Snapshot → `active` |
| `last_revision > current` | 異常（古い/混乱したクライアント） | ResyncRequired + 新規Snapshot → `active` |
| resume token 無効（§3） | 復帰不能 | resume 拒否 → 通常入室（新規 Snapshot） |

**[REC]** gap 判定後の Snapshot は RP 書 §7 の Join 時 Snapshot と同一の手順（interest による可視集合の純粋計算 → filtered snapshot → 当該接続へ配信）で生成する。

### 4.4 差分 replay

**[REC]** 差分 replay は `(last_revision, current]` 範囲の reliable イベントを順序保持・重複排除して送信する。

**[SPEC]** reliable イベントは破棄してはならない（仕様 §16.5、RP 書 §3.1）。replay 中でもこれは維持する。

**[REC]** latest-wins 状態は履歴 replay しない。replay 完了時点の最新値（current の状態）を 1 件送る。古い latest-wins 値は無意味であるため（§5）。

**[REC]** 新接続の sequence は再開されるため、replay される reliable イベントは新接続で採番し直す。重複排除は message_id を用いる（RP 書 §4.2/§4.3、ADR-006A）。

## 5. 最新状態優先と reliable 分離

### 5.1 latest-wins の上書き

**[SPEC]** 位置更新は送信キュー上で同一エンティティの古い更新を上書きする（仕様 §16.5）：

```text
x=1.0 → x=1.1 → x=1.2 → x=1.3
送信詰まり発生
→ x=1.3 だけを残す
```

**[REC]** 上書きは `realtime_delivery` の接続別キューの latest-wins 領域で行う。key は `(entity_id, component)` とし、同 key の最新値のみを保持する（RP 書 §3.1 Latest-wins クラス）。

### 5.2 reliable との分離

**[SPEC]** Reliable イベントは破棄してはならない。最新状態とイベントは別キューまたは別優先度で処理する（仕様 §16.5, §14.2、`architecture.md` §5.3）。

**[REC]** 分離は接続別キュー内の領域分離で実現する。詳細は §8.2 で定義する。latest-wins の上書き・drop が reliable 領域へ影響してはならない。

## 6. ハートビート

### 6.1 方式

**[SPEC]** application-level heartbeat を使用する。WebSocket ping/pong だけに依存しない（仕様 §16.6）。TCP 半開きや OS によるソケット停止を検知するためである（§1.1）。

**[SPEC]** 要件（仕様 §16.6）：

- RTT を測定する
- 連続失敗で切断扱いにする
- モバイル回線向けに過度に短い timeout を設定しない

**[REC]** Heartbeat / HeartbeatAck は Control クラスのメッセージであり、送信は RP 書 §3.3 の単一直列化経路を通る（RP 書 §3.2）。

### 6.2 方向と検知

**[REC]** client が Heartbeat を定期送信し、server が HeartbeatAck を返す。server は最終受信時刻を監視し、client は HeartbeatAck で RTT を測定する。server 側も必要に応じて Heartbeat を送信して client 生存を確認してよい。

**[REC]** 連続失敗の検知：

- server: 一定回数連続して Heartbeat（または有効なフレーム）を受信しなければ接続を切断扱いにする
- client: 一定回数連続して HeartbeatAck を受信しなければ切断扱いにし、再接続（§2）を開始する

**[REC]** heartbeatタイムアウトによる切断後、serverは接続を`closed`へ遷移させ、Presenceはresume grace期間だけMembershipを保持する（RP書 §5.3、`domain-model.md` DM-07）。grace内のresumeは§2/§3、grace超過後は`PresenceId`を終了し`MemberLeft`を発行する。

### 6.3 推奨初期値

**[SPEC]** 推奨初期値（仕様 §16.6）：

| パラメータ | 推奨初期値 |
|---|---|
| heartbeat 間隔 | 15〜30 秒 |
| timeout | 45〜90 秒 |
| 設定方法 | 環境変数で変更可能 |

**[ADR]** 間隔・timeout・連続失敗回数の確定値（上記範囲内）は MRIB-04 で決める。環境変数で上書き可能とする。

**[REC]** RTT 測定値は観測にのみ使用し、サーバー権威の判定（速度検証等）には使用しない。

## 7. Interest Management

### 7.1 目的と所有権

**[SPEC]** 同一ワールドに多数のユーザーが存在しても、各クライアントへ必要な状態だけを配信する（仕様 §17.1）。

**所有モジュール:** `interest`。state/visibility view から recipient ID 集合または可視集合を計算する I/O なしの policy/domain service（`architecture.md` §3）。

**設計前提** `interest` は I/O を行わず、connection queue / socket / gateway に依存しない（`architecture.md` §3, §3.1）。計算に必要な view は coordinator が `instance_runtime` から取得して `interest` へ渡す。

**[REC]** `interest` の純粋計算には 2 つの利用形態がある。いずれも view を入力とする純粋関数である：

| 利用形態 | 入力 | 出力 | 使用箇所 |
|---|---|---|---|
| Join/Resync 時可視集合 | joining subject の視点 + 候補状態集合 | subject に見える entity/presence 集合 | RP 書 §7.1、本書 §4.3 |
| 更新時 recipient 集合 | 状態変更 view + 購読状態 | 配信先 recipient ID 集合 | `architecture.md` §5.3 |

### 7.2 Uniform Spatial Grid による可視集合計算

**[SPEC]** Uniform Spatial Grid を標準とする（仕様 §17.2）。各エンティティを現在位置のセルへ登録し、クライアントは自身のセルと周辺セルを購読する。

```text
World
├─ Cell (0,0)
├─ Cell (0,1)
├─ Cell (1,0)
└─ Cell (1,1)
```

**[REC]** 空間索引は `interest` が所有する派生 view である。正準状態ではなく、`instance_runtime` の状態から再計算可能である（`state-and-runtime.md` §1.1「Interest 購読状態」は一時状態）。

**[REC]** 索引の更新は coordinator 経由でのみ行う。coordinator が位置更新を `interest` へ渡し、`interest` が entity のセル登録を更新する。`interest` 自ら `instance_runtime` を呼ぶことはない（DAG 遵守、§0.2）。

**[ADR]** セルサイズ、購読する周辺セルの範囲（例: 周囲 1 周）、索引の更新方式（incremental 更新 vs 再計算）は MRIB-05 で決める。

### 7.3 配信レベル

**[SPEC]** 配信レベルの例（仕様 §17.3）：

| レベル | 頻度 | 情報量 |
|---|---|---|
| Near | 高頻度 | 完全状態 |
| Mid | 低頻度 | 簡略状態 |
| Far | 非配信または集計情報 | 最小 |
| Global | 距離無関係 | 管理イベント等 |

**[SPEC]** コアは描画 LOD を管理しない。配信する情報量と頻度のみを管理する（仕様 §17.3）。

**[REC]** レベルは距離・可視性から `interest` が純粋計算で決定し、可視集合と合わせて出力する。レベルは latest-wins 状態（StateDelta）の配信頻度と完全度に影響する。reliable イベントは可視である限りレベルによらず配送する。

**[REC]** Near/Mid/Far/Global の 4 レベルを初期構成として採用する。仕様 §17.3 はこれらを例示しており、確定は MRIB-06 で行う。

**[ADR]** レベルの距離閾値、レベルごとの配信頻度、簡略状態の定義（どの component を省略するか）は MRIB-06 で決める。

### 7.4 可視性ポリシー

**[SPEC]** 可視性ポリシー（仕様 §17.4、`domain-model.md` §4.3）：

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

**[REC]** ポリシーの評価は `interest` 内で純粋に行う。`Custom` の tag 解釈は `interest` の policy evaluator が担い、コアは tag を透過的に保持する（`domain-model.md` §4.3）。

**[REC]** 可視性は空間条件とポリシー条件の両立で決まる。ある entity が subject に可視であるのは、空間的に購読範囲内 かつ ポリシーが許可する場合である。`Global` ポリシーは空間条件を緩和し、`OwnerOnly` は空間内でも owner のみを許可する。

**[REC]** `RoleRestricted` の評価に必要な role 情報は、coordinator が認証済み主体の view として `interest` へ渡す。`interest` は `identity_access` を呼ばない（DAG 遵守）。

### 7.5 ヒステリシス

**[SPEC]** 境界付近で購読と解除が頻発しないよう、参加半径と離脱半径を分けてよい（仕様 §17.5）。

**[REC]** 推奨初期値（仕様 §17.5 例）：

| パラメータ | 値 |
|---|---|
| subscribe radius | 30 m |
| unsubscribe radius | 35 m |

**[REC]** 購読状態の管理：entity が subscribe radius 以内に入ったとき購読を開始し、unsubscribe radius を超えたときのみ解除する。30 m を超えても 35 m 以内であれば購読を維持する。これにより境界付近の揺らぎ（flapping）を抑制する。

**[REC]** 購読状態（recipient ごとに購読中の entity 集合）は `interest` が ephemeral な派生状態として保持する（`state-and-runtime.md` §1.1）。recipient ID を key とし、connection/socket へ直接依存しない。

**[ADR]** subscribe/unsubscribe 半径の確定値、レベル境界との関係、揺らぎ抑制の追加方策（dwell time 等）は MRIB-07 で決める。

## 8. バックプレッシャー

### 8.1 原則と単一 writer

**[SPEC]** 遅いクライアント 1 台がインスタンス全体を遅延させてはならない（仕様 §18.1）。

**[REC]** 1 接続の socket への書き込みは単一の直列化された経路に集約する（RP 書 §3.3）。本章はこの経路の内部構造と上限値を定義する。

**所有モジュール:** `realtime_delivery`（接続別 bounded queue と delivery 調停）。socket は `realtime_gateway` が所有し、Application-owned outbound sink port を実装する（`architecture.md` §3, §3.1）。

### 8.2 接続ごとの送信キュー

**[SPEC]** 接続ごとの送信キューの要件（仕様 §18.2）：

- bounded queue
- メッセージ種別ごとの優先度
- latest-wins 状態の上書き
- reliable イベントの上限
- 上限超過時は警告後に切断可能

**[REC]** キューは接続ごとに独立した bounded priority queue とし、次の領域で構成する：

| 領域 | 対象 | 優先度 | 飽和時動作 |
|---|---|---|---|
| Control | ServerHello, JoinAccepted, ErrorMessage, HeartbeatAck, ResyncRequired | 最高 | 優先配送。飽和は接続異常の兆候 |
| Reliable（event lane） | EntityCommand, DomainEvent | 中 | 順序保持・欠落禁止。上限超過時は警告後に接続切断（silent drop しない） |
| Reliable（bulk lane） | Snapshot chunk | 中（event lane より低い重み） | 順序保持・欠落禁止。レート制御。上限超過時は警告後に接続切断 |
| Latest-wins | StateDelta, TransformInput 配信 | 通常 | 同 key 上書き。古い更新を drop |

**[REC]** 優先度は Control > Reliable > Latest-wins の順とする。Reliable 内では event lane を bulk lane より優先する。`state-and-runtime.md` §3.2 の mailbox 優先度（管理 command 高、位置更新通常）と整合させる。

**[SPEC]** Reliable イベントは上限超過時に drop せず、警告後に接続を切断してよい（仕様 §18.2, §16.5）。

**[REC]** Snapshot は RP 書 §3.2 / §7.2 で Reliable クラスと確定している。本書のキューでも Snapshot chunk を Reliable として扱い、順序・欠落・silent drop 禁止を維持する。Control とは分離する（Control に積まない）。大量の Snapshot が Control や event lane を飢餓させないよう、Reliable 内に bulk lane を分離し、重みとレートで制御する。

**[ADR]** 領域ごとの容量上限、優先度重み、Reliable 内の event/bulk lane 重み（Control 飢餓の防止）、Snapshot chunk のレートは MRIB-08 で決める。rate limit の初期値は TB-06（通常 100 msg/s、カスタムイベント 10 msg/s）を参照し、負荷試験で調整する。

### 8.3 Slow Consumer

**[SPEC]** Slow Consumer はメトリクス化する（仕様 §18.3）：

- キュー長
- ドロップした状態更新数
- reliable queue overflow 数
- 書き込み遅延
- slow consumer 切断数

**[REC]** slow consumer の検知と対応：

| 兆候 | 判定 | 対応 |
|---|---|---|
| latest-wins drop の継続 | 通常（想定内） | メトリクス記録。接続は維持 |
| キュー長が閾値を継続超過 | slow consumer | 警告 → 改善しなければ切断 |
| reliable queue overflow | 重大 | 警告後に接続切断（§8.2） |
| 書き込み遅延の増大 | slow consumer | 警告 → 切断 |

**[REC]** latest-wins の drop は最新状態優先の正常動作であり、切断の直接理由にしない。reliable overflow と持続的なキュー飽和を切断の基準にする。

**[ADR]** slow consumer 判定の閾値（キュー長、持続時間、書き込み遅延）は MRIB-09 で決める。

### 8.4 インスタンス保護

**[SPEC]** インスタンス保護の rate limit（仕様 §18.4）：

- 接続単位 rate limit
- ユーザー単位 rate limit
- IP 単位ログイン rate limit
- インスタンス単位総入力上限
- 高コストイベントの同時実行制限

**[REC]** rate limit の所有と配置：

| rate limit | 配置 | 所有モジュール |
|---|---|---|
| 接続単位 | inbound frame 検証 | `realtime_gateway` / coordinator |
| ユーザー単位 | command 検証 | coordinator |
| IP 単位ログイン | REST ログイン | `http_api` |
| インスタンス単位総入力 | mailbox 入口 | `instance_runtime` / coordinator |
| 高コストイベント同時実行 | command 処理 | `instance_runtime` |

**[REC]** 高コストイベント（大量 spawn、広域イベント等）は同時実行数を制限し、インスタンスの tick を圧迫しないようにする（`state-and-runtime.md` §3.3）。

**[REC]** rate limit 超過の応答は scope 別に設計する。仕様 §18.4 は rate limit の項目を定めるが、超過時の応答（拒否か切断か）までは確定しない。

| scope | 単発の超過 | 継続的な abuse |
|---|---|---|
| 接続単位 | 当該 command を拒否（ErrorMessage）/ スロットル | 警告後に当該接続を切断 |
| ユーザー単位 | 当該 command を拒否（ErrorMessage、REST では 429 相当） | 当該ユーザーの接続を切断 |
| IP 単位ログイン | REST で 429 | 当該 IP からのログインを一時拒否 |
| インスタンス単位総入力 | 新規入力を拒否（load shedding） | インスタンス保護を優先。無関係な接続は切断しない |
| 高コストイベント | 同時実行を制限・キューイング | 制限超過分を拒否 |

**[REC]** インスタンス単位・グローバルの制限はインスタンス保護が目的であり、制限超過を理由に無関係な接続を切断しない。負荷 shedding は新規/高コスト入力の拒否で行い、既存の健全な接続は維持する。

**[ADR]** 各 scope の閾値、単発拒否と切断の切り分け基準、shedding の方式は MRIB-10 で決める。各 rate limit の具体値は TB-06 / ARC-06 と負荷試験で調整する。本書は配置・所有・応答方針を定め、値を固定しない。

## 9. 観測項目

### 9.1 メトリクス

**[SPEC]** 最低限のメトリクスは仕様 §27.2 の確定要件である（`technology-decisions.md` TD-09 が整理）。本書の領域に関連する仕様 §27.2 の項目：`resume_attempts_total`、`resume_success_total`、`outbound_queue_depth`、`state_updates_dropped_total`、`snapshot_bytes_total`、`delta_bytes_total`、`websocket_disconnects_total`。

**[REC]** 本書が仕様 §27.2 に追加する観測項目：

| メトリクス | 種別 | 関連節 |
|---|---|---|
| `resume_failed_total{reason}` | counter（reason: expired/gap/epoch/rejected） | §2, §3, §4 |
| `resync_full_total` | counter | §4 |
| `delta_replay_total` | counter | §4 |
| `heartbeat_timeout_total` | counter | §6 |
| `reliable_queue_overflow_total` | counter | §8.2, §8.3 |
| `slow_consumer_disconnect_total` | counter | §8.3 |
| `rate_limit_rejected_total{scope}` | counter（scope: connection/user/ip/instance） | §8.4 |
| `interest_visible_set_size` | histogram | §7 |

**設計前提** UserId、ConnectionId、WorldInstanceId などの高 cardinality 値を metric label にしない（`technology-decisions.md` TD-09 の設計制約）。`reason` / `scope` などの低 cardinality 区分のみ label とする。

### 9.2 ログ

**[SPEC]** 構造化ログの必須フィールドは仕様 §27.1 の確定要件である（`technology-decisions.md` TD-08 が整理）：timestamp, level, service, version, request_id, connection_id, session_id（必要時に匿名化）, instance_id, user_id（必要時に匿名化）, event, duration_ms, error_code。

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4）。cookie、authorization header、不要な個人情報も記録しない（`technology-decisions.md` TD-08）。

**[REC]** Resume Token、access token、refresh token は raw および先頭数文字（prefix）のいずれもログ・メトリクス・telemetry へ記録しない。相関が必要なら鍵付き hash（HMAC）または server 側 opaque correlation ID のみを用いる（§3.2）。

**[REC]** resume の成否、replay 範囲、resync 移行、slow consumer 切断、rate limit 拒否は、connection_id / instance_id / user_id を含むイベントログとして記録する。

## 10. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。fake adapter と決定論的 clock を用いる（`architecture.md` §9、仕様 §31.4）。

### 10.1 再接続と resume

1. `active`接続のTCPを強制切断し、clientが指数バックオフ + jitterの後に再接続すると、ResumeSessionが送られ、履歴窓内の`last_revision`に対して差分replay後`active`へ復帰する。復帰後のinstance revisionはserverのcurrentと一致する。
2. access token 期限切れ状態で切断すると、client は refresh 後に再接続する。refresh 失敗時は再接続を断念し、resume を試行しない。
3. バックオフ待機時間は `min(cap, base * 2^n)` の範囲内に jitter され、同一時刻に集中しない。

### 10.2 Resume Token

4. 有効な token での resume 成功後、旧 token は無効化され、新しい token が発行される（単一使用・回転）。旧 token の再提示は拒否される。
5. grace 期間経過後の resume は拒否され、client は新規 Snapshot による入室へ移行する。接続は切断されない。
6. epoch 不一致の token 提示は拒否される。
7. ログ・メトリクス・telemetry に token の raw および先頭数文字（prefix）が含まれない。相関は鍵付き hash（HMAC）または server 側 opaque correlation ID のみで行う。

### 10.3 再同期

8. `last_revision < buffer.oldest` の resume は ResyncRequired + 新規 Snapshot へ移行し、欠落なく最新状態へ一致する。
9. `last_revision > current` の異常な提示は ResyncRequired + Snapshot で回復する。
10. 差分 replay は reliable イベントを順序保持・重複排除して送り、latest-wins の過去値は送らない。

### 10.4 latest-wins と reliable 分離

11. 書き込みを詰まらせた状態で同一 entity の `x=1.0..1.3` を投入すると、再開後に配送される latest-wins 値は最新（1.3）のみである。
12. 同一期間に投入した reliable イベントおよび Snapshot chunk はすべて順序保持・欠落なく配送され、1 件も silent drop されない。
13. reliable queue が上限超過すると、警告後に接続が切断され、イベントは silent drop されない。大量の Snapshot chunk（bulk lane）配信中も Control（HeartbeatAck、ErrorMessage 等）が飢餓しない。

### 10.5 単一 writer と隔離

14. 任意の瞬間に 1 接続の socket へ並行 write が発生しない（計装またはテストで検証）。
15. 1 接続を slow consumer 状態にしても、同一インスタンスの他接続の配送と `instance_runtime` の tick が遅延しない。

### 10.6 Interest

16. `interest` の可視集合計算は、同一入力 view に対して同一出力を返す純粋関数である。`interest` のテストに I/O adapter / socket / queue を必要としない。
17. ヒステリシス：30 m 以内の entity を購読し、32 m へ移動しても購読を維持し、36 m で解除し、34 m へ戻っても再購読せず、30 m 以下で再購読する。
18. `OwnerOnly` entity は空間内でも owner 以外の subject に可視とならない。`Global` entity は距離に関係なく可視となる。

### 10.7 heartbeat と rate limit

19. HeartbeatAckを停止すると、連続失敗後にclientは切断扱いにして再接続を開始する。serverは最終受信からtimeout後に接続を`closed`へ遷移させ、Presenceはgrace期間保持される。
20. 接続単位 rate limit を超えた inbound は拒否/切断され、インスタンス単位総入力上限は `instance_runtime` の tick を保護する。

### 10.8 Identity / Realtime 分離

21. Resume Token単独では認証できず、新接続には有効なAccess Tokenから発行した単一使用Realtime接続ticketが必須である。`AuthSessionId`はRealtime接続キーとして使用されない。

## 11. 要 ADR 事項

本書が主担当となる判断を MRIB ID で管理する。他文書が正本の判断（DM-07、TB-06、ARC-06、ADR-002、ADR-006A 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| MRIB-01 | 指数バックオフ + jitter の初期値 | base 1 s、cap 30 s、full jitter | 再接続殺到の回避。具体値は負荷試験で調整 |
| MRIB-02 | Resume Token の形式・entropy・保管 | 128 bit 以上の opaque 乱数、server 格納、単一使用・回転 | 推測困難（仕様 §16.3）。signed token 化は将来候補 |
| MRIB-03 | 差分履歴バッファの保持量 | revision 幅またはイベント数 + メモリ上限の bounded ring | 再同期判定（§4）。具体値は負荷試験で調整 |
| MRIB-04 | heartbeat 間隔・timeout・連続失敗回数 | 間隔 20 s、timeout 60 s、環境変数で上書き | 仕様 §16.6 の範囲内。モバイルの遅延を許容 |
| MRIB-05 | 空間グリッドのセルサイズと索引更新方式 | incremental 更新、セルサイズはワールド尺度に依存 | 可視集合計算の性能。具体値は負荷試験で調整 |
| MRIB-06 | 配信レベルの閾値・頻度・簡略状態定義 | Near/Mid/Far/Global の 4 レベル、距離閾値は hysteresis と整合 | 仕様 §17.3 は例示。描画 LOD は管理しない |
| MRIB-07 | hysteresis の subscribe/unsubscribe 半径 | 30 m / 35 m | 仕様 §17.5 例。flapping 抑制 |
| MRIB-08 | 接続別キュー領域の容量・優先度重み・Snapshot bulk lane レート | Control > Reliable（event > bulk）> Latest-wins、容量は bounded、bulk lane は重み/レートで Control 飢餓を防止 | 仕様 §18.2。Snapshot=Reliable（RP 書 §3.2/§7.2）。具体値は TB-06 と負荷試験で調整 |
| MRIB-09 | slow consumer 判定閾値 | キュー長・持続時間・書き込み遅延の複合 | 仕様 §18.3。具体値は負荷試験で調整 |
| MRIB-10 | rate limit 超過時の scope 別応答ポリシー | 単発は command 拒否/429、継続 abuse のみ切断、instance/global は無関係な接続を切らず shedding | 仕様 §18.4 は項目のみ。応答方針は本書の設計。具体値は TB-06 / ARC-06 |
