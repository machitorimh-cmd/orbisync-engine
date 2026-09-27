# OrbiSync 拡張機構設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §23（拡張機構）を、out-of-process 拡張の方針、manifest/capability、イベント配送と outbox、Webhook 要件、サンドボックスと failure isolation の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `EX-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| モジュール所有権、DAG、外部イベント配送フロー | `architecture.md` §3, §3.1, §5.5 | 前提として参照。配送フローを再定義しない |
| 内部イベント配送（typed event / outbox） | `architecture.md` ARC-04 | 前提として参照。確定は ARC-04 へ委ねる |
| ドメインイベントの所有と発行 | `domain-model.md` §5 | 前提として参照。イベントは所有モジュールのみ発行 |
| `extension_registrations` テーブル所有権 | `rest-api-persistence.md` §8 | 前提として参照 |
| 拡張 API（Webhook/REST、認証、timeout、retry、idempotency） | ADR-007 | Acceptedな設計前提として参照する |
| 観測・監査、PII redaction | ADR-008、`technology-decisions.md` TD-08, TD-09 | 前提として参照 |

## 1. 所有権と前提

**設計前提** 外部サービスとの認証、配送、タイムアウト、再試行は `extension_gateway` が所有する。Out-of-process Extension のみ（仕様 §23、`architecture.md` §3）。

**設計前提** 汎用イベントの内部配布と外部配送要求は `eventing` が所有する。internal event publisher、outbox request を公開し、ドメインを逆参照しない（`architecture.md` §3）。

**設計前提** domain/application event は所有モジュールのみが発行する（`domain-model.md` §5、`architecture.md` §3.2）。

**[SPEC]** 第三者のネイティブ動的ライブラリをコアプロセスへ直接ロードしない（仕様 §23.1）。Out-of-process Extension を採用する（仕様 §23.2）。

## 2. out-of-process のみ

**[SPEC]** 第三者のネイティブ動的ライブラリをコアプロセスへ直接ロードしない（仕様 §23.1）。理由：

- ABI 互換性
- メモリ安全性
- コア全体のクラッシュ
- 依存衝突
- セキュリティ境界の不明確化
- アップデート困難化

**[SPEC]** 初期版の拡張方式は Out-of-process Extension を採用する（仕様 §23.2）：

```text
OrbiSync
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

**[REC]** コアは拡張をプロセス外の実体として扱い、公開契約（Webhook、Extension Command API、scoped token）を通じてのみ相互作用する。コアの内部型・repository・DB へ拡張から直接アクセスさせない。

## 3. Extension manifest と capability

**所有モジュール:** `extension_gateway`

**[REC]** 拡張は `extension_registrations` テーブルへ登録する（`rest-api-persistence.md` §8、仕様 §22.1）。登録情報（manifest）：

| 項目 | 意味 |
|---|---|
| extension_id | 拡張の一意 ID |
| name / description | 表示情報 |
| endpoint | Webhook 配送先 URL |
| subscribed_events | 購読するイベント種別（§5） |
| capabilities | 許可する操作集合（§3.1） |
| token scopes | scoped service token の権限範囲 |
| status | active / suspended |
| signing_secret_ref | HMAC 署名鍵の参照（secret 本体は保持しない） |

### 3.1 capability model

**[REC]** capability は拡張がアクセスできるイベントとコマンドを限定する最小権限の集合とする。例：

- `events:read` + 購読イベント種別の限定
- `commands:entity:read` / `commands:audit:read` 等のコマンド_scope_
- 管理操作を拡張へ許可しない（管理は `http_api` + `auth-authorization.md` の認可経路のみ）

**[REC]** scoped service token は拡張ごとに発行し、manifest の scope に紐づける。token は拡張の認証にのみ使用し、ユーザーの `AuthSessionId` と混同しない（`auth-authorization.md` §5、`domain-model.md` §3.6）。

**設計前提** ADR-007により30日有効・scope付き256-bit opaque Extension token、HMAC digest保存、rotation可能とする。

### 3.2 署名鍵の管理

**[REC]** Webhook 署名鍵（HMAC secret）は `extension_gateway` が管理し、平文をログ・manifest 応答へ出さない（仕様 §26.4、TD-08）。鍵の参照（`signing_secret_ref`）のみを manifest に保持する。

**[ADR]** 署名鍵の生成・rotation・保存（secret store の要否）は EX-02 で決める。配備環境の secret 管理方針に依存するため固定しない。

## 4. イベント配送

### 4.1 内部イベントと outbox

**設計前提** 内部イベント配送は同一プロセス typed event、外部配送のみ durable outbox を推奨する（`architecture.md` ARC-04）。

**[REC]** 配送フロー（`architecture.md` §5.5 と整合）：

```text
domain/application event
  → public event mapping
  → transactional outbox（推奨）
  → extension delivery worker
  → timeout/retry/idempotency handling
  → success/failure telemetry and audit where required
```

**設計前提** イベントは既に成立した事実であり、所有モジュールのみが発行する（`domain-model.md` §5）。`extension_gateway` はイベントを発行せず、配送のみを担う。

**設計前提** 拡張登録前に発生したイベントは backfill しない。拡張へ配送されるのは、登録時点以降に発生したイベントのみである。

**[REC]** 公開イベントは内部ドメインイベントから mapping して生成する。内部イベントの内部フィールドをそのまま外部へ露出せず、公開契約として安定した形式へ変換する。

**[REC]** 外部配送を domain 状態遷移の local transaction に含めず、outbox へ書き込んで非同期に配送する（`architecture.md` §5.5、`rest-api-persistence.md` §10）。

### 4.2 イベント種別

**[SPEC]** 仕様 §23.3 は次のイベント名を例示する。これは確定済みの公開イベント集合ではない。

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

**[REC]** 上記の仕様例を初期公開イベント集合の候補として採用する。イベント種別は `<aggregate>.<action>` の命名とする。公開イベントschemaは公開契約として安定させ、正確な初期集合とversioningはEX-03で確定する。ADR-007により配送方式はoutbound Webhook、管理方式はRESTへ確定済みである。

**[ADR]** 公開イベントの schema 管理、versioning、購読の粒度（種別単位 vs ワイルドカード）は EX-03 で決める。

## 5. Webhook 要件

**[SPEC]** Webhook は次を満たす（仕様 §23.4）：

- HMAC 署名
- timestamp
- event ID
- 再送
- exponential backoff
- dead-letter 記録
- 重複配信を前提
- 受信側の idempotency
- タイムアウト
- 宛先ごとの遮断機構

**[REC]** 各要件の設計：

| 要件 | 設計 |
|---|---|
| HMAC 署名 | 配送 payload を拡張ごとの signing secret で HMAC 署名し header へ付与 |
| timestamp | 配送時刻を payload/header に含め、replay 窓の判定に使用する |
| event ID | イベントごとに一意 ID を付与。dedup と相関に使用する |
| 再送 | 失敗時に再送する。at-least-once を前提とする |
| exponential backoff | 再送間隔は指数バックオフ（jitter 推奨） |
| dead-letter 記録 | 再試行上限到達後に DLQ へ記録し、手動再送/調査を可能にする |
| 重複配信を前提 | at-least-once 配送。同一 event ID が複数回届きうる |
| 受信側の idempotency | 受信側が event ID で dedup することを契約として要求する |
| タイムアウト | 配送に timeout を設定し、遅延応答を失敗として扱う |
| 宛先ごとの遮断機構 | 宛先ごとに circuit breaker を持ち、連続失敗時に一時遮断する |

**[REC]** 配送は at-least-once とする。exactly-once は保証せず、受信側 idempotency で重複を吸収する契約にする。

**設計前提** ADR-007によりtimeout 5秒、retry 5回、full-jitter 1〜30秒、5回連続失敗で30秒circuit open、DLQ 7日とする。配送済み outbox 行の既定保持は1日（`retention.extension_outbox_days`）とする。失敗配送の調査窓はDLQの7日保持で担保するため、outboxで同じ期間を重複保持しない。outboxは配送機構であり監査ログではないため、監査はaudit logが担う。pending行は期間経過だけでは削除しない。

## 6. サンドボックスと failure isolation

**[SPEC]** 第三者のネイティブ動的ライブラリをコアプロセスへ直接ロードせず、初期版は out-of-process Extension を採用する（仕様 §23.1、§23.2）。

**[REC]** プロセス分離により拡張とコアはメモリ空間・ABI を共有せず、拡張プロセス単体のクラッシュがコアへ直接伝播する経路を閉じる。ただし、ネットワーク枯渇、接続数、CPU/メモリなどの共有資源を介した影響までは防げないため、timeout、bounded queue、rate limit、circuit breaker で別途隔離する。

**[REC]** failure isolation の設計（`architecture.md` §6 と整合）：

- 外部拡張への同期呼び出しを instance runtime の状態遷移クリティカルパスへ置かない（`architecture.md` §5.5）
- 拡張配送は独立した delivery worker が担い、timeout/retry/DLQ で隔離する
- 外部拡張、監査 exporter、telemetry exporter の停止を instance runtime へ無制限に伝播させない（`architecture.md` §6）
- 1 拡張の遅延・失敗が他拡張の配送をブロックしない（宛先ごとの worker/queue）

**[REC]** 拡張コマンド API（Extension Command API、仕様 §23.2）は、拡張からコアへの要求を受け付ける公開面である。要求は `extension_gateway` が認証（scoped token）し、認可（capability）したうえで application use case へ渡す。拡張が直接 `instance_runtime` や repository を呼ぶ経路を設けない（DAG 遵守、`architecture.md` §3.1）。

**設計前提** 拡張配送の失敗は正準状態の更新を失敗にしない。状態遷移と外部配送は outbox で分離する（§4.1、`architecture.md` §5.5）。

## 7. 将来の WASM

**[SPEC]** サンドボックス化された WASM プラグインは将来候補とする。初期版では次が未確定のため必須にしない（仕様 §23.5）：

- ABI
- capability model
- CPU/memory 制限
- 非同期 I/O
- 永続ストレージ
- バージョン互換性

**[REC]** 初期版は WASM を実装しない。out-of-process Extension のみを公開契約とする。WASM の採用は具体ユースケースと上記未確定項目の解決後に ADR で再評価する（仕様 §41、`technology-decisions.md` §8）。

**[ADR]** WASM の採用可否と sandbox 設計は、必要性が確認された場合に将来 ADR で決める。本書は判断を保留する。

## 8. 脅威・失敗時挙動

**[REC]** 本書が対象とする脅威と対策：

| 脅威 | 対策 | 根拠 |
|---|---|---|
| 偽造 callback / 改ざん | HMAC 署名 + timestamp | 仕様 §23.4、§5 |
| replay 攻撃 | timestamp による replay 窓 + event ID dedup | 仕様 §23.4、§5 |
| 拡張の権限超過 | scoped service token + capability 最小権限 | §3.1、仕様 §23.2 |
| 拡張からのデータ持ち出し | 公開イベントのみ配送、内部フィールド非露出 | §4.1 |
| 拡張の DoS / 遅延 | timeout、circuit breaker、DLQ、宛先ごと隔離 | 仕様 §23.4、§6 |
| 拡張クラッシュのコア伝播 | out-of-process、同期呼び出しを critical path に置かない | 仕様 §23.1、§6 |
| 署名鍵漏洩 | secret をログ/manifest へ出さない、rotation | 仕様 §26.4、§3.2 |

**[REC]** 失敗時挙動：

| 失敗 | 対応 | 状態影響 |
|---|---|---|
| 配送 timeout / 5xx | exponential backoff で再送 | 正準状態に影響なし |
| 再試行上限到達 | DLQ へ記録、配送停止 | 正準状態に影響なし |
| 宛先の連続失敗 | circuit breaker で一時遮断 | 他宛先の配送は継続 |
| scoped token 無効 | 拡張コマンドを拒否（401/403 相当） | なし |
| capability 超過 | コマンドを拒否 | なし |

**設計前提** 外部拡張の停止を instance runtime へ無制限に伝播させない（`architecture.md` §6）。配送失敗は正準状態の更新を失敗にしない（`architecture.md` §5.5）。

## 9. 監査・観測

**[REC]** 拡張配送の成否は telemetry と（必要に応じて）監査へ記録する（`architecture.md` §5.5）。

**[SPEC]** 最低限のメトリクスは仕様 §27.2 の確定要件に従う（TD-09）。

**[REC]** 本書が拡張配送向けに追加する観測項目：

| メトリクス | 種別 | 関連節 |
|---|---|---|
| `extension_delivery_attempts_total{result}` | counter | §5 |
| `extension_delivery_duration_seconds` | histogram | §5 |
| `extension_dlq_total` | counter | §5 |
| `extension_circuit_breaker_trips_total` | counter | §5, §6 |

**[REC]** 配送ログには event ID、extension_id、result、duration を含める。signing secret と token はログしない（仕様 §26.4、TD-08）。

**設計前提** UserId 等の高 cardinality 値を metric label にしない（TD-09、`mobile-resume-interest-backpressure.md` §9.1）。`result` 等の低 cardinality 区分のみ label とする。

## 10. テスト可能な受入条件

**[REC]** 実装は次の受入条件をテストで示す。delivery worker と clock は fake/決定論的実装を用いる（`architecture.md` §9、仕様 §31.4）。

1. コアプロセスは第三者のネイティブ動的ライブラリをロードせず、拡張は out-of-process のみである。
2. ドメインイベント発行時に公開イベントが mapping され、outbox へ書き込まれる。状態遷移の local transaction に外部配送が含まれない。
3. Webhook 配送は HMAC 署名・timestamp・event ID を含み、署名検証を受信側で再現できる。
4. 配送失敗時に exponential backoff で再送し、再試行上限到達後に DLQ へ記録する。
5. 同一 event ID の配送が重複しうる（at-least-once）。受信側が event ID で dedup できる契約である。
6. 1 宛先が連続失敗しても circuit breaker が当該宛先のみを遮断し、他宛先の配送は継続する。
7. 拡張配送の遅延・失敗が instance runtime の tick と正準状態更新を遅延・失敗させない。
8. scoped token のみを持つ拡張は、capability 外のコマンドを拒否される。管理操作を実行できない。
9. 拡張コマンドは `extension_gateway` の認証・認可を経て application use case へ渡り、拡張から repository/instance_runtime への直接呼出し経路が存在しない。
10. signing secret と scoped token がログ・メトリクス・manifest 応答に含まれない。
11. WASM プラグインの経路が存在しない（初期版は out-of-process のみ）。

## 11. 要 ADR 事項

本書が主担当となる判断を EX ID で管理する。他文書が正本の判断（ADR-007、ADR-008、ARC-04 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| EX-01 | capability 粒度と scoped token の形式・寿命・rotation | 最小権限の scope 集合、token は有限寿命 + rotation | 仕様 §23.2。ADR-007 と整合 |
| EX-02 | 署名鍵の生成・rotation・保存 | server 管理、rotation 可能、secret store は配備依存 | 仕様 §23.4, §26.4。配備環境に依存 |
| EX-03 | 公開イベント schema の管理と versioning | `<aggregate>.<action>`、破壊的変更は versioning | 公開契約の安定性 |
| EX-04 | 再試行/backoff/timeout/circuit breaker/DLQ の具体値 | 再試行上限、backoff 初期/上限、timeout、DLQ 保持 | 仕様 §23.4。具体値は負荷試験で調整 |

拡張 API の確定（Webhook/REST/gRPC、認証、timeout、retry、idempotency）は ADR-007、内部イベント配送方式は ARC-04、観測・監査の詳細は ADR-008 が正本であるため、本書の EX ID では管理しない。WASM の採用可否は必要性確認後の将来 ADR へ保留する。

## 12. 状態確定前の検証フック（第 3 の相互作用方向、ADR-025）

**[ADR]** ADR-025 は、§2 が定める 2 方向（Core→Extension の Webhook 配送、Extension→Core の Extension Command API）に加え、3 つ目の方向「Core→Extension 同期問い合わせ→Core 確定」を追加する。本節は既存の §2〜§11 を変更せず、追補として参照する。`generalization-and-llm-app-platform.md` §2.2 が指摘した欠落（Core を確定前に外部ルールへ問い合わせる経路がない）への対応であり、実装計画は `docs/plans/generalization-2.2-pre-commit-validation-hook.md` を参照する。

**[REC]** 役割の違い：

| 経路 | 方向 | タイミング | 状態への影響 |
|---|---|---|---|
| Webhook（§5） | Core→Extension | 状態確定**後**の事実通知 | なし（事後通知） |
| Extension Command API（§6） | Extension→Core | Extension が能動的に要求 | application use case 経由で状態変更しうる |
| 状態確定前フック（本節） | Core→Extension（同期問い合わせ）→Core | 状態確定**前**、`instance_runtime` の command 入口へ到達する前 | Extension の deny が commit を拒否する。allow は追加の許可を与えない（Core の既存検証の AND 条件） |

**[REC]** 対象は entity mutation コマンド（spawn/update/delete 相当）に限定し、高頻度の位置ストリーム（transform update）は対象としない。同期呼び出しを instance runtime のクリティカルパスへ置かない現行方針（§6）と両立させるため、フック呼び出しは instance runtime の command 入口（mailbox）へ到達する前の transport/coordinator 層で行い、actor 内では実行しない。

**[REC]** オプトインとし、対象コマンド種別を購読する Active な Extension registration が存在しない限り、フックは呼ばれない（既定は無効、既存挙動を維持）。capability（§3.1）に `hooks:entity:spawn` / `hooks:entity:update` / `hooks:entity:delete` を追加し、購読の粒度とする。

**[REC]** 権限分離：RBAC・所有権・数値検証などの Core 不変条件（`state-and-runtime.md` §2.2）はフックの前後を問わず Core が専有する。フックは Core が許可した操作に対する追加の拒否権のみを持ち、Core が拒否する操作を許可へ変える権限を持たない。

**[REC]** timeout・再送・競合の既定動作（ADR-025 で確定）：

| 事象 | 既定動作 |
|---|---|
| timeout（既定 500ms、Webhook 配送の 5 秒とは別値） | 拒否、状態変更なし |
| 非 2xx 応答 / 接続失敗 / 応答パース不能 | 拒否、状態変更なし |
| フック問い合わせ中に対象 entity の revision が進行 | 再判定しない。フック allow 後も actor の既存 `expected_revision` 比較がそのまま適用される |
| 同一 `command_id` の重複/再送 | 新規の重複排除を作らず、既存の command dedup（Leader/Duplicate 判定）を再利用し、Leader のみがフックを呼ぶ |

**[ADR]** ADR-025 は endpoint を `extension_registrations` の登録情報のみから解決し、クライアントが自由指定できないことを確定する。ADR-007 の egress policy（loopback/private/link-local/metadata/reserved 拒否）をそのまま再利用する。

### 12.1 Core観測状態の提供（2026-09-14）

署名要求の`current_entity`はCore観測値（対象不在ならnull）で、`revision`, `owner_id`, `components`を持つ。クライアント要求`payload`と区別する。Update/Deleteは観測revisionと要求revisionが一致し、Core所有権/権限がある場合のみ外部へ送る。actorは確定時に観測Entity全体と現在状態の一致を検証してから元コマンドを適用する。状態変更・削除・同ID再生成が間に入れば古いallowを拒否する。対象外Entityや外部DBの依存関係は含まない。詳細と互換性はADR-025「revisionとCore観測状態の束縛」を参照。
