# OrbiSync リアルタイムプロトコルと接続設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §14（リアルタイムプロトコル）と §15（接続フロー）を、Envelope 構造、メッセージ分類、sequence/revision モデル、メッセージ上限、接続ライフサイクルと失敗時状態遷移の設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: 仕様書で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項

モジュール所有権と DAG は `architecture.md` §3 に従う。本書は所有モジュールを再定義しない。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| REST / WebSocket の責務分離、protocol/domain DTO 変換 | `transport-boundaries.md` | 前提として参照。重複定義しない |
| メッセージ上限の初期値、圧縮、サブプロトコル名 | `transport-boundaries.md` §5 | 値を再定義せず、payload 種別への割当を補う |
| 状態分類、サーバー権威、入力検証境界、mailbox | `state-and-runtime.md` | 前提として参照 |
| ID 体系、Auth/Realtime/Presence のライフサイクル分離 | `domain-model.md` §2, §3.6 | 前提として参照。所有権を再定義しない |
| sequence / idempotency | ADR-006A | Acceptedな設計前提として参照する |
| protobuf 互換性、version negotiation、圧縮方式 | ADR-004 | Acceptedな設計前提として参照する |
| 自動再接続、Resume Token、再同期、ハートビート周期 | `mobile-resume-interest-backpressure.md`（仕様 §16） | 本書は接続状態機械の骨格のみ定義し、詳細は同書へ委ねる |
| Interest Management のアルゴリズム | `mobile-resume-interest-backpressure.md`（仕様 §17） | Snapshot の「適用済み」要件のみ扱う |
| バックプレッシャー、slow consumer、rate limit 具体値 | `mobile-resume-interest-backpressure.md`（仕様 §18）、`transport-boundaries.md` §4.4 | 接続状態への影響のみ扱う |

### 0.2 仕様記載の確定度

**[SPEC]** 仕様 §14.1 の Envelope は、現行 v1 の header field 集合・payload 種別・field number を含む確定記載である。本書はこれらを [SPEC] として扱い、変更しない。

**設計前提** ADR-004によりfield追加、reserved、version negotiation、breaking change規則を確定した。既存field numberを再利用しない。

**[REC]** 仕様 §15.1 の接続フローと §15.2/§15.3 の項目列挙は要求として確定する。仕様が内部構造を定めないpayload内部フィールドは、ADR-004 / ADR-006Aおよび公開正本`proto/orbisync/v1/realtime.proto`で確定する。

## 1. ID と所有権の前提

**[SPEC]** 認証セッションとワールド接続セッションは分離する（仕様 §9.5、`domain-model.md` §3.6）。本書が扱う接続フローはこの分離を前提とする。

| 概念 | 所有モジュール | 識別子 | ライフサイクル |
|---|---|---|---|
| Identity Session | `identity_access` | `AuthSessionId` | ログイン → token 失効/期限切れ |
| Realtime Connection | `realtime_presence` | `RealtimeConnectionId` | WSS 接続確立 → 切断 |
| Instance Membership | `realtime_presence` | `PresenceId` | JoinInstance → connection close/Kick/Timeout |

**[REC]** `realtime_presence` は `identity_access` を呼ばず、`AuthSessionId` を Realtime の主キーとして再利用しない（`architecture.md` §3.1）。

**[REC]** 接続確立時は、RESTでAccess Tokenを検証して単一使用Realtime接続ticketを発行し、`realtime_gateway → Application coordinator → identity_access.consumeRealtimeTicket(ticket) → authenticated subject + AuthSessionId → coordinator → realtime_presence.bind(...)`の方向で処理する（ADR-002、`architecture.md` §3.1）。raw token/ticketを`realtime_presence`へ渡さず、`AuthSessionId`をRealtime接続キーとして再利用しない。

**[REC]** 3 つのライフサイクルは独立して遷移する。`RealtimeConnectionId` の切断は `AuthSessionId` を失効させず、`PresenceId` の終了も `AuthSessionId` を失効させない（`domain-model.md` §3.6）。

## 2. Envelope

### 2.1 共通 Envelope の原則

**[SPEC]** すべてのリアルタイムメッセージを共通 Envelope へ格納する（仕様 §14.1）。

**所有モジュール:** `realtime_gateway`（encode/decode、socket I/O）。protocol DTO ↔ domain 型の変換ユーティリティは `protocol` crate が所有する（`transport-boundaries.md` §3.2）。

**[SPEC]** Envelope は接続制御・状態同期・イベントの全種別で共通の header を持つ。payload は 1 メッセージにつき 1 つとする（仕様 §14.1）。

### 2.2 Envelope header

**[SPEC]** 仕様 §14.1 の Envelope は次の header 情報を持つ。本書はこれを要求として確定する。

| header 項目 | 型（proto 例） | 意味 | 必須性 |
|---|---|---|---|
| protocol_major | uint32 | プロトコル major version | 全メッセージ必須 |
| protocol_minor | uint32 | プロトコル minor version | 全メッセージ必須 |
| message_id | string | メッセージの一意 ID | 全メッセージ必須 |
| sequence | uint64 | 接続ごとの単調増加番号（§4） | 全メッセージ必須 |
| sent_at_unix_ms | int64 | 送信時刻（Unix milliseconds） | 全メッセージ必須 |
| instance_id | string | 対象インスタンス ID | instance scope メッセージのみ |

**[SPEC]** realtime protocol の時刻は Unix milliseconds 等の明示形式とする（仕様 §31.4、`domain-model.md` §4.4、`technology-decisions.md` §6.4）。

**[REC]** `instance_id` は instance scope のメッセージ（JoinInstance 以降の StateDelta、EntityCommand、DomainEvent、Snapshot 等）に設定する。接続制御メッセージ（ClientHello、ServerHello、Heartbeat 等）では空を許容する。

**[REC]** `message_id` は UUID 文字列とする。Reliable イベントでは idempotency/dedup のキーとして再利用するため、再送時も同一値を維持する。Latest-wins メッセージでは毎回新規でよい（§4.3）。

### 2.3 payload oneof

**[SPEC]** 仕様 §14.1 は現行 v1 の Envelope payload 種別と field number を定める。次の oneof の field 集合と番号は [SPEC] として確定する。

```proto
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
```

**[SPEC]** 現行 v1 は header field に 1〜6、payload oneof に 20〜34 を使用する（仕様 §14.1）。7〜19 は header 拡張用の空き領域として確保されている。

**[SPEC]** field number を再利用しない。削除 field は reserved とする。unknown field を許容し、enum 追加を想定する（仕様 §34.4、`transport-boundaries.md` §3.4）。

**設計前提** ADR-004によりheaderは1〜19、payloadは20以降を使用し、削除fieldのname/numberをreservedへ移す。

**[REC]** 仕様 §14.1 が定めるのは Envelope の field 集合と番号であり、各payloadメッセージの内部フィールドまでは定めない。内部フィールドの公開契約は[`proto/orbisync/v1/realtime.proto`](../../proto/orbisync/v1/realtime.proto)を正本とする。

## 3. メッセージ分類

### 3.1 配信クラスの定義

**[SPEC]** 状態更新は最新値が意味を持つ latest-wins と、順序・重複排除が重要な reliable イベントへ分類する（仕様 §14.2、`architecture.md` §5.3）。

**[REC]** 本書は Envelope payload を次の 3 クラスへ分類する。この分類は `realtime_delivery` のキュー設計（latest-wins 集約と reliable 分離、`architecture.md` §5.3、`state-and-runtime.md` §3.2）へ直接対応する。クラスの違いは受信側の処理と、送信キュー内の優先度・集約方針の違いである。送信はいずれのクラスも接続単位の直列化された経路を通る（§3.3）。

| クラス | 意味 | 受信側の処理 | 送信側の配送 | 飽和時動作 |
|---|---|---|---|---|
| **Control** | 接続ライフサイクルと要求-応答 | `realtime_gateway` / coordinator が inline で処理 | 接続単位の直列化経路（bounded priority queue の最高優先度、§3.3） | 送信キュー飽和時は優先度に従う。詳細値は第18章 ADR |
| **Latest-wins** | 高頻度の一時状態。古い更新を破棄可能 | 検証後 `instance_runtime` へ | 最新値のみ配送。同 key は上書き | 古い更新を drop |
| **Reliable** | 順序と重複排除が重要。破棄不可 | 検証後 `instance_runtime` / 所有モジュールへ | 順序保持・重複排除して配送 | 上限超過時は警告後に接続切断（drop しない） |

**[SPEC]** Reliable イベントを破棄してはならない。最新状態とイベントは別キューまたは別優先度で処理する（仕様 §16.5、`architecture.md` §5.3）。

**[REC]** 「受信 control の inline 処理」は受信フレームの処理を指し、送信の直列化境界を迂回するものではない。ServerHello・JoinAccepted・ErrorMessage 等の control 応答の送信も §3.3 の直列化経路を通る。

### 3.2 payload とクラスの対応

**[REC]** 各 payload の分類：

| payload | クラス | 所有モジュール（意味の正本） | 備考 |
|---|---|---|---|
| ClientHello | Control | `realtime_gateway` | handshake |
| ServerHello | Control | `realtime_gateway` | handshake |
| JoinInstance | Control | `realtime_presence` | 入室要求。検証は application |
| JoinAccepted | Control | `realtime_presence` | 入室応答 |
| Snapshot | Reliable | `instance_runtime`（read model） | 分割送信可（§4.4, §7） |
| StateDelta | Latest-wins | `instance_runtime` | 定期状態配信 |
| TransformInput | Latest-wins | `instance_runtime`（入力） | クライアント入力 |
| EntityCommand | Reliable | `instance_runtime` | spawn/update/delete/transfer_ownership（ADR-027） |
| DomainEvent | Reliable | イベント所有モジュール（`domain-model.md` §5） | join/leave/ownership/role/moderation/custom 等 |
| Heartbeat | Control | `realtime_gateway` | 接続維持。周期は後続文書 |
| HeartbeatAck | Control | `realtime_gateway` | 接続維持 |
| ResumeSession | Control | `realtime_presence` | 再接続。詳細は後続文書 |
| ResumeAccepted | Control | `realtime_presence` | 再接続。詳細は後続文書 |
| ResyncRequired | Control | `instance_runtime` / coordinator | 再同期要求。詳細は後続文書 |
| ErrorMessage | Control | 各検証箇所 | §5.3 |

**[REC]** 仕様 §14.2 の分類例との対応：

- **Latest-wins（仕様例）**: transform、velocity、look direction、animation state、presence ping → 本書の StateDelta / TransformInput へ対応
- **Reliable（仕様例）**: join、leave、entity spawn、entity delete、ownership transfer、role change、custom domain event、moderation action → 本書の EntityCommand / DomainEvent へ対応

**[REC]** エンティティの生成・削除・所有権移譲のような信頼性が必須の状態変化は StateDelta へ載せず、EntityCommand または DomainEvent として送る。StateDelta は上書き可能な周期状態の配信に限定する。これにより latest-wins 集約が reliable イベントを巻き込んで破棄する経路を閉じる。

**[SPEC]** WebSocket 自体は順序保証されるが、アプリケーション層で再接続・再送・重複排除を扱う（仕様 §14.2）。transport の順序保証を application の配送保証と同一視しない。

### 3.3 送信の直列化とバックプレッシャー境界

**[SPEC]** 遅いクライアント 1 台がインスタンス全体を遅延させてはならない（仕様 §18.1）。接続単位の不正 frame や遅い受信者がインスタンス全体を停止させない隔離境界を置く（`architecture.md` §6、詳細は §5.4）。

**[REC]** 1 接続の socket への書き込みは、単一の直列化された経路に集約する。`realtime_gateway` への並行 write は禁止する。送信メッセージは `realtime_delivery` の接続単位 bounded priority queue、または Application-owned outbound sink port の直列化経路（`architecture.md` §3.1）を通って socket へ到達する。socket を所有するのは `realtime_gateway` だが、書き込み要求はこの単一経路に集約される。

**[REC]** この規則は Control クラスの送信メッセージ（ServerHello、JoinAccepted、ErrorMessage、HeartbeatAck、ResyncRequired 等）にも適用する。受信 control の inline 処理（§3.1）とその応答送信は別であり、応答送信も直列化経路を通る。これにより、制御応答が latest-wins/reliable と共通のバックプレッシャー境界の外へ迂回する経路を閉じる。

**[ADR]** 接続単位 bounded priority queue の優先度割当、容量、Control 送信の飽和時挙動の具体値は第18章（バックプレッシャー）の設計と ADR で決める。

## 4. Sequence と Revision

### 4.1 二つの番号の分離

**[SPEC]** 接続ごとに単調増加する sequence を付与する。インスタンス状態には revision を付与する。エンティティ単位にも revision を持たせてよい（仕様 §14.3）。

**[REC]** sequence と revision は目的が異なる別個の番号であり、兼用しない。

| 番号 | 目的 | スコープ | 所有 | 増加主体 |
|---|---|---|---|---|
| sequence | transport 上の順序確認・gap 検知・同一接続内の dedup | RealtimeConnectionId ごと | `realtime_gateway` | 送信側 |
| instance revision | 正準状態の楽観的並行制御・差分の基準点 | InstanceId ごと | `instance_runtime` | 状態遷移 |
| entity revision | エンティティ単位の更新整合性 | EntityId ごと | `instance_runtime` | 状態遷移 |

**[REC]** `Revision(u64)` はワールド定義・インスタンス・エンティティの楽観的並行制御に使用する（仕様 §14.3、`domain-model.md` §4.4）。

### 4.2 sequence の設計

**[REC]** 1 接続につき、送信方向ごとに独立した単調増加カウンタを持つ。

- server→client の `send_seq` と client→server の `recv_seq` を別々に管理する
- Envelope の `sequence` には、その方向の送信カウンタを設定する
- 受信側は期待される次の sequence を保持し、gap を検知したら protocol error または再同期の引き金とする
- 期待値より小さい sequence（重複）は drop する

**[REC]** sequence は `RealtimeConnectionId` の寿命に縛られる。再接続では新しい `RealtimeConnectionId` を割り当てるため（`architecture.md` §5.4）、sequence は新接続で再開する。接続をまたぐ状態の連続性は sequence ではなく revision と resume 材料で担保する。

**設計前提** ADR-006Aにより新接続のsequenceは1から再開し、replayするreliable messageは同じ`message_id`を維持する。

### 4.3 message_id による idempotency

**[REC]** Reliable イベントの `message_id` は再送・replay をまたいで同一に保ち、受信側の dedup キーとして使用する。Latest-wins メッセージの `message_id` は毎回新規でよく、dedup には使用しない。

**設計前提** ADR-006Aによりstate commandはUUIDv7の`command_id`を持ち、Instance単位で24時間dedupする。

### 4.4 revision による最新性検証

**[SPEC]** 古い revision のクライアント更新は拒否または再同期させる（仕様 §14.3、`domain-model.md` §4.4）。

**[REC]** revision の運用：

- TransformInput / EntityCommand は、クライアントが参照した entity revision を伴う
- `instance_runtime` は command 処理時に revision を検証し、古い更新は ErrorMessage（競合）または ResyncRequired で応答する
- Snapshot は現在の instance revision を含み、クライアントの差分適用の基準点とする（§7）
- StateDelta は適用対象の revision 範囲を含み、クライアントが自身の状態と照合できるようにする

**設計前提** latest-winsのTransformInputは古いsequenceを破棄できるが、正準状態revision検証を省略しない。Reliable EntityCommandのrevision不一致はErrorMessageまたはResyncRequiredで明示する（ADR-006A）。

### 4.5 実装契約（MEDIUM-006）

**[SPEC]** inbound sequence は接続の認証・入室経路を含む全受信 Envelope に
適用する。`expected=1` から開始し、`sequence == expected` のときだけ期待値を
1 進める。`sequence < expected` は duplicate として drop し、rate limit、
backpressure、mailbox、actor state を変更しない。`sequence > expected` は
`SEQUENCE_GAP` ErrorMessage を返して protocol error とし、接続を終了する。
`u64::MAX` の次は wrap せず `SEQUENCE_OVERFLOW` として reconnect を要求する。
再接続では新しい connection の expected を 1 に戻す（旧接続の最後の値は
resume/revision と混同しない）。

**[SPEC]** sequence の duplicate 判定と envelope/command の構文検証は rate
limit・backpressure・actor dispatch より前に行う。したがって、重複 envelope
や不正 `command_id` は command lane や actor state に到達しない。

**[SPEC]** `EntityCommand.command_id` は UUIDv7 の typed `CommandId` として
transport から `InstanceCommand`、instance actor まで同じ値を渡す。成功した
reliable broadcast/duplicate acknowledgement の payload も client の同じ値を
保持し、新しい ID に置き換えない。

**[SPEC]** command idempotency は instance ごとに最大 4,096 件、完了時刻から
24 時間の bounded store とする。同一 ID・同一 payload は保存済みの成功 event
または deterministic error を再利用し、actor dispatch/broadcast を再実行しない。
同一 ID・異なる payload は `COMMAND_ID_CONFLICT` として fail closed とする。
同時同一 ID は single-flight とし、leader だけが actor に dispatch し、followers
は leader の結果を待って同じ結果を acknowledgement として受け取る。期限切れ
完了エントリは eviction され、容量到達時は新規 command を
`COMMAND_ID_CAPACITY` で拒否する。

**[SPEC]** Dedup outcomes are included in the instance checkpoint codec. The bounded
`RealtimeState` store snapshots outcomes while building a checkpoint and restores them
when the instance is activated. Each instance retains at most 4,096 entries with a
24-hour TTL from completion. After idle reap or process restart, the latest checkpoint
can therefore replay the same result (success event/ack or deterministic error) while
its TTL remains valid. The persisted fingerprint is SHA-256 of the request's canonical
protobuf payload with `command_id` removed, and is intentionally independent from the
replay response payload (which may contain the post-apply revision). The actor checkpoint
and this outcome are saved before a successful command is broadcast/acknowledged; a
storage failure returns a retryable error instead of success. Malformed, expired, future,
duplicate, oversized, or payload-invalid records are rejected during restore.
## 5. 接続ライフサイクルと失敗時状態遷移

### 5.1 接続状態機械

**[REC]** WebSocket接続1本（`RealtimeConnectionId` 1つ）につき次の状態機械を管理する。機械契約の正本は[`contracts/realtime-connection-state-machine.json`](../../contracts/realtime-connection-state-machine.json)であり、本文の次の2表はCIで完全一致を検査する。状態名はlower snake caseを正規名とし、文章中の表示も`active`を用いる。

| machine state | 意味 | 許可される受信wire payload |
|---|---|---|
| `connecting` | TCP/TLS/WS upgrade確立中 | —（HTTP upgrade要求のみ） |
| `awaiting_hello` | WS確立済み、ClientHello待ち | ClientHello |
| `ready` | 認証済み・未入室 | JoinInstance、Heartbeat、ResumeSession |
| `joining` | JoinInstance検証中 | Heartbeat（検証完了まで他は保留） |
| `active` | 入室済み・状態交換中 | TransformInput、EntityCommand、DomainEvent、Heartbeat |
| `resuming` | resume tokenによる復帰処理中 | ResumeSession、Heartbeat |
| `closing` | close frame送信とtransport drain中 | —（送信のみ） |
| `failing` | 致命的ErrorMessage/close frame送信中 | —（送信のみ） |
| `closed` | 正常・回復可能なtransport終端 | — |
| `failed` | 致命的protocol errorによる終端 | — |

| from | event | to |
|---|---|---|
| `connecting` | `upgrade_succeeded` | `awaiting_hello` |
| `connecting` | `upgrade_failed` | `failed` |
| `awaiting_hello` | `hello_accepted` | `ready` |
| `awaiting_hello` | `version_negotiation_failed` | `failing` |
| `awaiting_hello` | `invalid_token` | `failing` |
| `awaiting_hello` | `oversized_message` | `failing` |
| `awaiting_hello` | `persistent_rate_limit` | `failing` |
| `awaiting_hello` | `protocol_abuse` | `failing` |
| `awaiting_hello` | `transport_lost` | `closed` |
| `ready` | `join_requested` | `joining` |
| `ready` | `resume_requested` | `resuming` |
| `ready` | `oversized_message` | `failing` |
| `ready` | `persistent_rate_limit` | `failing` |
| `ready` | `protocol_abuse` | `failing` |
| `ready` | `heartbeat_timeout` | `closed` |
| `ready` | `transport_lost` | `closed` |
| `ready` | `close_requested` | `closing` |
| `joining` | `join_accepted` | `active` |
| `joining` | `join_rejected` | `ready` |
| `joining` | `oversized_message` | `failing` |
| `joining` | `persistent_rate_limit` | `failing` |
| `joining` | `protocol_abuse` | `failing` |
| `joining` | `heartbeat_timeout` | `closed` |
| `joining` | `transport_lost` | `closed` |
| `joining` | `close_requested` | `closing` |
| `active` | `resync_required` | `active` |
| `active` | `oversized_message` | `failing` |
| `active` | `persistent_rate_limit` | `failing` |
| `active` | `protocol_abuse` | `failing` |
| `active` | `heartbeat_timeout` | `closed` |
| `active` | `transport_lost` | `closed` |
| `active` | `close_requested` | `closing` |
| `resuming` | `resume_accepted` | `active` |
| `resuming` | `resync_required` | `ready` |
| `resuming` | `oversized_message` | `failing` |
| `resuming` | `persistent_rate_limit` | `failing` |
| `resuming` | `protocol_abuse` | `failing` |
| `resuming` | `heartbeat_timeout` | `closed` |
| `resuming` | `transport_lost` | `closed` |
| `resuming` | `close_requested` | `closing` |
| `closing` | `graceful_close_completed` | `closed` |
| `closing` | `transport_lost` | `closed` |
| `failing` | `fatal_close_completed` | `failed` |
| `failing` | `transport_lost` | `failed` |

`close_requested`だけが`closing`へ入り、close送信完了またはdrain中の`transport_lost`で`closed`へ終端する。`version_negotiation_failed`、`invalid_token`、`oversized_message`、`persistent_rate_limit`、`protocol_abuse`は`failing`へ入り、fatal close送信完了またはtransport lossで必ず`failed`へ終端する。これによりgraceful/fatalの終端を機械契約から一意に導ける。`heartbeat_timeout`と通常処理中の`transport_lost`は再接続可能な`closed`へ入る。`active → active`の`resync_required`は接続を維持した再同期である。

接続I/O taskは正準ワールド状態を所有しない。`active`はsocketが入室済みであることだけを示し、正準状態は`instance_runtime`が所有する。入室成立と補償はApplication coordinatorが調停する（`architecture.md` §5.2）。

### 5.2 失敗の分類原則

**[REC]** 検証は 2 段階で行う（`state-and-runtime.md` §2.2）。

1. **Transport 層**（`realtime_gateway`）: 構文的検証（サイズ、形式、version、サブプロトコル、rate limit）。失敗は接続レベルのエラーへ直結する。
2. **Domain 層**（`instance_runtime` 等）: 意味的検証（権限、物理的妥当性、状態整合性）。失敗は ErrorMessage として返し、接続は維持する。

**[REC]** この分類が状態遷移を決定する。確立後の致命的Transport失敗は`failing`を経て`failed`へ遷移させる。graceful closeは`closing`を経て`closed`へ入る。upgrade確立前の失敗だけは直接`failed`へ入る。Domain失敗は状態を維持したままErrorMessageを返す。

### 5.3 失敗時状態遷移表

**[REC]** 失敗ごとの検知箇所・応答・遷移先。Transport 由来の表は `transport-boundaries.md` §4.1、Domain 由来の表は `state-and-runtime.md` §2.2, §3.5 と整合させる。

#### Transport 層の失敗（接続レベル）

| 失敗 | 検知箇所 | 応答 | 遷移先 |
|---|---|---|---|
| 未対応サブプロトコル | `realtime_gateway`（upgrade） | HTTP 400 / 接続拒否 | `failed`（確立前） |
| version negotiation失敗 | `realtime_gateway`（hello） | ErrorMessage + close | `awaiting_hello → failing → failed` |
| 不正なProtobuf（decode失敗） | `realtime_gateway` | 単発はErrorMessage、継続はclose | 継続時は`現在状態 → failing → failed` |
| メッセージサイズ超過 | `realtime_gateway` | ErrorMessage + close | `現在状態 → failing → failed` |
| 認証token無効（接続時） | `realtime_gateway` / coordinator | ErrorMessage + close | `awaiting_hello → failing → failed` |
| rate limit超過 | `realtime_gateway` / coordinator | 単発はErrorMessage、継続abuseはclose | 継続時は`現在状態 → failing → failed` |
| Heartbeatタイムアウト | `realtime_gateway` | 検知側で切断 | `ready/joining/active/resuming → closed`。Presenceはresume graceへ |
| transport loss | `realtime_gateway` | 応答不能 | `awaiting_hello/ready/joining/active/resuming → closed`、`closing → closed`、`failing → failed` |

**[SPEC]** サーバーは未対応サブプロトコルを明示的に拒否する（仕様 §13.4）。

**設計前提** ADR-004によりprotocol major不一致は接続拒否、minorはserver対応範囲へnegotiationする。

#### Domain 層の失敗（接続維持）

| 失敗 | 検知箇所 | 応答 | 遷移先 |
|---|---|---|---|
| JoinInstance権限不足 | coordinator / `identity_access`結果 | ErrorMessage | `joining → ready` |
| JoinInstance定員超過 | `world_directory` / coordinator | ErrorMessage | `joining → ready` |
| インスタンス状態が入室不可 | `world_directory` | ErrorMessage | `joining → ready` |
| 所有権違反 | `instance_runtime` | ErrorMessage | `active`（維持） |
| ドメイン検証失敗（NaN、速度、境界） | `instance_runtime` | ErrorMessageまたはclamp | `active`（維持） |
| revision不一致（競合） | `instance_runtime` | ErrorMessage / ResyncRequired | `active → active`（再同期） |

**[SPEC]** NaN、Infinity、範囲外値を拒否する（仕様 §9.7、`state-and-runtime.md` §2.2）。

**[SPEC]** クライアントの自己申告 owner やクライアント種別・自己申告値を権限判定に使ってはならない（仕様 §20.3、§15.2）。

**[REC]** JoinInstanceの拒否は接続を切断しない。クライアントは`ready`から別のインスタンスへJoinInstanceを再試行できる。

### 5.4 隔離

**[SPEC]** 遅いクライアント 1 台がインスタンス全体を遅延させてはならない（仕様 §18.1）。接続単位の不正 frame や遅い受信者がインスタンス全体を停止させない隔離境界を置く（`architecture.md` §6、`state-and-runtime.md` §3.6）。

**[REC]** 1接続の状態遷移・失敗は当該接続のみに閉じる。`active`接続のErrorMessageや切断が、同一インスタンスの他接続や`instance_runtime`の正準状態を破損させてはならない。

## 6. ClientHello / ServerHello

### 6.1 ClientHello の内容

**[SPEC]** ClientHello には次を含める（仕様 §15.2）。

| requirement key | Proto field | 意味 | 用途 |
|---|---|---|---|
| `sdk_name` | `ClientHello.client_name` | クライアントSDKの名称 | 観測・デバッグ |
| `sdk_version` | `ClientHello.client_version` | SDKのバージョン | 観測・互換性参考 |
| `protocol_major` | `Envelope.protocol_major` | 対応protocol major | major一致検査 |
| `protocol_minor` | `ClientHello.supported_minor_min`, `ClientHello.supported_minor_max` | 対応minor範囲 | version negotiation |
| `client_type` | `ClientHello.client_type` | desktop/mobile/server等の種別 | 観測（権限には不使用） |
| `supported_compressions` | `ClientHello.supported_compressions` | 利用可能な圧縮方式の識別子 | 圧縮合意 |
| `supported_features` | `ClientHello.supported_features` | 対応するoptional featureの識別子 | 機能合意 |
| `realtime_ticket` | `ClientHello.realtime_ticket` | 60秒有効・単一使用のopaque token | browser互換の接続認証（ADR-002） |
| `resume_token` | `ClientHello.resume_token` | 任意。再接続時のtokenバインドヒント | 後続のResumeSessionとの取り違え検知 |

**[SPEC]** サーバーはクライアント種別や自己申告値を権限判定に使ってはならない（仕様 §15.2）。これらは観測と互換性参考のみに使用する。

**[REC]** ClientHelloは`awaiting_hello`で受信する最初のapplication messageとする。ClientHello以外のpayloadを`awaiting_hello`で受信した場合はErrorMessageで応答する。

**設計前提** ADR-002によりAccess TokenをWebSocket upgrade、URL query、Cookie、subprotocolへ入れない。ClientHelloで60秒有効・単一使用のRealtime接続ticketを提示し、serverは5秒以内にatomic consumeする。ticket検証前はClientHello以外を処理しない。任意のResume Tokenはticketと別fieldであり、認証を兼ねない。

**[REC]** resume tokenの規範的な機械契約は`contracts/realtime-resume-token-policy.json`とし、次の表をCIで相互照合する。説明文ではなく、contract valueを実装判断に使用する。

| policy key | contract value | 意味 |
|---|---|---|
| `canonical_field` | `ResumeSession.resume_token` | resume要求と認可の正本 |
| `client_hello_role` | `optional_binding_hint` | ClientHello側は任意の取り違え検知用ヒント |
| `comparison` | `constant_time` | 同じ接続の2値を定数時間で比較する |

`tokens_equal`は両方のtokenがある場合だけbooleanとし、それ以外は`null`（比較対象外）とする。次の5行が入力空間全体を構成する完全決定表である。

| case | client_hello_present | resume_session_present | tokens_equal | decision | next_state | error |
|---|---|---|---|---|---|---|
| `client_hello_only` | `true` | `false` | `null` | `do_not_start_resume` | `ready` | `null` |
| `resume_session_only` | `false` | `true` | `null` | `accept_resume_request` | `resuming` | `null` |
| `both_equal` | `true` | `true` | `true` | `accept_resume_request` | `resuming` | `null` |
| `both_mismatch` | `true` | `true` | `false` | `reject_resume` | `ready` | `resume_token_mismatch` |
| `neither` | `false` | `false` | `null` | `do_not_start_resume` | `ready` | `null` |

SDKがClientHelloにもtokenを設定する場合は、ResumeSessionと同じ値を設定する。

### 6.2 ServerHello と negotiation

**[REC]** ServerHelloはClientHello受理後に返す。次の対応表を公開契約とし、ClientHello表とともにCIでProto fieldとの一致を検査する。

| requirement key | Proto field | 意味 |
|---|---|---|
| `protocol_major` | `Envelope.protocol_major` | 合意したprotocol major |
| `protocol_minor` | `ServerHello.negotiated_minor` | 合意したprotocol minor |
| `compression` | `ServerHello.negotiated_compression` | 合意した圧縮方式。v1初期は空文字 |
| `features` | `ServerHello.enabled_features` | 合意したoptional feature識別子 |
| `server_time` | `ServerHello.server_time_unix_ms` | Unix millisecondsのserver時刻 |
| `connection_id` | `ServerHello.connection_id` | 新しいRealtimeConnectionId |
| `heartbeat_interval` | `ServerHello.heartbeat_interval_ms` | 合意したheartbeat間隔 |

**[SPEC]** 合意できないサブプロトコルは明示的に拒否する（仕様 §13.4）。

**設計前提** ADR-004によりv1初期はWebSocket圧縮を無効とする。したがってclientは`supported_compressions`を空配列、serverは`negotiated_compression`を空文字としてよい。将来permessage-deflateをopt-in追加するときもWebSocket extension negotiationを正本とし、このfieldは合意結果の観測・互換性確認に使う。

**設計前提** ADR-004により未知fieldは無視し、minorは互換範囲へnegotiationし、major不一致は拒否する。

## 7. Snapshot

### 7.1 Snapshot の内容

**[SPEC]** 入室時に、クライアントが必要とする初期状態を送る（仕様 §15.3）。

| 項目 | 意味 | 所有モジュール |
|---|---|---|
| 自分のユーザー情報 | 当該クライアントのユーザー情報 | `identity_access`（主体情報） |
| インスタンス情報 | 入室先インスタンスのメタ情報 | `world_directory` |
| 自分の権限 | 当該インスタンスでの権限 | `identity_access` / coordinator |
| 周辺エンティティ | Interest 適用後のエンティティ | `instance_runtime` |
| 周辺ユーザー | Interest 適用後のユーザー | `instance_runtime` / `realtime_presence` |
| 現在 revision | 差分適用の基準点（§4.4） | `instance_runtime` |
| サーバー時刻 | Unix milliseconds | `realtime_gateway` / clock |

**[SPEC]** 1000 人全員分を無条件に送らず、Interest Management 適用後の状態を送る（仕様 §15.3、`architecture.md` §5.2）。

**[REC]** Join 時 Snapshot の生成と配信は次の順序で行う（`architecture.md` §5.2, §5.3 のフロー）：

1. `instance_runtime` が read model（候補となるエンティティ・プレゼンスの状態集合）を生成する。
2. coordinator が joining subject の視点と候補集合を `interest` へ渡し、`interest` は当該 subject に対する可視エンティティ集合と可視プレゼンス集合を純粋計算する。`interest` は I/O を呼ばず、socket・queue・gateway に依存しない（`architecture.md` §3）。
3. coordinator が Snapshot をその可視集合へ絞り込む（filtered snapshot）。
4. coordinator が filtered snapshot を `realtime_delivery` へ渡し、当該 `RealtimeConnectionId` 1 件へ配信する（§3.3 の直列化経路）。

`instance_runtime` は `interest` や `realtime_delivery` を直接呼ばない。Join 時 `interest` の計算対象は「メッセージの受信者集合」ではなく「joining subject に見える状態集合」である。

### 7.2 Snapshot の配信

**[SPEC]** スナップショットは分割送信可能とする（仕様 §14.4）。

**[REC]** Snapshot がメッセージ上限（§8）を超える場合、複数 chunk に分割して送る。各 chunk は同一の Snapshot 論理メッセージに属することを示す識別子と、chunk 順序・終端情報を持つ。

**[REC]** Snapshot 全体は Reliable クラスとして扱い、分割 chunk の欠落は再送または ResyncRequired で回復する。latest-wins 集約で chunk を破棄しない。

**設計前提** Snapshot chunkは通常message上限16KiB以下、同一logical IDとchunk index/countを持ち、Reliable bulk laneで順序配送する（ADR-004）。

**[REC]** Snapshot配信の完了をもって`active`へ遷移する（§5.1）。JoinAccepted送信後、Snapshot配信前に接続が切断された場合の補償（presence取消等）はcoordinatorが調停する（`architecture.md` §5.2）。

## 8. メッセージ上限

### 8.1 上限値

**[SPEC]** 初期推奨値は仕様 §14.4 に従い、`transport-boundaries.md` §5 で一元管理する。本書は値を再定義しない。

| 種別 | 上限 |
|---|---|
| 通常リアルタイムメッセージ | 16 KiB 以下 |
| カスタムイベント | 64 KiB 以下 |
| スナップショット | 分割送信可能 |

**[SPEC]** 圧縮後・展開後の両方に上限を設定する（仕様 §14.4）。

**[SPEC]** 1 接続あたり毎秒メッセージ数を制限する（仕様 §14.4）。

**[SPEC]** 巨大データやアセットを WebSocket へ流してはならない（仕様 §14.4）。

### 8.2 payload 種別への割当

**[REC]** 各 payload に適用する上限の割当：

| payload | 適用上限 | 超過時動作 |
|---|---|---|
| TransformInput、StateDelta、Heartbeat 等 | 16 KiB | ErrorMessage + 切断（§5.3） |
| DomainEvent（custom） | 64 KiB | ErrorMessage + 切断 |
| EntityCommand | 16 KiB（custom payload を含む場合 64 KiB） | ErrorMessage + 切断 |
| Snapshot | chunk 単位で 16 KiB 以下、全体は分割 | chunk 分割（§7.2） |
| ClientHello / ServerHello | 16 KiB | ErrorMessage + 切断 |

**[REC]** 毎秒メッセージ数の初期推奨は `transport-boundaries.md` TB-06（通常 100 msg/s、カスタムイベント 10 msg/s）を参照する。具体値は負荷試験で調整する（TB-06、`transport-boundaries.md` §4.4）。

**設計前提** v1初期は圧縮無効とし、decode前frameとdecode後messageの双方へTB書§5の上限を適用する（ADR-004）。

## 9. 要 ADR 事項

本書で提示した [ADR] のうち、本書が主担当となる判断を RP ID で管理する。他文書が正本の判断（ADR-004、ADR-006A、TB-04、TB-05、TB-06 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| RP-01 | sequence の方向モデル | Accepted: 接続ごとに send/recv の独立カウンタ | ADR-006A |
| RP-02 | 再接続時 replay の sequence 扱い | Accepted: 新接続で採番し直し、dedup は message_id | ADR-006A |
| RP-03 | StateDelta と EntityCommand の責務分離 | reliable な状態変化は StateDelta に載せない | latest-wins 集約による reliable イベント破棄を防止 |
| RP-04 | JoinInstance拒否時の接続扱い | 切断せず`ready`へ戻す | 別インスタンス再試行を許容。domain失敗は接続維持（§5.2） |
| RP-05 | Snapshot chunk の分割方式 | Accepted: 同一snapshot ID + chunk index/count | ADR-004と公開Proto |

Envelope の field 集合・番号は [SPEC] として確定済みであり（§2.3）、その将来変更規則は ADR-004 が正本であるため、本書の RP ID では管理しない。
