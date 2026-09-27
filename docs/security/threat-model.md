# OrbiSync 脅威モデル

## 0. 文書の目的と表記

本書は `metaverse_core_specification.md` §38 と `docs/design/deployment-and-threat-model.md` §4 を詳細化し、OrbiSync の脅威、信頼境界、対策、検証方法、残存リスクを継続管理する正本である。

- **[SPEC] 仕様由来の確定事項**: 原仕様で要求または禁止されている事項
- **[REC] 設計・運用上の推奨**: 要求を満たすための対策案
- **[ADR] 未確定事項**: 実装または運用環境に応じて別途決定する事項

本書に記載した対策は、実装とテストが存在して初めて有効とみなす。対策が未実装または未検証の場合、該当する脅威の状態を「未対応」または「一部対応」として扱う。

## 1. 対象範囲

### 1.1 対象

**[SPEC]** 次の公開面とCore内部処理を対象とする（仕様 §1、§38）。

- HTTPSによる管理REST API
- WSSによるリアルタイムプロトコル
- 認証、token発行・更新・失効
- 認可、所有権、管理操作
- World Instance、Entity、Snapshot、Delta、Domain Event
- PostgreSQLへの永続化とmigration
- Webhookおよびout-of-process Extension
- ログ、メトリクス、トレース、監査記録
- OCI image、依存ライブラリ、Release CI
- バックアップ、restore、設定、secret

### 1.2 対象外

**[SPEC]** 3D描画、物理エンジン、音声・映像配信、アセット配信、製品固有UIはCoreの責務外である（仕様 §1、§6）。

**[REC]** 対象外コンポーネントからCoreへ入力が到達する境界は対象内とする。たとえば、外部フロントエンドが生成したREST bodyやWebSocket frameは信頼しない。

### 1.3 保護対象

| 資産 | 保護する性質 | 侵害時の主な影響 |
|---|---|---|
| password、access token、refresh token、service token | 機密性・完全性 | アカウント侵害、権限昇格 |
| User、Role、Permission、所有権 | 完全性 | 不正操作、管理権限奪取 |
| World Definition、Instance、Entityの正準状態 | 完全性・可用性 | 状態改ざん、サービス停止 |
| Snapshot、Delta、Domain Event | 機密性・完全性・順序性 | 情報漏洩、クライアント状態破損 |
| PostgreSQLデータとbackup | 機密性・完全性・可用性 | データ漏洩・消失 |
| 監査記録 | 完全性・追跡可能性 | 不正操作の隠蔽 |
| Webhook宛先とpayload | 機密性・完全性 | SSRF、内部情報漏洩 |
| build/release artifact | 完全性・出所証明 | サプライチェーン侵害 |
| secretと運用設定 | 機密性・完全性 | 全体侵害、保護機構の無効化 |

## 2. 攻撃者と前提

### 2.1 想定する攻撃者

- 未認証のインターネット利用者
- 認証済みだが権限の低い利用者
- 侵害された利用者端末または盗まれたtokenの保持者
- 悪意または脆弱性を持つExtension/Webhook受信先
- 誤設定を行う運用者
- 侵害された依存パッケージ、CI、container base imageの提供者
- DB、backup、ログ保存先へ不正アクセスできる内部者

### 2.2 セキュリティ前提

**[SPEC]**

- 公開通信はHTTPS/WSSを使用する（仕様 §3.3、§26.1）。
- clientの自己申告による所有権や権限を信用しない（仕様 §20.3）。
- password、token、完全なpayloadをログへ出力しない（仕様 §26.4）。
- 一時状態は主にメモリ、永続状態はPostgreSQLで管理する（仕様 §10）。

**[REC]**

- internet、client、Extension、Webhook応答、proxy header、DBから読み戻した可変データを信頼しない。
- TLS終端より内側であっても、認証・認可・入力検証を省略しない。
- 単一プロセス構成を、モジュール間の無制限な信頼の根拠にしない。

## 3. 信頼境界とデータフロー

```text
Untrusted Client
   │ HTTPS / WSS
   ▼
TLS Endpoint / Reverse Proxy
   │ forwarded metadata（検証対象）
   ▼
HTTP API / Realtime Gateway
   │ 認証済み主体 + 検証済みcommand
   ▼
Application Coordinator
   ├── identity_access
   ├── instance_runtime
   ├── realtime_presence
   ├── interest（I/Oなし）
   └── realtime_delivery
          │
          └── outbound sink → WSS Client

Application / Domain
   │ repository port
   ▼
PostgreSQL / Backup Storage

Outbox / Extension Gateway
   │ outbound HTTPS（宛先検証対象）
   ▼
Untrusted Webhook / Extension

Build Source → CI → OCI / Release Artifact → Operator
```

| 境界 | 信頼しない入力 | 必須の保護 |
|---|---|---|
| Client → HTTP API | header、path、query、JSON、credential | TLS、size limit、構文検証、認証、認可、rate limit |
| Client → Realtime Gateway | upgrade要求、frame、sequence、revision、resume材料 | WSS、protocol/version検証、size/rate limit、認証、状態機械 |
| Gateway → Application | transport DTO、proxy由来metadata | DTO変換、認証済み主体、use case単位の認可 |
| Application → Runtime | entity command、transform、ownership要求 | domain検証、revision検証、直列化 |
| Application → PostgreSQL | query parameter、migration | parameter binding、最小権限、backup/restore検証 |
| Core → Webhook | URL、redirect、DNS解決結果、payload | SSRF対策、timeout、size limit、配送分離 |
| Core → Observability | log field、label、trace attribute | secret除去、cardinality制限、アクセス制御 |
| Source → Release | dependency、workflow、artifact | review、scan、固定version、checksum、SBOM |

## 4. リスク評価

**[REC]** リスクは「影響度 × 発生可能性」で評価する。

| 値 | 影響度 | 発生可能性 |
|---|---|---|
| 1 | 限定的。単一要求の失敗、機密情報なし | 現実的な経路がほぼない |
| 2 | 単一利用者または短時間の劣化 | 条件付きで可能 |
| 3 | 複数利用者、限定的な情報漏洩・改ざん | 一般的な攻撃手段で可能 |
| 4 | 組織全体、長時間停止、重要情報漏洩 | 公開面から再現可能 |
| 5 | 管理権限奪取、広範な永続データ侵害 | 低コストで反復可能 |

リスク値は影響度と発生可能性の積とし、1–4をLow、5–9をMedium、10–16をHigh、17–25をCriticalとする。

**[ADR]** 正式なrisk acceptance権限、SLA、CVSS併用方法はセキュリティ運用ADRで決める。

## 5. 脅威台帳

状態は `未対応`、`一部対応`、`対応済み`、`受容` のいずれかとする。初期値は実装前を想定して `未対応` とする。

| ID | 脅威 | 主な影響 | 初期リスク | 必須対策 | 検証 | 状態 |
|---|---|---|---:|---|---|---|
| TM-AUTH-01 | ブルートフォースログイン | account侵害、可用性低下 | High | IP/主体単位rate limit、同一応答、監査 | integration/load test | 未対応 |
| TM-AUTH-02 | 不正・偽造token | なりすまし | Critical | 署名・期限・issuer/audience検証、失効 | unit/integration test | 未対応 |
| TM-AUTH-03 | 失効後token再利用 | logout後の不正利用 | High | refresh rotation、hash保存、reuse検知 | integration test | 未対応 |
| TM-AUTH-04 | Realtime接続ticketの窃取・再利用 | 不正なWebSocket接続 | High | 60秒期限、単一使用、server-side binding、handshake timeout | integration test | 未対応 |
| TM-AUTHZ-01 | 権限昇格 | 管理操作・状態の不正変更 | Critical | application境界の認可、server authoritative ownership | negative test | 未対応 |
| TM-RT-01 | 大量WebSocket接続 | resource枯渇 | High | 接続上限、timeout、rate limit、隔離 | load/soak test | 未対応 |
| TM-RT-02 | 巨大・不正frame | memory/CPU枯渇、parser異常 | High | decode前後のsize limit、typed validation | fuzz/security test | 未対応 |
| TM-RT-03 | transform spam | tick遅延、帯域枯渇 | High | rate limit、latest-wins、bounded queue | load test | 未対応 |
| TM-RT-04 | NaN/Infinity注入 | 正準状態破損 | High | finite/range検証、異常値拒否 | property/fuzz test | 未対応 |
| TM-RT-05 | 他利用者Entity操作 | 状態改ざん | Critical | 認証済み主体と所有権のserver-side照合 | integration test | 未対応 |
| TM-DATA-01 | Snapshot情報漏洩 | 非公開状態・利用者情報漏洩 | Critical | Interest/Visibility filterをencode前に適用 | authorization test | 未対応 |
| TM-EXT-01 | Webhook SSRF | 内部service/metadataへの到達 | Critical | scheme/host/IP制限、再解決、redirect制限 | integration test | 未対応 |
| TM-AUDIT-01 | 監査ログ改ざん・削除 | 不正操作の隠蔽 | High | append-only化、アクセス分離、欠落監視 | integration/運用test | 未対応 |
| TM-SC-01 | 依存ライブラリ脆弱性 | 任意コード実行、情報漏洩 | High | dependency scan、lock、更新手順 | CI | 未対応 |
| TM-SC-02 | build/release改ざん | 配布物へのmalware混入 | Critical | protected release、checksum、SBOM、署名候補 | Release CI | 未対応 |
| TM-DB-01 | SQL injection・DB権限過多 | 永続データ侵害 | Critical | parameter binding、最小権限、secret分離 | security test | 未対応 |
| TM-OPS-01 | secret漏洩・危険な設定 | 全体侵害 | Critical | secret非出力、起動時検証、rotation | config/security test | 未対応 |
| TM-BACKUP-01 | backup漏洩・restore不能 | 情報漏洩、復旧失敗 | High | 暗号化、アクセス制御、restore訓練 | restore drill | 未対応 |

## 6. 対策の詳細

### 6.1 認証とtoken

**[SPEC]**

- passwordはArgon2id等の適切なpassword hashingで保存する（仕様 §8.1、§19.2）。
- 短命access tokenと長命refresh tokenを分離し、refresh tokenはhash保存とrotationを行う推奨モデルとする（仕様 §19.4）。
- logout、利用者無効化、password変更等で必要な失効を行う（仕様 §19.4）。
- 認証失敗応答でIDの存在有無を露出しない（仕様 §26.3）。

**[REC]**

- token検証は形式、暗号学的検証、期限、用途、失効状態を一つの境界で実施する。
- raw tokenを`realtime_presence`、domain object、ログ、メトリクスへ渡さない。
- refresh token reuseを検出した場合は、同一token familyを失効して監査イベントを記録する。
- login rate limitはIPだけに依存せず、正規化済みlogin ID等の主体軸を併用する。

**設計前提** ADR-002により15分のEd25519署名JWT、30日のrotating opaque Refresh Token、30秒のclock skew、60秒・単一使用のRealtime接続ticketを採用した。login lockout閾値はDM-05で別途確定する（`docs/adr/ADR-002-authentication-session.md`）。

### 6.2 認可と所有権

**[SPEC]** 全管理操作と状態変更はapplication境界で認証・認可する（仕様 §20、§26.3）。

**[REC]**

- transportが渡したrole、permission、ownerを認可根拠にしない。
- use caseごとに「認証済み主体、必要permission、対象資源、所有権」を評価する。
- denyをdefaultとし、未知permissionや欠落contextを許可へ倒さない。
- DB queryの絞り込みだけを認可境界にせず、application policyとして明示する。

### 6.3 Realtime、DoS、入力検証

**[SPEC]**

- 接続、利用者、IP、Instance単位でrate limitを適用可能にする（仕様 §18.4）。
- message size上限を設定し、遅いclient一台がInstance全体を遅延させない（仕様 §14.4、§18.1）。
- NaN、Infinity、範囲外値を拒否する（仕様 §9.7）。
- Reliable eventをsilent dropしない（仕様 §16.5）。

**[REC]**

- frame全体の上限をdecode前に、fieldごとの上限をDTO変換時に検証する。
- 接続単位のbounded queue、timeout、heartbeat、slow-consumer切断で障害を隔離する。
- parse error、rate limit超過、domain rejectionを区別して観測するが、内部情報をclientへ返さない。
- fuzz testはEnvelope decoder、DTO変換、Transform、sequence/revision境界を対象にする。

### 6.4 Snapshotと可視性

**[SPEC]** Snapshotと状態更新はInterest Management適用後の可視対象だけを送る（仕様 §15.3、§17）。

**[REC]**

- 可視性filterはserializationより前に適用し、送信後のclient側filterへ依存しない。
- Join、Resume、Resyncの全経路で同じVisibility Policyを使用する。
- permission変更、ownership移転、退室直後のcacheやreplay bufferから古い可視情報が漏れないことを検証する。
- telemetryへEntity payloadや可視集合全体を記録しない。

### 6.5 Webhook SSRF

**[SPEC]** Webhook宛先を制限する（仕様 §38）。

**[REC]**

- `https`のみを許可し、userinfo、曖昧なhost表現、非標準的なIP表現を拒否する。
- loopback、link-local、private、multicast、unspecified、予約済みIP rangeをIPv4/IPv6とも拒否する。
- DNS解決後の全addressを検証し、接続時にも解決結果と実接続先の不一致を防ぐ。
- redirectは無効化するか、各hopで同じ検証を再実施する。
- proxy環境では、宛先制限を迂回しないegress policyを併用する。
- response body size、接続/応答timeout、同時配送数を制限する。
- credential、内部header、不要な個人情報をWebhook payloadへ含めない。

**[ADR]** 公開allowlist、許可port、redirect方針、DNS pinning方式、egress proxy要件はDT-05で決める。

### 6.6 PostgreSQL、backup、secret

**[REC]**

- queryはparameter bindingを使用し、入力からSQL identifierを直接構築しない。
- runtime用DB roleとmigration用roleを分離し、通常運用でDDL権限を与えない。
- `DATABASE_URL`、署名鍵、Webhook credentialをrepository、image、logへ含めない。
- backupは本番データと同等の機密情報として暗号化・アクセス制御・保持期限を設定する。
- restoreは別環境で定期検証し、成功記録と所要時間を残す。
- secret変更時に再起動または安全なreloadが必要かを明示する。

### 6.7 監査とobservability

**[SPEC]** 管理操作、認証・認可上重要な操作、所有権変更を構造化監査可能にする（仕様 §20.4、§27）。

**[REC]**

- 監査記録にactor、action、target、result、timestamp、request/connection correlationを含める。
- password、raw token、Authorization header、完全payloadは含めない。
- application実行主体から監査保存済みデータの変更・削除権限を分離する。
- 監査出力失敗を観測し、重要操作を継続するか拒否するかを操作種別ごとに定義する。
- rate limit急増、token reuse、認可拒否急増、監査欠落をalert候補とする。

**[ADR]** append-onlyの実装、外部sink、保持期間、監査失敗時のfail-open/fail-closed方針を決める。

### 6.8 サプライチェーン

**[SPEC]** Release CIはdependency scan、container scan、SBOM、checksum等を扱う（仕様 §33）。

**[REC]**

- `Cargo.lock`とCI action/container参照を固定し、自動更新はreviewを通す。
- PR CIで脆弱性、license、禁止dependency、secret混入を検査する。
- release artifactをclean CI環境で生成し、source tag、checksum、SBOMを対応付ける。
- base imageとtoolchainを定期更新し、既知脆弱性の例外には期限と所有者を付ける。
- release権限、CI secret、registry権限を最小化する。

**[ADR]** artifact signing、provenance形式、registry、脆弱性severity gateはrelease/security ADRで決める。

## 7. セキュリティ検証

### 7.1 必須テスト

| 分類 | 最低限の検証 |
|---|---|
| Authentication | 不正password、不存在ID、lock/rate limit、期限切れ・失効・改ざんtoken、refresh reuse |
| Authorization | role/permission不足、自己申告owner、他利用者Entity、無効化利用者 |
| Realtime | 巨大frame、不正version、不正sequence/revision、NaN/Infinity、spam、slow consumer |
| Visibility | Join/Resume/Resyncごとの非可視Entity除外、permission変更直後 |
| SSRF | loopback、private、link-local、IPv6、DNS rebinding、redirect、巨大response、timeout |
| Database | SQL injection payload、runtime roleのDDL拒否、migration失敗時rollback |
| Audit | 必須field、secret非出力、改ざん/削除権限の拒否、sink停止 |
| Supply chain | dependency/container scan、SBOM、checksum、release artifact対応 |
| Recovery | backup取得、別環境restore、secret rotation、graceful shutdown |

### 7.2 合格条件

**[REC]**

1. High/Critical脅威に、所有者、実装対策、再現可能な検証が存在する。
2. security testは失敗を検出でき、単に正常系を通すだけではない。
3. rate/size/timeの具体値は設定とテストで同じ正本を参照する。
4. security controlを無効化した状態で対応テストが失敗することを確認する。
5. 受容する残存リスクは期限、理由、承認者、再評価条件を記録する。

## 8. 残存リスクと未決事項

| ID | 未決事項 | 一時方針 | 関連 |
|---|---|---|---|
| SEC-01 | token形式、期限、署名鍵管理 | **Accepted:** 15分Ed25519 JWT、30日rotated refresh、60秒Realtime ticket | ADR-002 |
| SEC-02 | TLS終端とtrusted proxy | topologyを確定せずproxy metadataを検証 | ADR-009 / DT-01 |
| SEC-03 | rate/size/queue上限 | boundedを必須とし具体値は負荷試験 | TB-06 / MRIB-08 |
| SEC-04 | Webhook宛先規則 | private系address拒否を最低線とする | DT-05 |
| SEC-05 | 監査のappend-only実装 | applicationから変更権限を分離 | security ADR |
| SEC-06 | artifact signing/provenance | checksumとSBOMを先行 | TC-07 / RL-07 |
| SEC-07 | backup暗号化・保持期間 | 環境別runbookで管理 | deployment ADR |
| SEC-08 | vulnerability acceptance | 期限付き例外のみ許可する案 | security policy |

## 9. 運用と更新

**[REC]** 次の場合に本書を更新する。

- 公開endpoint、protocol message、permission、Extension capabilityを追加・変更したとき
- token、TLS、DB、Webhook、audit、deployment topologyを変更したとき
- 新しいHigh/Critical脆弱性またはsecurity incidentが発生したとき
- dependency、base image、CI/release経路を大きく変更したとき
- 少なくとも各release候補のsecurity review時

変更時は次を記録する。

- 追加・変更した脅威ID
- リスク評価の変更理由
- 対策の実装箇所と所有者
- 検証方法と結果
- 受容した残存リスクと再評価期限

**[REC]** security incidentでは、証拠保全、token/secret失効、影響範囲特定、利用者通知判断、修正版公開、事後分析の順序をrunbook化する。非公開脆弱性報告手順はrepository rootの `SECURITY.md` を正本とする。

## 10. 参照

- `metaverse_core_specification.md` §19、§20、§26、§27、§33、§38
- `docs/design/auth-authorization.md`
- `docs/design/deployment-and-threat-model.md`
- `docs/design/extension-mechanism.md`
- `docs/design/mobile-resume-interest-backpressure.md`
- `docs/design/observability-and-config.md`
- `docs/design/test-and-ci.md`
- `docs/design/transport-boundaries.md`
