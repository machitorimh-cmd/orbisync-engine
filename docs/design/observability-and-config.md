# OrbiSync 観測可能性と設定設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §27（観測可能性）と §28（設定仕様）を、structured logging、metrics、tracing、audit の cardinality/secret 規則、設定の source/precedence/validation/reload/secret 分離の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `OC-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| モジュール所有権、`audit_observability` の責務 | `architecture.md` §3 | 前提として参照。所有権を再定義しない |
| `tracing` crate、Prometheus 互換メトリクス | `technology-decisions.md` TD-08, TD-09 | 前提として参照。技術選定を再定義しない |
| 構造化ログ必須フィールド、cardinality 制約 | `technology-decisions.md` TD-08, TD-09 | 前提として参照。本書は規則を具体化 |
| 機密情報除去（パスワード、token、payload） | `technology-decisions.md` TD-08、仕様 §26.4 | 前提として参照。本書は secret 規則を具体化 |
| メトリクス最低要件 | `technology-decisions.md` TD-09、仕様 §27.2 | 前提として参照。本書は追加項目と cardinality 規則を定義 |
| 監査ログ、保持期間、改ざん対策 | ADR-008 | Acceptedな設計前提として参照する |
| metrics endpoint の認証/公開範囲 | ADR-009 | Acceptedな設計前提として参照する |
| Resume Token のログ記録禁止 | `mobile-resume-interest-backpressure.md` §3.2, §9.2 | 前提として参照 |
| 設定例（TOML）、階層、環境変数 | 仕様 §28 | 本書は設定の設計を具体化 |
| 負荷試験の測定条件 | `scale-and-nfr.md` §9 | 相互参照 |

## 1. 所有権と前提

**設計前提** 監査記録 port、ログ/メトリクス/トレース adapter は `audit_observability` が所有する（`architecture.md` §3）。

**設計前提** telemetry facade は `audit_observability` が提供し、他モジュールは facade を通じて観測機能を呼び出す。各モジュールが直接 tracing/metrics adapter を構築しない。

**設計前提** 外部拡張、監査 exporter、telemetry exporter の停止を instance runtime へ無制限に伝播させない（`architecture.md` §6）。

## 2. Structured Logging

### 2.1 ログ形式

**[SPEC]** JSON 形式を標準とする（仕様 §27.1）。

**設計前提** `tracing` crate を使用する（TD-08）。非同期処理を span と相関 ID で追跡し、ログと trace の文脈を統合する。

**[REC]** ログ出力の設定：

| 設定項目 | 初期値 | 説明 |
|---|---|---|
| `log_format` | `json` | `json` または `pretty`（開発時のみ） |
| `log_level` | `info` | `trace` / `debug` / `info` / `warn` / `error` |
| `audit_retention_days` | `365` | 1..3650 days; operator-only archive and bounded purge. The runtime never deletes |
| `log_output` | `stdout` | `stdout` / `stderr` / ファイルパス |

**[REC]** 本番環境では `json` 形式を必須とする。`pretty` 形式は開発時のデバッグ用に限定する。

### 2.2 必須フィールド

**[SPEC]** 仕様 §27.1 が定める構造化ログの必須フィールド：

| フィールド | 型 | 意味 | 常時出力 |
|---|---|---|---|
| `timestamp` | RFC 3339 | イベント時刻（UTC） | はい |
| `level` | string | ログレベル | はい |
| `service` | string | サービス名（`orbisync`） | はい |
| `version` | string | ビルドバージョン | はい |
| `request_id` | string | REST リクエストの一意 ID | REST のみ |
| `connection_id` | string | WebSocket 接続の一意 ID | WebSocket のみ |
| `session_id` | string | Identity Session ID（必要時に匿名化） | 必要時 |
| `instance_id` | string | ワールドインスタンス ID | instance scope のみ |
| `user_id` | string | ユーザー ID（必要時に匿名化） | 必要時 |
| `event` | string | イベント名 | はい |
| `duration_ms` | number | 処理時間（ms） | 処理完了時 |
| `error_code` | string | エラーコード | エラー時のみ |

**設計前提** ADR-014 により、HTTP middleware が全 REST request に `req_<canonical UUIDv7>` 形式の `request_id` を server 側で採番する。inbound `X-Request-Id` は authoritative ID として受理しない。同じ ID を request span と全 log record へ伝搬し、response の `X-Request-Id` header、および error body と監査ログの `request_id` に使用する。

**[REC]** `session_id` と `user_id` の匿名化は、ログの用途に応じて行う。監査ログでは実 ID を記録し、デバッグログでは匿名化してよい。匿名化の方式は OC-01 で決める。

**[ADR]** 匿名化の方式（hash、mask、pseudonymization）と、監査ログとデバッグログの分離方針は OC-01 で決める。

### 2.3 Secret 規則

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4）。

**設計前提** cookie、authorization header、不要な個人情報も記録しない（TD-08）。

**[REC]** ログ記録禁止項目：

| 禁止項目 | 理由 |
|---|---|
| パスワード（平文・ハッシュ） | 資格情報の漏洩 |
| Access Token / Refresh Token / Realtime接続ticket / Resume Token（raw・prefix） | 認証・接続・復帰情報の漏洩 |
| Resume Token（raw・prefix） | 復帰材料の漏洩（MRIB 書 §3.2） |
| WebSocket / REST の完全な payload | 機密データ・PII の漏洩 |
| cookie / authorization header | 認証情報の漏洩 |
| DB 接続文字列（パスワード含む） | 資格情報の漏洩 |
| 内部スタックトレース | 内部構造の漏洩 |

**[REC]** ログ記録許可項目：

| 許可項目 | 条件 |
|---|---|
| request_id / connection_id | 相関用。機密情報ではない |
| instance_id / entity_id | 相関用。機密情報ではない |
| user_id | 監査ログでは実 ID、デバッグログでは匿名化 |
| error_code | 機械可読。内部詳細を含まない |
| duration_ms | 性能測定。機密情報ではない |
| メッセージサイズ（バイト数） | 性能測定。payload 内容を含まない |
| HTTP method / path / status | REST 相関用。query parameter の機密値は除外 |

**[REC]** token の相関が必要な場合、鍵付き hash（HMAC）または server 側 opaque correlation ID のみを用いる。平文ハッシュは token を復元可能な参照子になりうるため、鍵付きでない hash は用いない（MRIB 書 §3.2）。

### 2.4 ログレベルの基準

**[REC]** ログレベルの使い分け：

| レベル | 用途 | 例 |
|---|---|---|
| `error` | 処理の失敗。運用者の対応が必要 | DB 接続失敗、panic 検知 |
| `warn` | 異常だが処理は継続。監視対象 | slow consumer 検知、rate limit 超過、resume 失敗 |
| `info` | 正常な業務イベント | 接続確立、入室、退室、認証成功 |
| `debug` | 開発・デバッグ用の詳細情報 | command 処理の詳細、Interest 計算の結果 |
| `trace` | 最も詳細な追跡情報 | 個々のメッセージの送受信、queue の状態 |

**[REC]** 本番環境のデフォルトは `info` とする。`debug` / `trace` は問題調査時に一時的に有効化する。

## 3. Metrics

### 3.1 最低限のメトリクス

**[SPEC]** 仕様 §27.2 が定める最低限のメトリクス：

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

**設計前提** Prometheus 互換メトリクスを使用する（TD-09）。pull 型監視へ接続でき、特定 SaaS を必須にしない。

### 3.2 追加メトリクス

**[REC]** 本書が仕様 §27.2 に追加するメトリクス。既存設計文書で定義済みの追加項目（MRIB 書 §9.1）は再定義せず参照する：

| メトリクス | 種別 | 関連節 | 正本 |
|---|---|---|---|
| `resume_failed_total{reason}` | counter | 再接続失敗 | MRIB 書 §9.1 |
| `resync_full_total` | counter | 全再同期 | MRIB 書 §9.1 |
| `delta_replay_total` | counter | 差分 replay | MRIB 書 §9.1 |
| `heartbeat_timeout_total` | counter | heartbeat タイムアウト | MRIB 書 §9.1 |
| `reliable_queue_overflow_total` | counter | reliable queue 超過 | MRIB 書 §9.1 |
| `slow_consumer_disconnect_total` | counter | slow consumer 切断 | MRIB 書 §9.1 |
| `rate_limit_rejected_total{scope}` | counter | rate limit 拒否 | MRIB 書 §9.1 |
| `interest_visible_set_size` | histogram | 可視集合サイズ | MRIB 書 §9.1 |
| `tick_duration_seconds` | histogram | tick 処理時間 | 本書 §3.3 |
| `interest_calc_duration_seconds` | histogram | Interest 計算時間 | 本書 §3.3 |
| `config_reload_total{result}` | counter | 設定リロード結果 | 本書 §6.3 |
| `audit_events_total` | counter | 監査イベント数 | 本書 §5 |
| `extension_delivery_total{result}` | counter | 拡張配送結果 | 本書 §3.3 |
| `extension_outbox_deleted_total` | counter | retentionで削除した配送済みoutbox行数 | 本書 §6.5 |
| `extension_outbox_pending` | gauge | 保留中の拡張outbox行数 | 本書 §6.5 |
| `outbound_queue_depth` | gauge | Current outbound queue depth | MRIB ?8.2 |
| `outbound_queue_depth_max` | gauge | Process-lifetime outbound queue high-water mark | MRIB ?8.2 |
| `outbound_queue_bytes` | gauge | Current queued payload bytes | MRIB ?8.2 |
| `outbound_queue_bytes_max` | gauge | Process-lifetime queued payload bytes high-water mark | MRIB ?8.2 |

### 3.3 Cardinality 規則

**設計前提** UserId、ConnectionId、WorldInstanceId など高 cardinality 値を metric label にしない（TD-09 の設計制約）。

**[REC]** label の cardinality 規則：

| 許可される label | cardinality | 例 |
|---|---|---|
| `method` | 低（~10） | GET, POST, PATCH, DELETE |
| `status` | 低（~20） | 200, 400, 401, 403, 404, 429, 500 |
| `reason` | 低（~10） | expired, gap, epoch, rejected |
| `scope` | 低（~5） | connection, user, ip, instance |
| `result` | 低（~5） | success, failure, timeout |
| `instance_id` | **禁止** | 高 cardinality |
| `user_id` | **禁止** | 高 cardinality |
| `connection_id` | **禁止** | 高 cardinality |
| `entity_id` | **禁止** | 高 cardinality |

**[REC]** histogram の bucket は用途に応じて設計する。デフォルトの Prometheus bucket（.005, .01, .025, .05, .1, .25, .5, 1, 2.5, 5, 10）を基に、レイテンシ用とサイズ用で分けてよい。

**設計前提** metrics endpointはADR-009によりlocalhost/internal network限定とする。histogram bucketはOC-02の初期値を使用する。

### 3.5 Protected operational diagnostics

`GET /v1/admin/diagnostics` is a human-oriented summary layered on top of the
existing metrics and durable database state; it does not replace Prometheus.
The endpoint requires `admin.diagnostics.read` and is deliberately separate
from unauthenticated liveness/readiness. It exposes only aggregate counts,
timestamps, bounded queue observations and configured capacities. Credentials,
payloads, and high-cardinality identifiers are forbidden. Retention success
timestamps are process-local, so `null` after restart means "not yet observed
by this process", not proof that cleanup has never run.

### 3.4 Metrics の Secret 規則

**[REC]** metric name と label に機密情報を含めない。metric の値（カウント、合計、histogram）は集計済みであり、個別の機密データを含まない。

**[REC]** metric の help text に機密情報を含めない。

## 4. Tracing

### 4.1 Span の対象

**[SPEC]** 仕様 §27.3 が定める tracing の対象：

| 対象 | span の種類 | 所有モジュール |
|---|---|---|
| REST request | server span | `http_api` |
| WebSocket handshake | server span | `realtime_gateway` |
| join flow | 複合 span（coordinator） | Application coordinator |
| snapshot generation | internal span | `instance_runtime` |
| extension delivery | client span | `extension_gateway` |
| DB query | client span | `persistence` |

**[SPEC]** すべての transform update を個別 span 化すると負荷が高いため、sampling または集約を使用する（仕様 §27.3）。

### 4.2 Sampling 方針

**[REC]** transform update の tracing は sampling で制御する：

| span 種別 | sampling 方針 |
|---|---|
| REST request | 全件（低頻度） |
| WebSocket handshake | 全件（接続時のみ） |
| join flow | 全件（入室時のみ） |
| snapshot generation | 全件（入室/resync 時のみ） |
| transform update | sampling（例: 1%）または集約（tick 単位） |
| extension delivery | 全件（低頻度） |
| DB query | 全件（低頻度）または slow query のみ |

**設計前提** ADR-008によりOTLPを既定無効、error/管理100%、通常REST 10%、transform 1% samplingとする。

### 4.3 Trace の Secret 規則

**[REC]** span attribute に機密情報を含めない。§2.3 のログ記録禁止項目は trace にも適用する。

**[REC]** span attribute に許可されるもの：

| 許可項目 | 例 |
|---|---|
| request_id / connection_id | 相関用 |
| instance_id / entity_id | 相関用（trace のみ。metric label には不可） |
| HTTP method / path / status | REST 相関用 |
| duration_ms | 性能測定 |
| error_code | エラー相関用 |
| メッセージサイズ（バイト数） | 性能測定 |

**[REC]** trace の instance_id / entity_id は metric label とは異なり、個別 span の attribute として許容する。trace は sampling 済みであり、metric のような cardinality 爆発のリスクが低い。ただし、trace storage の容量に注意する。

## 5. Audit

### 5.1 監査対象

**[SPEC]** 全管理操作を監査ログとして記録する（仕様 §26.3）。

**設計前提** 監査記録 port は `audit_observability` が所有する（`architecture.md` §3）。

**[REC]** 監査対象のイベント：

| カテゴリ | イベント例 |
|---|---|
| 認証 | ログイン成功/失敗、ログアウト、token 更新、パスワード変更 |
| ユーザー管理 | 作成、有効化/無効化、パスワードリセット、CSV インポート |
| ロール管理 | 作成、変更、削除、ユーザーへの割当 |
| ワールド管理 | 作成、変更、アーカイブ |
| インスタンス管理 | 作成、開始、停止、kick |
| リアルタイム | 入室、退室、kick、所有権移譲 |
| 拡張管理 | 登録、変更、削除 |
| 運用 | 設定変更、graceful shutdown |

### 5.2 監査ログの形式

**[REC]** 監査ログは構造化ログとは分離した sink へ記録する。形式：

| フィールド | 型 | 意味 |
|---|---|---|
| `audit_id` | string | 監査イベントの一意 ID |
| `timestamp` | RFC 3339 | イベント時刻（UTC） |
| `actor_type` | string | 操作主体の種別（user / admin / system） |
| `actor_id` | string | 操作主体の ID |
| `action` | string | 操作種別（機械可読） |
| `resource_type` | string | 対象リソースの種別 |
| `resource_id` | string | 対象リソースの ID |
| `result` | string | 結果（success / failure） |
| `error_code` | string | 失敗時のエラーコード |
| `request_id` | string | 相関用のリクエスト ID |
| `details` | object | 追加情報（機密情報除外） |

**[REC]** 監査ログには実 ID を記録する（匿名化しない）。監査の目的は操作の追跡であり、匿名化すると追跡不能になる。

**設計前提** Milestone 1 の監査ログ実装は ADR-014 に従い、HTTP middleware が採番した server-generated `request_id` を保存する。

### 5.3 監査ログの保持と Secret

**[SPEC]** 監査ログ保持期間を設定可能にする（仕様 §26.4）。

**[SPEC]** ログへパスワード、token、完全な payload を出さない（仕様 §26.4）。監査ログの `details` にも適用する。

**[REC]** 監査ログの `details` に含めてよいもの：

| 許可 | 禁止 |
|---|---|
| 変更されたフィールド名 | 変更前のパスワード（平文・ハッシュ） |
| 変更後の値（機密以外） | token（raw・prefix） |
| IP アドレス（保持期間設定可能） | 完全な request/response payload |
| 操作のパラメータ（機密以外） | DB 接続文字列 |

**設計前提** ADR-008によりPostgreSQL append-only audit table、runtime roleのUPDATE/DELETE禁止、既定保持1年とする。

**設計前提** ADR-017により認証・権限変更はapplication所有の用途限定portをstorage-postgresが実装し、identity mutationとaudit insertを同じlocal transactionで行う。一般化したcross-module Unit of Workは導入しない。

## 6. 設定

### 6.1 設定の Source と Precedence

**[SPEC]** 仕様 §28.3 が定める設定の階層（優先順位）：

1. CLI 引数
2. 環境変数
3. 設定ファイル
4. 安全なデフォルト値

**[REC]** 各 source の役割：

| Source | 用途 | 例 |
|---|---|---|
| CLI 引数 | 一時的な上書き、デバッグ | `--bind 0.0.0.0:9090` |
| 環境変数 | コンテナ/オーケストレータとの統合、secret | `DATABASE_URL`, `ORBISYNC_SERVER_BIND` |
| 設定ファイル | 永続的な設定、バージョン管理 | `orbisync.toml` |
| デフォルト値 | 安全な初期値 | `bind = "0.0.0.0:8080"` |

**[REC]** 環境変数の命名規則は `ORBISYNC_<SECTION>_<KEY>` とする。例: `ORBISYNC_SERVER_BIND`, `ORBISYNC_DATABASE_MAX_CONNECTIONS`, `ORBISYNC_REALTIME_HEARTBEAT_INTERVAL_SECONDS`。

**[REC]** 設定ファイルの形式は TOML を標準とする（仕様 §28.2 の例）。YAML は MAY で提供してよいが、初期版では TOML のみとする。

**[ADR]** YAML のサポート、設定ファイルの探索パス（`./orbisync.toml`, `/etc/orbisync/orbisync.toml` 等）は OC-05 で決める。

### 6.2 設定の検証

**[SPEC]** 起動時に全設定を検証する（仕様 §28.1）。

**[SPEC]** 不明な設定キーは警告またはエラーにする（仕様 §28.1）。

**[REC]** 検証の段階：

| 段階 | 検証内容 | 失敗時の挙動 |
|---|---|---|
| 構文検証 | TOML の構文、型 | 起動を中止し、エラーを出力 |
| 未知キー検証 | 定義されていないキーの検出 | 警告（デフォルト）またはエラー（strict mode） |
| 値域検証 | 数値の範囲、文字列の形式 | 起動を中止し、エラーを出力 |
| 相互依存検証 | 矛盾する設定の検出 | 起動を中止し、エラーを出力 |
| secret 検証 | secret の存在確認（値は検証しない） | 起動を中止し、エラーを出力 |

**[REC]** 値域検証の例：

| 設定キー | 検証 |
|---|---|
| `server.bind` | 有効なソケットアドレス |
| `server.request_timeout_seconds` | 1 以上。通常 HTTP リクエストのボディ読み込みとハンドラ実行を合わせた上限（既定 30 秒） |
| `database.max_connections` | 1 以上 |
| `auth.access_token_ttl_seconds` | 60 以上 86400 以下 |
| `auth.password_min_length` | 8 以上 |
| `realtime.heartbeat_interval_seconds` | 5 以上 120 以下 |
| `realtime.resume_prune_interval_seconds` | 0 より大きく `world.resume_grace_seconds` 未満 |
| `realtime.max_message_bytes` | 1024 以上 1048576 以下 |
| `world.server_tick_hz` | 1 以上 60 以下 |
| `interest.cell_size` | 0 より大きい |
| `interest.near_radius` | 0 より大きい |
| `interest.unsubscribe_radius` | `near_radius` より大きい |

**[REC]** 検証エラーは人間向けメッセージで出力する。どのキーが、どの検証に、なぜ失敗したかを示す。

### 6.3 設定のリロード

**[REC]** 初期版では設定のリロードをサポートしない。設定変更は再起動で行う。

**[REC]** 将来のリロード対応のため、設定の読み取りは起動時に 1 回のみ行い、実行中は読み取り専用の設定オブジェクトを参照する。実行中の設定変更は ADR で検討する。

**[ADR]** リロードの要否、対象キー（heartbeat 間隔、rate limit 閾値等）、リロードのトリガー（SIGHUP、API、ファイル監視）は OC-06 で決める。

**[REC]** リロードをサポートしない初期版でも、設定変更の監査ログを記録する（§5.1）。

### 6.4 Secret の分離

**[SPEC]** secrets と通常設定を分ける（仕様 §28.1）。

**[REC]** secret の定義：

| Secret | 例 | 注入方法 |
|---|---|---|
| DB 接続文字列 | `DATABASE_URL` | 環境変数のみ |
| Access Token署名鍵 | Ed25519 private key（公開鍵は`kid`付きkey set） | secret manager、または権限制限したファイルパス |
| Argon2id pepper | pepper 値 | 環境変数（任意） |
| Webhook 署名鍵 | HMAC secret | 環境変数またはファイルパス |
| TLS 証明書/鍵 | cert/key ファイル | ファイルパス（環境変数で指定） |

**[REC]** secret は設定ファイルに直接記述しない。環境変数またはファイルパスで参照する。設定ファイルには secret の参照先のみを記述する：

```toml
[database]
url_env = "DATABASE_URL"  # 環境変数名を指定。値は環境変数から取得

[auth]
token_signing_key_env = "ORBISYNC_TOKEN_SIGNING_KEY"  # 環境変数名を指定
pagination_hmac_key_env = "ORBISYNC_PAGINATION_HMAC_KEY"  # 環境変数名を指定
```

**[REC]** secret の検証は、起動時に環境変数またはファイルの存在確認のみ行う。値の内容（長さ、形式）は検証してよいが、値自体をログへ出力しない。

**[REC]** secret のファイルパス指定は、ファイルのパーミッション（owner のみ読み取り可能）を検証してよい。

### 6.5 設定例

仕様 §28.2 は設定例（TOML）を示している。以下は仕様例の構造を継承しつつ、本書の設計（secret 分離、observability 設定の追加）を反映した推奨設定例である。

**[REC]** 本書の推奨設定例：

```toml
[server]
bind = "0.0.0.0:8080"
worker_threads = 0
max_request_body_bytes = 2097152
request_timeout_seconds = 30

[database]
url_env = "DATABASE_URL"
max_connections = 20
acquire_timeout_seconds = 2
readiness_timeout_seconds = 3

[auth]
access_token_ttl_seconds = 900
refresh_token_ttl_seconds = 2592000
password_min_length = 12
argon2_memory_cost_kib = 65536
argon2_iterations = 3
argon2_parallelism = 1
login_failure_threshold = 5
lockout_duration_seconds = 900
token_signing_key_env = "ORBISYNC_TOKEN_SIGNING_KEY"
pagination_hmac_key_env = "ORBISYNC_PAGINATION_HMAC_KEY"

[rate_limit]
normal_per_sec = 100
custom_per_sec = 10
persistent_threshold = 5
max_buckets = 10000

[realtime]
heartbeat_interval_seconds = 20
connection_timeout_seconds = 60
max_message_bytes = 65536
outbound_queue_capacity = 256
per_connection_capacity = 256
resume_prune_interval_seconds = 15

[world]
default_capacity = 100
server_tick_hz = 10
resume_grace_seconds = 60
checkpoint_interval_ticks = 300
history_capacity = 256
max_speed = 50.0
max_acceleration = 10.0
speed_acceleration_check_enabled = true

[identity]
csv_max_bytes = 1048576
csv_max_rows = 1000

[retention]
realtime_ticket_interval_seconds = 60
idempotency_interval_seconds = 3600
drain_budget_seconds = 20
restart_backoff_seconds = 5
extension_outbox_days = 1

[interest]
cell_size = 20.0
near_radius = 30.0
unsubscribe_radius = 35.0

[observability]
log_format = "json"
log_level = "info"
audit_retention_days = 365
```

`observability.audit_retention_days` は監査ログ本体の保持ポリシーを宣言
する設定である。ADR-008 の append-only と runtime role の権限境界を維持
するため、retention worker は `audit_events` を削除しない。実際の削除は
運用手順の責務とし、パーティションの切り離しまたは外部アーカイブ後の
運用削除で実施する。

**[REC]** 上記の値は初期推奨値であり、負荷試験で調整する。各値の根拠は対応する設計文書を参照する。

| 設定キー | 根拠 |
|---|---|
| `heartbeat_interval_seconds = 20` | MRIB 書 §6.3 / MRIB-04（15〜30 秒の範囲内） |
| `connection_timeout_seconds = 60` | MRIB 書 §6.3 / MRIB-04（45〜90 秒の範囲内） |
| `server_tick_hz = 10` | `state-and-runtime.md` §3.3 / SR-04（10〜20 Hz の範囲内） |
| `resume_grace_seconds = 60` | `domain-model.md` DM-07 / MRIB 書 §3.4 |
| `resume_prune_interval_seconds = 15` | `resume_grace_seconds` 未満で期限切れ binding を定期回収 |
| `near_radius = 30.0` | MRIB 書 §7.5 / MRIB-07 |
| `unsubscribe_radius = 35.0` | MRIB 書 §7.5 / MRIB-07 |
| `cell_size = 20.0` | MRIB 書 §7.2 / MRIB-05 |
| `outbound_queue_capacity = 256` | MRIB 書 §8.2 / MRIB-08 |
| `extension_outbox_days = 1` | DLQの7日保持が失敗配送の調査窓を担保するため、配送済みoutboxの重複保持を避ける |
| `audit_retention_days = 365` | ADR-008の既定1年。append-only監査本体の削除はruntimeでは行わず、運用のパーティション/アーカイブ手順で実施する |
| `request_timeout_seconds = 30` | 通常HTTPのボディ読み込みとハンドラ実行を合わせた全体deadline。既存の30秒ボディdeadlineとの運用互換を維持する既定値 |
| `speed_acceleration_check_enabled = true` | ADR-024。既定は既存動作（常時検証）を維持する後方互換優先。所有権・revision・距離・数値の有限性はこのキーの値によらず常に検証される |

### 6.6 デフォルト値の原則

**[SPEC]** 安全なデフォルト値を使用する（仕様 §28.3）。

**[REC]** デフォルト値の原則：

| 原則 | 例 |
|---|---|
| セキュリティ優先 | TLS 必須、CORS デフォルト拒否、rate limit 有効 |
| 保守的な性能 | tick 10 Hz、queue 容量は中程度 |
| 明示的な無効化 | JSON debug mode 無効、tracing sampling 低め |
| secret のデフォルトなし | secret にデフォルト値を設定しない。未設定なら起動失敗 |

## 7. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 7.1 Logging

1. 本番モード（`log_format = "json"`）のログ出力が有効な JSON であり、必須フィールド（§2.2）をすべて含む。
2. ログ出力にパスワード、token（raw・prefix）、完全な payload が含まれない。
3. `log_level = "warn"` の場合、`info` 以下のログが出力されない。

### 7.2 Metrics

4. 仕様 §27.2 の最低限のメトリクスがすべて `/metrics` endpoint に出力される。
5. metric label に高 cardinality 値（instance_id、user_id、connection_id、entity_id）が含まれない。
6. histogram の bucket が設定され、`_bucket` / `_sum` / `_count` が出力される。

### 7.3 Tracing

7. REST request、WebSocket handshake、join flow、snapshot generation、extension delivery、DB query の各 span が出力される。
8. transform update の span が sampling され、全件が出力されない（sampling rate < 1.0 の場合）。
9. span attribute に機密情報（パスワード、token）が含まれない。

### 7.4 Audit

10. 全管理操作（ユーザー作成、ロール割当、インスタンス開始/停止、kick 等）の監査ログが記録される。
11. 監査ログの `details` にパスワード、token、完全な payload が含まれない。
12. 監査ログに実 ID（actor_id、resource_id）が記録される。

### 7.5 設定

13. 起動時に全設定が検証され、不正な値（範囲外、型不一致）で起動が中止される。
14. 不明な設定キーが警告またはエラーとして報告される。
15. secret の環境変数が未設定の場合、起動が中止される。
16. 設定ファイルに secret の値が直接記述されていない（参照先のみ）。
17. CLI 引数 > 環境変数 > 設定ファイル > デフォルトの優先順位が守られる。

## 8. 要 ADR 事項

本書が主担当となる判断を OC ID で管理する。他文書が正本の判断（ADR-008、ADR-009、MRIB-04、SR-03 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| OC-01 | ログの匿名化方式と、監査ログ/デバッグログの分離 | 監査ログは実 ID、デバッグログは HMAC また mask | 仕様 §26.4 のプライバシー要件と監査の追跡性の均衡 |
| OC-02 | histogram bucket の具体値、metrics endpoint の認証/公開範囲 | レイテンシ用とサイズ用で bucket を分ける。metrics は認証付きまたは内部ネットワーク限定 | TD-09 の cardinality 制約。ADR-009 と整合 |
| OC-03 | trace exporter、sampling rate、集約方式 | OTLP exporter、transform は 1% sampling、他は全件 | 仕様 §27.3 の負荷配慮。ADR-008 と整合 |
| OC-04 | 監査ログの保持期間、改ざん対策、保存先 | DB テーブル（append-only）、保持期間は設定可能、初期値 1 年 | 仕様 §26.4。ADR-008 と整合 |
| OC-05 | YAML サポート、設定ファイルの探索パス | 初期版は TOML のみ。探索は `./orbisync.toml` → `/etc/orbisync/orbisync.toml` | 仕様 §28.1 は YAML を MAY とする。初期は TOML に限定 |
| OC-06 | 設定リロードの要否と対象キー | 初期版はリロードなし。再起動で対応。将来は SIGHUP または API で一部キーをリロード可能 | 初期の複雑性回避。運用要望に基づき ADR |

## Audit retention operational enforcement

`observability.audit_retention_days` is an operational input, not a log-only
value. The daily `systemd` timer described in
[`docs/operations/audit-retention.md`](../operations/audit-retention.md) passes
this validated value to `scripts/audit-retention.sh`, which computes a
PostgreSQL-clock cutoff, exports eligible `audit_events`, verifies the JSONL
row count and SHA-256, records the archive marker, and invokes the separate
`orbisync_audit_maintenance` role in bounded batches. The application runtime
never performs this deletion and keeps its append-only grants.

This policy is distinct from `retention.source_ip_days`: source-IP cleanup
removes only `audit_source_ips` rows and does not implement audit-event
retention. Both jobs must be monitored independently. A missing archive marker,
manifest mismatch, lock timeout, or changed cutoff count fails closed; the
scheduler alerts on any non-zero exit or a missing successful run for 26 hours.


### LOW-002 retention contract

`observability.audit_retention_days` is validated as an integer in `1..=3650`.
The operator script accepts `--batch-size` only in `1..=10000` and passes the
validated value to migration `0014_audit_retention_operator.sql`. The migration
keeps `orbisync_runtime` at `SELECT, INSERT` on `audit_events`; only the
separately provisioned `orbisync_audit_maintenance` membership can execute the
`SECURITY DEFINER` retention functions. No direct DELETE grant or runtime role
escalation is permitted.

Each purge call takes the transaction-scoped
`pg_advisory_xact_lock`, validates the archive manifest, and deletes one
bounded `ORDER BY occurred_at, id` batch selected with `FOR UPDATE SKIP LOCKED`.
A failed statement aborts that transaction, including the marker update, so
partial batches are rolled back. The script supports `--dry-run`/`--count-only`
without archive or purge mutation and reports cutoff, eligible count/oldest,
and after execution remaining count/oldest.
