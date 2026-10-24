# Changelog

本ファイルは [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) の形式に従い、versioningは `release-maintenance-license.md` §1 のsemantic versioning方針に従います。

公開release前のため、まだtagは存在しません。

## [Unreleased]

### Added

- 全認証方式のIP・方式別発行制限、同時発行上限、一時主体の発行予算と429再試行目安
- 自分と管理対象ユーザーのセッション一覧、端末表示名、個別失効・現在以外の失効。失効後は既存WebSocketも停止
- Presenceの所有不整合、猶予・回収待ち、永続化の待ち時間、Resume失敗理由を運用画面・メトリクスに追加し、Prometheusアラート例とテストを配布
- TypeScript SDKの汎用EntityCommand確定待ち・結果不明の追跡と、同一ID・要求内容による明示的な再送
- 監査ログの複合検索、フィルターに束縛された精密カーソル、秘密情報の除去とCSV式対策を施したページ単位のJSON・CSV出力
- 設定済みローカルJWKSの検証・監査・原子的な再読み込み。既存の鍵は不正なファイルや監査障害で置換しない
- 参照ConsoleとwhiteboardのWorld・Instance追加取得、接続先・検索条件変更時のリセットと競合対策
- 上記運用機能にはmigration 0024～0026が必要。既存の監査カーソルは更新後に検索の先頭から取得し直す

Milestone 1 以降で追加された機能。詳細は [`docs/design/roadmap-and-traceability.md`](docs/design/roadmap-and-traceability.md) §3.1 の受入マトリクスを参照。

- 認証と識別: Argon2id パスワード検証、Ed25519 署名 access token、opaque refresh token の発行・ローテーション・再利用検知、ログイン失敗によるロックアウト
- ユーザーとロール管理: 最初の管理者の作成（`bootstrap-admin`）、ユーザー作成・参照・一覧・更新・有効化・無効化・一括投入、ロールの作成・参照・一覧・更新・削除・割り当て、RBAC による権限判定
- 監査ログ: 管理操作とイベントの記録、REST からの参照、保持期間の運用手順
- World / Instance 管理: REST での一覧・作成・参照・更新・archive・start・stop・kick・members、PostgreSQL 永続化
- Realtime: WebSocket handshake、60 秒寿命の realtime ticket、protocol v1、instance への join / leave、Transform 同期と速度・距離の検証、snapshot と state delta の配信
- Interest Management: 均一グリッド、ヒステリシス付き購読、可視性ポリシー
- Entity: `EntityCommand` 経由の生成・更新・削除。所有権と revision を検証し、結果を reliable イベントとして可視性判定を通して配信
- 永続エンティティ: `persistent_entities` と `persistent_entity_components` テーブル（migration `0016` / `0017`）。spawn / delete / コンポーネント更新のたびに行として保存し、instance の起動時に読み戻す
- Checkpoint: 定期保存・アイドル回収時・シャットダウン時の保存、復元時の厳密な検証と fail-closed、ペイロード上限とその超過の分類・計測、保存の同時実行数の上限
- 切断からの Resume と送信集約（backpressure）: 単一使用の resume token、履歴不足時の `ResyncRequired`、詰まった受信者への位置の間引きと reliable イベントの非間引き
- 受信 sequence と command id の重複排除
- Extension webhook: 登録、outbox、配送、terminal state の原子的更新、遅い webhook による直列化の解消
- 可観測性: `/metrics`、`/health/live`、`/health/ready`、`/version`、structured logging
- TypeScript SDK: Envelope の encode / decode、再接続と resume、送信キュー
- 運用: バックアップと復元訓練（`scripts/restore-drill.sh`）、incident response、secret rotation、migration recovery、graceful shutdown の各 Runbook
- 受入条件のトレーサビリティ表: 全 11 設計文書 163 項目とテストの対応（[`docs/design/acceptance-traceability/`](docs/design/acceptance-traceability/)）

### Fixed

- SDKの正常切断後に同期状態がreadyのまま残り、未確定コマンドを送信できた問題を修正
- JWT暗号処理をAWS-LCへ切り替えて旧RustCrypto依存とRSAの監査除外を削除し、Tokio・WebPKI・ルート証明書を脆弱性修正版へ更新
- ログイン確定とパスワード再設定・ユーザー無効化の競合、非ローカル主体の管理操作、Argon2設定変更後の再ハッシュ判定を修正
- SDKの認証方式切替と管理Consoleの接続先変更時に、古い非同期応答が認証情報を上書きする問題を修正
- Resume token失効後のPresence回収、旧接続の終了処理、一時主体失効後のイベント認可、複数EntityのInterest購読維持を修正
- Entity・監査・外部outbox保存を安定したIDで再試行し、保存待ちのInstanceが他Instanceの定期処理を停止させないよう修正。migration `0023_runtime_effect_receipts.sql` が必要
- 白板の遠隔更新で編集中の本文・フォーカスを失う問題と、共有計測値を使う並列テストの不安定性を修正
- クラッシュや強制終了の際に、直前の checkpoint 以降のエンティティ変更が復元されなかった問題。行テーブルへは毎 tick 書き込まれていたが起動時に読み戻していなかったため、最大 `world.checkpoint_interval_secs`（既定 300 秒）ぶんが失われていた
- 信頼プロキシの設定値が CIDR ではなくリテラル IP 文字列として扱われていた問題
- webhook の SSRF 検証が実際の接続先アドレスに紐づいていなかった問題
- TypeScript SDK が失敗した socket を残し、古い socket のイベントが現在の接続に影響していた問題

### Changed

- CI（`ci.yml`、`dependency-audit.yml`）を `workflow_dispatch` のみに変更。GitHub Actions の無料枠を使い切ったための運用上の判断であり、push や schedule では起動しない

- Cargo workspaceと15 crateの構成（`repo-crate-conventions.md` §1.2、§2.2）
- 共通基盤: UUIDv7 ID newtype、`Timestamp` と `Clock` trait、domain/application/config error型、設定のsource・precedence・検証、JSON structured logging
- `proto/` からのbuild時コード生成（pure Rust compiler、生成コードはcommitしない）
- PostgreSQL migration基盤（`sqlx migrate`、`orbisync-server migrate`、PostgreSQL 16/17 CI matrix）。スキーマ本体はMilestone 1
- operational HTTP endpoint: `/health/live`、`/health/ready`、`/version`
- Docker開発環境（`deploy/compose/compose.dev.yml`、`deploy/docker/Dockerfile`、`scripts/bootstrap-dev.sh`）
- PR CI: format、clippy、documentation build、unit/contract test、依存規則検査、cargo-deny、OpenAPI検証、PostgreSQL 16/17 integration
- `scripts/check_architecture.py`（Allowed Dependency Matrix、循環依存、framework混入、testkitのdev-dependency限定）
- `scripts/validate_openapi.py`（OpenAPI 3.1 schema検証）
