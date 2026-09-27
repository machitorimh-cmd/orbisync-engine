# OrbiSync ロードマップ・MVP トレーサビリティ・設計索引

## 0. 表記規則

本書は `metaverse_core_specification.md` §39（初期ロードマップ）、§40（MVP 完了条件）、§41（将来検討事項）、§42（未決定事項）を、既存設計文書へ統合する実行計画と設計トレーサビリティとして具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `RM-xx` で管理する。

### 0.1 本書の位置づけ

本書は実装プロンプトやコード生成の指示書ではない。設計文書の索引、仕様章のカバレッジ、マイルストンの入口/出口条件、MVP 受入のトレーサビリティ、将来候補と ADR の接続、未決事項のマッピングを提供する。

## 1. 設計文書索引

### 1.1 docs/design 索引

| # | ファイル | 対象仕様章 | 主な内容 |
|---|---|---|---|
| 1 | `system-context.md` | §1, §3, §4, §5, §6, §7 | システム境界、責務、非目標 |
| 2 | `architecture.md` | §3, §7 | レイヤー、モジュール境界、DAG、実行モデル、主要フロー |
| 3 | `technology-decisions.md` | §8 | 技術選定（Rust/Tokio/Axum/Protobuf/PostgreSQL 等）、ADR バックログ |
| 4 | `domain-model.md` | §9 | 集約、Entity、Value Object、ID 体系 |
| 5 | `state-and-runtime.md` | §10, §11, §12 | 状態分類、サーバー権威、Actor/mailbox/tick |
| 6 | `transport-boundaries.md` | §13 | REST/WebSocket 責務分離、DTO 変換、メッセージ制限 |
| 7 | `realtime-protocol-and-connection.md` | §14, §15 | Envelope、メッセージ分類、sequence/revision、接続状態機械 |
| 8 | `mobile-resume-interest-backpressure.md` | §16, §17, §18 | 再接続、Resume Token、Interest、バックプレッシャー |
| 9 | `auth-authorization.md` | §19, §20 | 認証、RBAC、権限、所有権モデル |
| 10 | `rest-api-persistence.md` | §21, §22 | REST 資源、テーブル所有権、migration |
| 11 | `extension-mechanism.md` | §23 | out-of-process 拡張、Webhook、outbox |
| 12 | `client-sdk.md` | §24 | SDK 接続/再接続/serialize/状態適用/error 契約 |
| 13 | `scale-and-nfr.md` | §25, §26 | 段階目標、容量モデル、NFR 測定/SLO、failure isolation |
| 14 | `observability-and-config.md` | §27, §28 | metrics/log/tracing/audit、設定 source/precedence/secret |
| 15 | `repo-crate-conventions.md` | §29, §30, §31, §43, §45 | workspace/crate 構成、依存 matrix、コーディング規約、ADR テンプレート、開発者起動体験 |
| 16 | `test-and-ci.md` | §32, §33, §44 | テスト戦略、CI stages/release gates、PR チェックリスト |
| 17 | `release-maintenance-license.md` | §34, §35, §36 | versioning/deprecation、OSS メンテナンス、ライセンス候補 |
| 18 | `deployment-and-threat-model.md` | §37, §38 | デプロイ/graceful shutdown/backup、脅威モデル |
| 19 | `roadmap-and-traceability.md`（本書） | §39, §40, §41, §42 | ロードマップ、MVP、将来候補、未決事項、索引 |

### 1.2 仕様章カバレッジ Matrix

| 仕様章 | タイトル | 設計文書 | 状態 |
|---|---|---|---|
| 仕様 §1 | 文書の目的 | `system-context.md` | 設計前提として参照 |
| 仕様 §2 | 仕様用語 | — | 定義規定。設計文書不要 |
| 仕様 §3 | プロジェクトの基本方針 | `system-context.md`、`architecture.md` | 設計前提として参照 |
| 仕様 §4 | スコープ | `system-context.md` | 設計前提として参照 |
| 仕様 §5 | 目標と非目標 | `system-context.md` | 設計前提として参照 |
| 仕様 §6 | システムコンテキスト | `system-context.md` | 設計前提として参照 |
| 仕様 §7 | アーキテクチャ原則 | `architecture.md` | 具体化済み |
| 仕様 §8 | 推奨技術スタック | `technology-decisions.md` | 具体化済み |
| 仕様 §9 | ドメインモデル | `domain-model.md` | 具体化済み |
| 仕様 §10 | 状態分類 | `state-and-runtime.md` | 具体化済み |
| 仕様 §11 | サーバー権威 | `state-and-runtime.md` | 具体化済み |
| 仕様 §12 | インスタンス実行モデル | `state-and-runtime.md` | 具体化済み |
| 仕様 §13 | 通信方式 | `transport-boundaries.md` | 具体化済み |
| 仕様 §14 | リアルタイムプロトコル | `realtime-protocol-and-connection.md` | 具体化済み |
| 仕様 §15 | 接続フロー | `realtime-protocol-and-connection.md` | 具体化済み |
| 仕様 §16 | モバイル回線対応 | `mobile-resume-interest-backpressure.md` | 具体化済み |
| 仕様 §17 | Interest Management | `mobile-resume-interest-backpressure.md` | 具体化済み |
| 仕様 §18 | バックプレッシャー | `mobile-resume-interest-backpressure.md` | 具体化済み |
| 仕様 §19 | 認証仕様 | `auth-authorization.md` | 具体化済み |
| 仕様 §20 | 認可仕様 | `auth-authorization.md` | 具体化済み |
| 仕様 §21 | REST API 概要 | `rest-api-persistence.md` | 具体化済み |
| 仕様 §22 | データベース設計案 | `rest-api-persistence.md` | 具体化済み |
| 仕様 §23 | 拡張機構 | `extension-mechanism.md` | 具体化済み |
| 仕様 §24 | Client SDK 仕様 | `client-sdk.md` | 具体化済み |
| 仕様 §25 | 1000 人規模への設計 | `scale-and-nfr.md` | 具体化済み |
| 仕様 §26 | 非機能要件 | `scale-and-nfr.md` | 具体化済み |
| 仕様 §27 | 観測可能性 | `observability-and-config.md` | 具体化済み |
| 仕様 §28 | 設定仕様 | `observability-and-config.md` | 具体化済み |
| 仕様 §29 | リポジトリ構成 | `repo-crate-conventions.md` | 具体化済み |
| 仕様 §30 | Crate 責務と依存規則 | `repo-crate-conventions.md` | 具体化済み |
| 仕様 §31 | コーディング規約 | `repo-crate-conventions.md` | 具体化済み |
| 仕様 §32 | テスト戦略 | `test-and-ci.md` | 具体化済み |
| 仕様 §33 | CI/CD | `test-and-ci.md` | 具体化済み |
| 仕様 §34 | リリースと互換性 | `release-maintenance-license.md` | 具体化済み |
| 仕様 §35 | OSS メンテナンス方針 | `release-maintenance-license.md` | 具体化済み |
| 仕様 §36 | ライセンス案 | `release-maintenance-license.md` | 決定済み（RL-05: MIT OR Apache-2.0） |
| 仕様 §37 | デプロイ仕様 | `deployment-and-threat-model.md` | 具体化済み（topologyはADR-009でAccepted） |
| 仕様 §38 | 脅威モデル概要 | `deployment-and-threat-model.md` | 具体化済み |
| 仕様 §39 | 初期ロードマップ | 本書 §2 | 具体化済み |
| 仕様 §40 | MVP 完了条件 | 本書 §3 | 具体化済み |
| 仕様 §41 | 将来検討事項 | 本書 §4 | 非コミットメントとして ADR trigger へ接続 |
| 仕様 §42 | 未決定事項 | 本書 §5 | 既存 ADR ID へマッピング済み |
| 仕様 §43 | ADR テンプレート | `repo-crate-conventions.md` §9 | 仕様例を[REC]として統合済み |
| 仕様 §44 | PR チェックリスト案 | `test-and-ci.md` §5 | 仕様例を[REC]として統合済み |
| 仕様 §45 | 開発者向け最短起動体験 | `repo-crate-conventions.md` §10 | 仕様例を[REC]として統合済み |
| 仕様 §46 | 参考となる公式仕様 | — | 参考リンク。規範の正本ではなく設計文書不要 |
| 仕様 §47 | 最終要約 | — | 新規要件ではない要約規定。設計文書不要 |

## 2. 初期ロードマップ

### 2.1 Phase 定義

**[SPEC]** 仕様 §39 が定める Milestone 0〜6 と各 Milestone の成果物内容は確定したロードマップ基準である。本書はこれに入口/出口条件・依存関係・検証を付加して具体化する。

**[REC]** 仕様 §39 が規定しない期間・日付・Milestone 内部タスクの厳密順序は、計画時に決定する。成果物と検証条件を定義し、期間は見積もらない。

#### Milestone 0: Repository Foundation

| 項目 | 内容 |
|---|---|
| 成果物 | Cargo Workspace、CI、lint/format、基本文書、ADR、Docker 開発環境 |
| 入口条件 | プロジェクト名・識別子はADR-001、ライセンスはRL-05で確定済み。Proto payload集合と本文wire payload表、JSON状態遷移集合と本文状態遷移表、SDK状態表とMRIB再接続図、Resume Token Policy JSONと本文policy表の自動対照検査が成功する |
| 出口条件 | `cargo build` / `cargo fmt --check` / `cargo clippy` が pass。CI が PR で実行される。基本文書（README, CONTRIBUTING, SECURITY 等）がリポジトリに存在する |
| 依存 | なし（最初の Milestone） |
| 検証 | `repo-crate-conventions.md` §8 の受入条件1〜3、8〜11。`test-and-ci.md` §4.5の受入条件12。`python scripts/validate_design.py --review`でpayload集合、server状態・event・transition集合、SDK/MRIB状態、resume policyの本文対照と到達性を検査 |
| 関連設計 | `repo-crate-conventions.md`、`test-and-ci.md`、`release-maintenance-license.md` §2.1 |

#### Milestone 1: Identity and Administration

| 項目 | 内容 |
|---|---|
| 成果物 | ローカル認証、ユーザー管理、ロール/権限、PostgreSQL migration、REST API、監査ログ |
| 入口条件 | Milestone 0完了。ADR-002、ADR-003、ADR-005はAccepted済み |
| 出口条件 | 管理者がユーザーを作成・有効化/無効化できる。ログイン/ログアウト/token 更新が動作する。RBAC の権限チェックが機能する。監査ログが記録される |
| 依存 | Milestone 0 |
| 検証 | `auth-authorization.md` の受入条件。`rest-api-persistence.md` の受入条件。`observability-and-config.md` §7.4 の受入条件 10〜12 |
| 関連設計 | `auth-authorization.md`、`rest-api-persistence.md`、`domain-model.md` §3.1〜§3.2、`observability-and-config.md` §5 |

#### Milestone 2: Realtime Minimum

| 項目 | 内容 |
|---|---|
| 成果物 | WebSocket handshake、protocol v1、Instance 作成、join/leave、位置同期、snapshot/delta、TypeScript SDK |
| 入口条件 | Milestone 1 完了。ADR-004（protocol evolution）、ADR-006/ADR-006A（instance concurrency / sequence）の確定 |
| 出口条件 | 2 クライアントが同一 Instance に参加し、Transform 更新が相互に配信される。Snapshot/Delta が正常に動作する。SDK で接続・入室・状態購読ができる |
| 依存 | Milestone 1 |
| 検証 | `realtime-protocol-and-connection.md` の受入条件。`client-sdk.md` §11 の受入条件 4〜6、14〜16。`test-and-ci.md` §4.2 の受入条件 4〜6 |
| 関連設計 | `realtime-protocol-and-connection.md`、`transport-boundaries.md`、`state-and-runtime.md` §3、`client-sdk.md` |

#### Milestone 3: Reliability

| 項目 | 内容 |
|---|---|
| 成果物 | heartbeat、reconnect、resume token、resync、bounded queue、rate limiting、graceful shutdown |
| 入口条件 | Milestone 2 完了 |
| 出口条件 | 切断後に自動再接続し、resume で状態復旧できる。latest-wins 集約が機能する。rate limit が動作する。SIGTERM で graceful shutdown する |
| 依存 | Milestone 2 |
| 検証 | `mobile-resume-interest-backpressure.md` §10 の受入条件 1〜15、19〜21。`deployment-and-threat-model.md` §5.1 の受入条件 3〜4 |
| 関連設計 | `mobile-resume-interest-backpressure.md`、`deployment-and-threat-model.md` §2 |

#### Milestone 4: Generic Entity Runtime

| 項目 | 内容 |
|---|---|
| 成果物 | entity spawn/update/delete、ownership、custom component、persistence checkpoint、extension events |
| 入口条件 | Milestone 3 完了。ADR-007（拡張 API）の確定 |
| 出口条件 | エンティティの CRUD と所有権検証が動作する。チェックポイントで永続化される。拡張イベントが配送される |
| 依存 | Milestone 3 |
| 検証 | `state-and-runtime.md` の受入条件。`extension-mechanism.md` の受入条件。`deployment-and-threat-model.md` §5.3 の受入条件 12〜13 |
| 関連設計 | `state-and-runtime.md` §2.4、`extension-mechanism.md`、`domain-model.md` §4 |

#### Milestone 5: Scaling

| 項目 | 内容 |
|---|---|
| 成果物 | Uniform Grid、visibility policy、load generator、200〜1000 接続試験、soak test、performance documentation |
| 入口条件 | Milestone 4 完了 |
| 出口条件 | Phase 2（200 人/1000 接続）の負荷試験が合格基準を満たす。24 時間 soak test で leak がない。性能ドキュメントが作成される |
| 依存 | Milestone 4 |
| 検証 | `scale-and-nfr.md` §9.3 の Phase 2 シナリオ。§10 の受入条件 1〜9。`test-and-ci.md` §4.3 の受入条件 7〜8 |
| 関連設計 | `scale-and-nfr.md`、`mobile-resume-interest-backpressure.md` §7、`test-and-ci.md` §2.6〜§2.7 |

#### Milestone 6: OSS Usability

| 項目 | 内容 |
|---|---|
| 成果物 | reference web、minimal admin web、SDK examples、deployment guide、upgrade/rollback guide、first stable beta |
| 入口条件 | Milestone 5 完了 |
| 出口条件 | 参照実装が動作する。デプロイガイドでセルフホストできる。アップグレード/ロールバック手順が文書化される。stable beta がリリースされる |
| 依存 | Milestone 5 |
| 検証 | `release-maintenance-license.md` §4.1 の受入条件 1〜5。`deployment-and-threat-model.md` §5.1 の受入条件 1〜2 |
| 関連設計 | `release-maintenance-license.md`、`deployment-and-threat-model.md` §1、`client-sdk.md` §10 |

### 2.2 依存関係図

```text
M0 (Repository Foundation)
 │
 ▼
M1 (Identity and Administration)
 │
 ▼
M2 (Realtime Minimum)
 │
 ▼
M3 (Reliability)
 │
 ▼
M4 (Generic Entity Runtime)
 │
 ▼
M5 (Scaling)
 │
 ▼
M6 (OSS Usability)
```

**[SPEC]** 上記の Milestone 順序は仕様 §39 が定める確定した依存順序である。Milestone 間の依存は、後続が先行の成果物を必要とすることを意味する。

**[REC]** 各 Milestone の内部タスクは並行してよい。期間・日付は計画時に決定する。

## 3. MVP 完了条件

### 3.1 MVP Acceptance Matrix

**[SPEC]** 仕様 §40 が定める MVP 完了条件（18 項目）を、設計文書と検証方法へマッピングする。

| # | MVP 条件 | 設計文書 | 検証方法 | Milestone |
|---|---|---|---|---|
| 1 | 管理者がユーザーを作成できる | `auth-authorization.md` §2、`rest-api-persistence.md` §2 | integration test | M1 |
| 2 | ユーザーが ID とパスワードでログインできる | `auth-authorization.md` §3 | integration test | M1 |
| 3 | 匿名接続が拒否される | `auth-authorization.md`、`transport-boundaries.md` §1.3 | integration test | M1/M2 |
| 4 | 管理者が World Definition と Instance を作成できる | `rest-api-persistence.md` §2 | integration test | M1 |
| 5 | 2 つ以上の異なるクライアントが同一 Instance へ参加できる | `realtime-protocol-and-connection.md` §5 | integration test | M2 |
| 6 | 一方の Transform 更新が他方へ配信される | `state-and-runtime.md` §3.4 | integration test | M2 |
| 7 | サーバーが不正 Transform を拒否できる | `state-and-runtime.md` §2.2 | unit test + integration test | M2 |
| 8 | 切断後に自動再接続できる | `mobile-resume-interest-backpressure.md` §2、`client-sdk.md` §5 | integration test | M3 |
| 9 | Resume または Snapshot 再取得で状態復旧できる | `mobile-resume-interest-backpressure.md` §4、`client-sdk.md` §6 | integration test | M3 |
| 10 | 最新位置更新が送信詰まり時に集約される | `mobile-resume-interest-backpressure.md` §5、`client-sdk.md` §4.3 | unit test + integration test | M3 |
| 11 | エンティティを作成、更新、削除できる | `state-and-runtime.md` §3.1、`domain-model.md` §4 | integration test | M4 |
| 12 | 所有権と権限が検証される | `state-and-runtime.md` §2.4、`auth-authorization.md` | unit test + integration test | M4 |
| 13 | Docker Compose で起動できる | `deployment-and-threat-model.md` §1.2 | E2E test | M0/M6 |
| 14 | PostgreSQL バックアップ・復元手順がある | `deployment-and-threat-model.md` §3 | 手順書 + リストアテスト | M6 |
| 15 | CI で protocol 互換性を検査できる | `test-and-ci.md` §2.3、`repo-crate-conventions.md` §4 | CI 実行 | M0/M2 |
| 16 | 負荷試験結果を再現できる | `scale-and-nfr.md` §9、`test-and-ci.md` §2.6 | load test 実行 | M5 |
| 17 | SECURITY.md と脆弱性報告経路がある | `release-maintenance-license.md` §2.6 | ドキュメント存在確認 | M0 |
| 18 | CONTRIBUTING.md に開発環境構築手順がある | `release-maintenance-license.md` §2.1 | ドキュメント存在確認 | M0 |

### 3.2 MVP の判定

**[REC]** MVP は上記 18 項目をすべて満たした時点で完了とする（仕様 §40）。各項目の検証は対応する Milestone の出口条件で実施する。

## 4. 将来検討事項

### 4.1 非コミットメントの原則

**[SPEC]** 仕様 §41 が列挙する将来検討事項を初期コアへ先行実装しない（仕様 §41）。

**[REC]** 以下はコミットメントではなく、ADR trigger として管理する。具体的な必要性が観測された場合に ADR を作成し、設計・実装を決定する。

### 4.2 将来候補と ADR Trigger

| # | 将来候補（仕様 §41） | ADR Trigger | 関連設計 |
|---|---|---|---|
| 1 | LDAP/AD/OIDC/SAML adapter | 外部 IdP 連携の要求が発生 | `auth-authorization.md`、`architecture.md` §8（Authentication Service 分離） |
| 2 | マルチテナント | 複数組織の運用要求が発生 | `architecture.md` §8、TD-06 |
| 3 | WebTransport/QUIC | WebSocket の限界が計測で確認 | `transport-boundaries.md` §1.2（仕様 §13.2 の将来 transport） |
| 4 | Redis/NATS 等の内部 message bus | 単一プロセスの限界が計測で確認 | `architecture.md` §8（Persistence Worker 分離）、TD-06 |
| 5 | Instance の別 Node 配置 | 単一インスタンスの CPU/メモリ限界 | `scale-and-nfr.md` §6（Phase 3）、SN-01 |
| 6 | 空間 sharding | 1 インスタンス 1000 人の要求 | `scale-and-nfr.md` §6、SN-01 |
| 7 | WASM extension runtime | out-of-process の限界または需要 | `extension-mechanism.md`、仕様 §23 |
| 8 | 永続 Event Store | イベント履歴の要求 | `extension-mechanism.md`、`architecture.md` ARC-04 |
| 9 | CRDT を使った一部共同編集 | 共同編集の要求 | `state-and-runtime.md` §2（サーバー権威との整合） |
| 10 | 管理者向け自動アップデート支援 | 運用負担の増大 | `release-maintenance-license.md` §2.7 |
| 11 | 複数地域配置 | 地理分散の要求 | `scale-and-nfr.md` §6、`architecture.md` §8 |
| 12 | SDK 追加 | 他言語 SDK の需要とメンテナー確保 | `client-sdk.md` §9、TD-12、ADR-011 |
| 13 | Federation | 組織間連携の要求 | `architecture.md` §8、`system-context.md` §3 |

**[REC]** 各候補の ADR は、`architecture.md` §8 の分離候補表と整合させる。分離は「将来可能」であり、予定ではない。

## 5. 未決定事項マッピング

### 5.1 仕様 §42 の未決事項

**[REC]** 仕様 §42 が列挙する 14 項目の未決定事項を、既存 ADR ID / 正本文書 / 決定時期 / owner 領域へマッピングする。

| # | 未決定事項（仕様 §42） | ADR ID | 正本文書 | 決定時期 | Owner 領域 |
|---|---|---|---|---|---|
| 1 | 正式プロジェクト名 | ADR-001（Accepted: OrbiSync） | `docs/adr/ADR-001-naming-identifiers.md` | 決定済み（2026-07-31） | 全体 |
| 2 | Apache-2.0 単独か MIT/Apache-2.0 デュアルか | RL-05（Accepted: MIT OR Apache-2.0） | `docs/adr/RL-05-license.md` | 決定済み（2026-07-31） | 全体 |
| 3 | Access Token形式 | ADR-002（Accepted: 15分Ed25519 JWT） | `docs/adr/ADR-002-authentication-session.md` | 決定済み（2026-07-31） | identity_access |
| 4 | Refresh Tokenのbrowser運用方式 | ADR-002（Accepted: SDK memory保持、永続化はhost責務） | `docs/adr/ADR-002-authentication-session.md` | 決定済み（2026-07-31） | identity_access |
| 5 | Protocol Buffersのcode generation | ADR-004（Accepted: Buf、生成物非commit） | `docs/adr/ADR-004-realtime-protocol.md` | 決定済み（2026-07-31） | protocol |
| 6 | JSON debug protocol | ADR-004（Accepted: 開発時featureのみ） | `docs/adr/ADR-004-realtime-protocol.md` | 決定済み（2026-07-31） | realtime_gateway |
| 7 | World Instance の自動生成規則 | RM-01 | `rest-api-persistence.md`（world_directory） | world/instance API 実装時（M1） | world_directory |
| 8 | custom component の最大サイズ | RM-02 | `state-and-runtime.md`、`transport-boundaries.md` §5 | entity runtime 実装時（M4） | instance_runtime |
| 9 | 初期tick rate | ADR-006（Accepted: active 20Hz / idle 1Hz） | `docs/adr/ADR-006-instance-concurrency.md` | 決定済み（2026-07-31） | instance_runtime |
| 10 | transform validation のデフォルト値 | SR-01 | `state-and-runtime.md` §2.2 | runtime 実装時（M2） | instance_runtime |
| 11 | snapshot 差分履歴の保持量 | MRIB-03 | `mobile-resume-interest-backpressure.md` §4.2 | reliability 実装時（M3） | instance_runtime |
| 12 | Webhook拡張をMVPへ含めるか | ADR-007 / RM-03（Accepted: v1で実装、MVP条件外） | `docs/adr/ADR-007-extension-api.md` | 決定済み（2026-07-31） | extension_gateway |
| 13 | admin-web を同一リポジトリで管理するか | RM-04 | `repo-crate-conventions.md` §1.2 | M6 の設計時 | 全体 |
| 14 | 公式 TypeScript SDK の対応ランタイム範囲 | ADR-011 | `client-sdk.md` §9、`technology-decisions.md` TD-12 | SDK 公開前（M2/M6） | SDK |

### 5.2 設計文書内の ADR バックログ

**[REC]** 各設計文書が主担当とする ADR ID の一覧。詳細は各文書の「要 ADR 事項」節を参照。

| 文書 | ADR ID 範囲 | 件数 |
|---|---|---|
| `architecture.md` | ARC-01〜ARC-08 | 8 |
| `domain-model.md` | DM-01〜DM-07 | 7 |
| `state-and-runtime.md` | SR-01〜SR-07 | 7 |
| `transport-boundaries.md` | TB-01〜TB-07 | 7 |
| `realtime-protocol-and-connection.md` | RP-01〜RP-05 | 5 |
| `mobile-resume-interest-backpressure.md` | MRIB-01〜MRIB-10 | 10 |
| `auth-authorization.md` | AA-01〜AA-xx | 複数 |
| `rest-api-persistence.md` | AP-01〜AP-xx | 複数 |
| `extension-mechanism.md` | EX-01〜EX-xx | 複数 |
| `client-sdk.md` | SDK-01〜SDK-04 | 4 |
| `scale-and-nfr.md` | SN-01〜SN-05 | 5 |
| `observability-and-config.md` | OC-01〜OC-06 | 6 |
| `repo-crate-conventions.md` | RC-01〜RC-04 | 4 |
| `test-and-ci.md` | TC-01〜TC-07 | 7 |
| `release-maintenance-license.md` | RL-01〜RL-07 | 7 |
| `deployment-and-threat-model.md` | DT-01〜DT-05 | 5 |
| `technology-decisions.md` | ADR-001〜ADR-012 | 12 |
| 本書 | RM-01〜RM-04 | 4 |

## 6. 再監査結果

### 6.1 正本の重複

**[REC]** 全 19 設計文書を再監査した結果、以下の正本重複は意図的であり、参照関係で管理されている：

| 重複事項 | 正本 | 参照側 | 管理方法 |
|---|---|---|---|
| モジュール所有権・DAG | `architecture.md` §3 | 全文書 | 「設計前提」として参照。再定義しない |
| rate limit / backpressure | MRIB 書 §8、TB 書 §4 | `scale-and-nfr.md`、`deployment-and-threat-model.md` | 値の参照のみ。再定義しない |
| 機密情報除去 | TD-08、仕様 §26.4 | `observability-and-config.md`、`client-sdk.md` | 設計前提として参照 |
| graceful shutdown | `architecture.md` §4.3、仕様 §37.3 | `deployment-and-threat-model.md` §2 | 対応表で整合。再定義しない |

### 6.2 矛盾なし

**[REC]** 全設計文書間で以下の矛盾がないことを確認した：

- 依存方向（adapter → application → domain）は全文書で一致
- DAG（world-runtime ↛ interest/delivery、interest ↛ realtime）は全文書で一致
- 一時/永続状態の分類は `state-and-runtime.md` §1 が正本であり、他文書は参照のみ
- ライセンスはRL-05で`MIT OR Apache-2.0`、v1 topologyはADR-009で確定し、全文書で整合
- 仕様の推奨/例/候補を[SPEC]へ格上げしていない（各レビュー修正済み）

## 7. 要 ADR 事項

本書が主担当となる判断を RM ID で管理する。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| RM-01 | World Instance の自動生成規則 | 明示的作成のみ。自動生成はしない | 仕様 §42.7。初期は管理操作のみ |
| RM-02 | custom component の最大サイズ | 64 KiB（カスタムイベント上限と整合） | 仕様 §42.8。TB 書 §5 のメッセージ上限と整合 |
| RM-03 | Webhook 拡張を MVP へ含めるか | M4 で含める。MVP 条件には含めない | 仕様 §42.12。MVP §40 の 18 項目に拡張は含まれない |
| RM-04 | admin-web を同一リポジトリで管理するか | 同一リポジトリの `apps/admin-web` で管理 | 仕様 §42.13。monorepo の利点（§29）と整合 |
