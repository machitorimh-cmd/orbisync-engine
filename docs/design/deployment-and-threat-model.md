# OrbiSync デプロイと脅威モデル設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §37（デプロイ仕様）と §38（脅威モデル概要）を、最小デプロイ/health/readiness/graceful shutdown/backup-restore/secret、asset/authz/replay/DoS/supply-chain/extension 脅威と境界・対策・検証の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `DT-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| Composition Root、起動/終了順序 | `architecture.md` §4.3 | 前提として参照。graceful shutdown の骨格 |
| OCI image / Docker Compose | TD-10 | 前提として参照。技術選定 |
| TLS、セキュリティ要件 | `scale-and-nfr.md` §7.3、TD-06 §6.1 | 前提として参照。脅威モデルの対策と整合 |
| 設定の secret 分離 | `observability-and-config.md` §6.4 | 前提として参照 |
| rate limit、size limit、backpressure | MRIB 書 §8、TB 書 §4, §5 | 前提として参照。DoS 対策 |
| 認証・認可 | `auth-authorization.md` | 前提として参照。authz 脅威の対策 |
| 拡張機構（Webhook、out-of-process） | `extension-mechanism.md` | 前提として参照。extension 脅威の対策 |
| 監査ログ | `observability-and-config.md` §5 | 前提として参照。audit 改ざん対策 |
| 依存関係とサプライチェーン | TD-05、`test-and-ci.md` §3.5 | 前提として参照。supply-chain 脅威の対策 |
| release artifact、SBOM | `release-maintenance-license.md` §1.5 | 前提として参照 |
| failure isolation | `scale-and-nfr.md` §8 | 前提として参照 |

## 1. デプロイ構成

### 1.1 最小構成

**[REC]** 仕様 §37.1 が示す最小構成の例：

```text
Reverse Proxy / TLS
        ↓
orbisync-server
        ↓
PostgreSQL
```

**[REC]** 上記は仕様例であり、確定した topology ではない。確定できるのは、Core サーバーと PostgreSQL を同一 Linux ホストまたは Docker Compose で運用可能であること、仕様 §37.1 が最小構成の例を示していることまでである。1 deployment = 1 組織の対応や、Docker Compose を標準運用形態とするかは topology の設計判断である。

**設計前提** ADR-009によりv1は1 deployment = 1 organization、外部reverse proxy TLS終端、1 server process + 1 PostgreSQL logical database/schemaとする。Kubernetesはv1 support対象外である。

### 1.2 Docker Compose

**[REC]** 仕様 §37.2 が示す Docker Compose の例を基に、初期版の標準構成を定義する。

**[REC]** 推奨 Compose 構成：

```yaml
services:
  core:
    image: ghcr.io/example/orbisync:0.1  # タグ固定、latest 禁止
    environment:
      DATABASE_URL: postgres://...        # secret は .env または secret manager
    depends_on:
      postgres:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/health/live"]
      interval: 10s
      timeout: 5s
      retries: 3

  postgres:
    image: postgres:17
    volumes:
      - postgres_data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U orbisync"]
      interval: 5s
      timeout: 3s
      retries: 5

volumes:
  postgres_data:
```

**[REC]** イメージ tag は固定し、`latest` を本番例で推奨しない（仕様 §37.2）。

**[REC]** Compose は本番の TLS/backup/monitoring を自動的に保証しない（TD-10）。TLS、バックアップ、監視は運用者が別途構成する。

**[ADR]** Compose の production 構成（TLS 終端、ログ収集、監視統合）は DT-01 で決める。

### 1.3 Health / Readiness

**[SPEC]** DB 停止時に ready=false、プロセス生存のみを live で判定する（仕様 §26.1）。

**[REC]** health endpoint の設計：

| Endpoint | 意味 | 判定基準 |
|---|---|---|
| `/health/live` | プロセス生存 | プロセスが起動していること |
| `/health/ready` | 受付可能 | DB 接続プールが確立していること |

**[REC]** `/health/live` は常に 200 を返す（プロセス生存中）。`/health/ready` は DB 接続が確立していれば 200、そうでなければ 503 を返す。

**[REC]** Kubernetes を採用する場合、liveness probe は `/health/live`、readiness probe は `/health/ready` を使用する。これは Kubernetes 採用を意味しない条件付きの例である。Docker Compose の healthcheck は `/health/live` を使用する。

**[REC]** health endpoint は認証を要求しない。ただし、内部ネットワークまたは localhost のみからアクセス可能にすることを推奨する。

**設計前提** ADR-009によりhealth endpointは認証なし・localhost/internal network限定とする。

## 2. Graceful Shutdown

### 2.1 手順

**[SPEC]** 仕様 §37.3 が定める graceful shutdown の手順：

1. ready=false
2. 新規接続拒否
3. 新規インスタンス作成停止
4. 既存 WebSocket へ shutdown 通知
5. checkpoint 対象を保存
6. 接続 drain
7. DB pool close
8. 終了

**設計前提** 起動処理だけが具体 adapter を選び、port へ注入する。終了時は新規受付停止 → 接続/command の drain → 永続化要求の flush → task 終了とする（`architecture.md` §4.3）。

**[REC]** 上記の仕様手順と `architecture.md` §4.3 の対応：

| 仕様 §37.3 の手順 | architecture.md §4.3 の対応 | 所有 |
|---|---|---|
| 1. ready=false | 新規受付停止 | `server`（Composition Root） |
| 2. 新規接続拒否 | 新規受付停止 | `transport-http` / `realtime_gateway` |
| 3. 新規インスタンス作成停止 | 新規受付停止 | `world_directory` |
| 4. 既存 WebSocket へ shutdown 通知 | — | `realtime_gateway` |
| 5. checkpoint 対象を保存 | 永続化要求の flush | `instance_runtime` → `persistence` |
| 6. 接続 drain | 接続/command の drain | `realtime_gateway` / `realtime_delivery` |
| 7. DB pool close | task 終了 | `storage-postgres` |
| 8. 終了 | task 終了 | `server` |

**[REC]** shutdown 通知は WebSocket の Close frame（code 1001 Going Away）で送信する。クライアントは SDK の再接続フロー（`client-sdk.md` §5）で別のサーバーへ再接続できる。

**[ADR]** drain 期限の具体値は DT-03 で決める。推奨初期値は 30 秒。期限超過後は残りの接続を強制切断する。

### 2.2 Shutdown のテスト

**[REC]** graceful shutdown は integration test と chaos test で検証する（`test-and-ci.md` §2.8）：

| テスト | 方法 | 合格基準 |
|---|---|---|
| SIGTERM 後の drain | SIGTERM 送信 → drain 完了を待機 | 全接続が正常に閉じる。checkpoint が保存される |
| drain 期限超過 | drain 中に接続を維持し続ける | 期限超過後に強制切断される |
| shutdown 中の新規接続 | shutdown 開始後に接続を試みる | 拒否される（503 または WS upgrade 拒否） |

## 3. バックアップとリストア

### 3.1 バックアップ対象

**[SPEC]** 仕様 §37.4 が定めるバックアップの対象：

- PostgreSQL バックアップ
- 暗号鍵、設定、secret の安全な保管
- 復元手順を定期テスト
- バックアップ取得だけでなく restore test を行う

**[REC]** バックアップ対象の詳細：

| 対象 | 方法 | 頻度 |
|---|---|---|
| PostgreSQL データ | `pg_dump` / `pg_basebackup` | 日次 + WAL アーカイブ |
| 暗号鍵（token 署名鍵等） | 安全な保管（secret manager 等） | 変更時 |
| 設定ファイル | バージョン管理（git） | 変更時 |
| secret（環境変数） | 安全な保管（secret manager 等） | 変更時 |

**[REC]** 一時状態（位置、向き、接続状態）はバックアップ対象外である（`state-and-runtime.md` §1.1）。チェックポイントで保存された永続エンティティのみが復元対象となる。

### 3.2 リストア手順

**[REC]** リストア手順の文書化：

1. PostgreSQL のリストア（`pg_restore` / `pg_basebackup` から）
2. migration の適用（リストア先の schema version が古い場合）
3. secret のリストア（環境変数 / secret manager）
4. サーバーの起動
5. 起動後の検証（health check、データ整合性）

**[REC]** リストア手順は定期テストする（仕様 §37.4）。テストの頻度は運用 ADR で決める。

**[ADR]** バックアップの保持期間、リストアテストの頻度、secret manager の選定は DT-04 で決める。

## 4. 脅威モデル

### 4.1 対象と境界

**[REC]** 脅威モデルの対象は OrbiSync の公開面（REST API、WebSocket、管理 API）と内部処理である。フロントエンド、3D 描画、アセット、音声は対象外である（仕様 §1、`system-context.md` §3）。

**[REC]** 信頼境界：

```text
┌─────────────────────────────────────────────────────────────┐
│ 信頼境界の外（クライアント、ネットワーク）                    │
│                                                             │
│  任意クライアント ──HTTPS/WSS──► Reverse Proxy / TLS        │
└─────────────────────────────────┬───────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────┐
│ 信頼境界 1: Inbound Adapter                                 │
│  transport-http / realtime_gateway                          │
│  入力検証、認証、rate limit、size limit                      │
└─────────────────────────────────┬───────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────┐
│ 信頼境界 2: Application / Domain                            │
│  application / world-runtime / interest / identity          │
│  認可、所有権検証、domain validation                         │
└─────────────────────────────────┬───────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────┐
│ 信頼境界 3: Outbound Adapter                                │
│  storage-postgres / extensions / observability              │
│  DB アクセス、外部配送、telemetry                            │
└─────────────────────────────────────────────────────────────┘
```

### 4.2 脅威と対策

**[REC]** 仕様 §38 が列挙する想定脅威と対策を、信頼境界と検証方法で具体化する。

#### 認証・認可の脅威

| 脅威 | 攻撃例 | 対策 | 所有モジュール | 検証方法 |
|---|---|---|---|---|
| ブルートフォースログイン | 大量のログイン試行 | IP 単位 rate limit、アカウントロック | `http_api` / `identity_access` | integration test（§2.10 security test） |
| 不正 token | 偽造/改ざん token の提示 | token 署名検証、形式検証 | `identity_access` | unit test（署名検証） |
| 失効後 token 再利用 | ログアウト済み token の再利用 | token revocation、refresh rotation | `identity_access` | integration test（logout 後の token 無効化） |
| 権限昇格 | 一般ユーザーが管理操作を実行 | RBAC の application 境界での強制 | Application coordinator / `identity_access` | integration test（権限不足の拒否） |
| 他ユーザーエンティティ操作 | 他人の entity を更新 | server authoritative ownership | `instance_runtime` | unit test（所有権検証）+ integration test |

**設計前提** 認証・認可の詳細は `auth-authorization.md` が定める。クライアントの自己申告を権限判定に使わない（仕様 §20.3, §15.2）。

#### リアルタイムの脅威

| 脅威 | 攻撃例 | 対策 | 所有モジュール | 検証方法 |
|---|---|---|---|---|
| 大量 WebSocket 接続 | 接続リソースの枯渇 | 接続単位 rate limit、同時接続数上限 | `realtime_gateway` | load test（`test-and-ci.md` §2.6） |
| 巨大メッセージ | メモリ/CPU の圧迫 | message size limit（16 KiB / 64 KiB） | `realtime_gateway` | integration test（size limit 超過の拒否） |
| 高頻度 transform spam | tick 処理の圧迫 | 接続単位 rate limit、latest-wins 集約 | `realtime_gateway` / `instance_runtime` | load test + unit test（queue 集約） |
| NaN/Infinity 注入 | 状態の破損 | typed validation（有限値の強制） | `instance_runtime` | unit test（NaN/Infinity の拒否） |
| snapshot 情報漏洩 | 可視範囲外の entity 情報の取得 | Interest による visibility filter | `interest` / coordinator | unit test（可視集合の純粋計算）+ integration test |

**設計前提** rate limit、size limit、backpressure の詳細は MRIB 書 §8、TB 書 §4, §5 が定める。

#### 外部配送の脅威

| 脅威 | 攻撃例 | 対策 | 所有モジュール | 検証方法 |
|---|---|---|---|---|
| Webhook SSRF | 内部ネットワークへのリクエスト誘導 | Webhook 宛先制限（プライベート IP 拒否） | `extension_gateway` | integration test（プライベート IP の拒否） |
| Webhook 改ざん | 配送内容の改ざん | HMAC 署名、timestamp、event ID | `extension_gateway` | unit test（署名検証） |

**設計前提** Webhook の要件（HMAC 署名、timestamp、event ID、再送、backoff、DLQ、重複配信前提、受信側 idempotency）は `extension-mechanism.md`、仕様 §23.4 が定める。

#### 監査・サプライチェーンの脅威

| 脅威 | 攻撃例 | 対策 | 所有モジュール | 検証方法 |
|---|---|---|---|---|
| 監査ログ改ざん | ログの削除/変更 | structured audit、append-only | `audit_observability` | integration test（監査ログの不可変性） |
| 依存ライブラリ脆弱性 | 既知脆弱性の悪用 | dependency scanning、cargo deny | CI | PR CI + Nightly（`test-and-ci.md` §3.5） |
| リプレイ攻撃 |  captured メッセージの再送 | sequence 管理、message_id dedup | `realtime_gateway` / `instance_runtime` | unit test（sequence gap/重複の検知） |

**設計前提** 監査ログの形式と保持は `observability-and-config.md` §5 が定める。依存関係の検査は `test-and-ci.md` §3.5、TD-05 が定める。

### 4.3 対策のまとめ

**[REC]** 仕様 §38 が列挙する対策と、本書の対応：

| 対策 | 本書の節 | 正本 |
|---|---|---|
| rate limit | §4.2 認証・リアルタイム | MRIB 書 §8.4、TB 書 §4.4 |
| token rotation/revocation | §4.2 認証 | `auth-authorization.md`、ADR-002 |
| size limit | §4.2 リアルタイム | TB 書 §5 |
| typed validation | §4.2 リアルタイム | `state-and-runtime.md` §2.2 |
| server authoritative ownership | §4.2 認証 | `state-and-runtime.md` §2.4 |
| permission check | §4.2 認証 | `auth-authorization.md` |
| visibility filter | §4.2 リアルタイム | MRIB 書 §7 |
| webhook 宛先制限 | §4.2 外部配送 | `extension-mechanism.md` |
| structured audit | §4.2 監査 | `observability-and-config.md` §5 |
| dependency scanning | §4.2 サプライチェーン | `test-and-ci.md` §3.5 |

### 4.4 脅威モデルの管理

**[SPEC]** 詳細は `docs/security/threat-model.md` で管理する（仕様 §38）。

**[REC]** 本書は設計レベルの脅威モデルを定義する。運用レベルの詳細（具体的な攻撃シナリオ、CVSS スコア、緩和策の実装詳細）は `docs/security/threat-model.md` で管理し、セキュリティレビューごとに更新する。

**[REC]** 脅威モデルは以下のタイミングで見直す：

- 新しい transport の追加（例: WebTransport/QUIC）
- 認証方式の変更（例: OIDC 追加）
- 拡張機構の変更
- 依存関係の重大な変更
- セキュリティインシデントの後

## 5. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 5.1 デプロイ

1. Docker Compose で `orbisync-server` と PostgreSQL が起動し、`/health/live` が 200 を返す。
2. DB 停止時に `/health/ready` が 503 を返し、DB 復旧後に 200 に戻る。
3. SIGTERM 後に graceful shutdown が完了し、仕様 §37.3 の 8 手順が順序通りに実行される。
4. shutdown 中に新規接続が拒否される。
5. OCI image のタグが固定されており、`latest` が使用されない。

### 5.2 バックアップ

6. PostgreSQL のバックアップからリストアし、サーバーが正常に起動する。
7. リストア後に migration が適用され、schema が最新になる。

### 5.3 脅威モデル

8. ブルートフォースログインが rate limit で拒否される。
9. 不正 token（署名不一致）が拒否される。
10. 失効後 token の再利用が拒否される。
11. message size limit を超える WebSocket メッセージが拒否される。
12. NaN/Infinity を含む TransformInput が拒否される。
13. 他ユーザーの entity 更新が所有権検証で拒否される。
14. Interest の可視範囲外の entity が Snapshot に含まれない。
15. Webhook の宛先がプライベート IP の場合、配送が拒否される。
16. 監査ログが append-only で記録され、既存エントリの変更・削除ができない。
17. sequence の重複（リプレイ）が検知され、drop される。

## 6. 要 ADR 事項

本書が主担当となる判断を DT ID で管理する。他文書が正本の判断（ADR-009、ADR-002、SN-05 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| DT-01 | TLS 終端の配置、reverse proxy の選定、Compose production 構成 | reverse proxy による TLS 終端（プロセス外）。proxy の選定は運用環境に依存 | 仕様 §37.1 は最小構成の例。topology の確定ではない。ADR-009 と整合 |
| DT-02 | health endpoint の認証/公開範囲 | 認証なし、内部ネットワーク/localhost のみ | 仕様 §26.1 の live/ready。Kubernetes probe との整合。ADR-009 と整合 |
| DT-03 | drain 期限の具体値 | 30 秒 | `architecture.md` §4.3 の drain。モバイルクライアントの再接続を許容する時間 |
| DT-04 | バックアップ保持期間、リストアテスト頻度、secret manager | 保持 30 日、リストアテスト月次、secret manager は運用環境に依存 | 仕様 §37.4 の定期テスト。具体値は運用 ADR |
| DT-05 | Webhook 宛先制限の具体規則 | プライベート IP（RFC 1918、link-local、loopback）の拒否。allowlist のオプション提供 | SSRF 対策。仕様 §38 の「webhook 宛先制限」を具体化 |
