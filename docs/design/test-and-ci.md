# OrbiSync テスト戦略と CI/CD 設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §32（テスト戦略）と §33（CI/CD）を、unit/integration/contract/load/chaos/migration test、CI stages/cache/artifact/security/release gates の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `TC-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| モジュール境界、DAG、adapter/application/domain | `architecture.md` §2, §3 | 前提として参照。テストの所有権と境界 |
| 検証可能なアーキテクチャ適合条件 | `architecture.md` §9 | 前提として参照。CI 検査項目の根拠 |
| crate 構成、依存規則、コーディング規約 | `repo-crate-conventions.md` | 前提として参照。CI の lint/format/依存検査 |
| 負荷試験シナリオ、容量モデル、SLO 候補 | `scale-and-nfr.md` §9 | 前提として参照。load/soak test の合格基準 |
| Protocol 互換性、version negotiation | `transport-boundaries.md` §3.4、ADR-004 | 前提として参照。contract test の対象 |
| Migration 方針 | `rest-api-persistence.md`、TD-06 | 前提として参照。migration test の対象 |
| 決定論的テスト（Clock trait 注入） | `architecture.md` §9、TD-08、仕様 §31.4 | 前提として参照 |
| fake adapter による use case test | `architecture.md` §9.5 | 前提として参照 |
| testkit crate | `repo-crate-conventions.md` §2.2 | 前提として参照 |
| 負荷試験ツール | `architecture.md` §3（`load_test_tooling`）、`scale-and-nfr.md` §0.2 | 前提として参照。サーバー外から公開 API/protocol を駆動 |
| セキュリティ要件 | `scale-and-nfr.md` §7.3、TD-06 | 前提として参照。security test の対象 |
| 観測項目（metrics/log/trace） | `observability-and-config.md` §2〜§4 | 前提として参照。test での観測検証 |

## 1. テスト戦略の全体像

### 1.1 テストピラミッド

**[REC]** テストはピラミッド構造で構成する。下層ほど多く、上層ほど少なくする。

```text
         ╱╲
        ╱ E2E╲          少数（主要フローのみ）
       ╱──────╲
      ╱ Load / ╲        定期（nightly / release）
     ╱  Chaos   ╲
    ╱────────────╲
   ╱ Integration  ╲     中程度（PR CI で実行）
  ╱────────────────╲
 ╱  Contract / Prop ╲   中程度（PR CI で実行）
╱────────────────────╲
╱      Unit Test       ╲  多数（PR CI で実行）
╱────────────────────────╲
```

### 1.2 テスト種別と所有権

**[REC]** 各テスト種別の対象、所有権、実行タイミング：

| テスト種別 | 対象 | 所有 | 実行タイミング | 仕様根拠 |
|---|---|---|---|---|
| Unit | 値オブジェクト、権限計算、transform validation、sequence/revision、Interest、queue 集約、resume 判定 | 各 crate 内 `#[cfg(test)]` | PR CI（必須） | §32.1 |
| Integration | PostgreSQL repository、migration、REST API、WebSocket handshake、login→join→update→leave、reconnect→resume | `tests/integration/` | PR CI（必須） | §32.2 |
| Protocol Compatibility | 旧クライアント fixture、decode 互換、reserved field、breaking change、golden test vectors | `tests/protocol-compat/` | PR CI（必須） | §32.3 |
| Property | 任意の有限 Transform、encode/decode round trip、Spatial Grid 包含、権限組合せ、sequence order | 各 crate 内または `tests/` | PR CI（推奨） | §32.4 |
| Fuzz | Protobuf decoder、WebSocket frame、custom component validation、login ID parser、config parser | `tests/fuzz/` | Nightly | §32.5 |
| Load | 大量同時ログイン、段階的接続増加、10 Hz 移動、再接続混在、slow consumer、インスタンス集中/分散 | `tests/load/`（`load_test_tooling`） | Nightly / Release | §32.6 |
| Soak | 24〜72 時間連続実行、memory/connection/task leak、revision overflow、DB 接続枯渇、queue 増大 | `tests/soak/` | Release / 定期 | §32.7 |
| Chaos | DB 一時停止、ネットワーク遅延、パケットロス、SIGTERM、client 強制切断、proxy 再起動、clock skew | `tests/chaos/` | Nightly / Release | §32.8 |
| Migration | 既存 DB からの移行、Expand→Migrate→Contract、前方互換 | `tests/migrations/` | PR CI（必須） | §22.6 |
| Security | TLS 必須、rate limit、size limit、SQL injection、依存脆弱性 | `tests/security/` + CI | PR CI + Nightly | §26.3 |

**設計前提** `load_test_tooling` は Core 実行サーバー外から公開 API/protocol を駆動する。サーバー内部 module に依存しない（`architecture.md` §3、`scale-and-nfr.md` §0.2）。

## 2. テスト種別の設計

### 2.1 Unit Test

**[SPEC]** 仕様 §32.1 が定める unit test の対象：

- 値オブジェクト
- 権限計算
- transform validation
- sequence/revision
- Interest Management
- queue 集約
- resume 判定

**設計前提** fake adapter と決定論的 clock を用いる（`architecture.md` §9、仕様 §31.4）。

**[REC]** unit test の規約：

| 規約 | 理由 |
|---|---|
| I/O（DB、ネットワーク、ファイル）を使用しない | 高速・決定論的 |
| Clock trait を注入し、時刻を固定する | 仕様 §31.4、決定論的テスト |
| fake adapter で port を実装する | `architecture.md` §9.5 |
| テスト名は `test_<対象>_<条件>_<期待>` で命名する | 可読性 |
| 1 テストで 1 主張を原則とする | 失敗時の診断性 |

**[REC]** 主要な unit test の例：

| 対象 | テスト例 |
|---|---|
| `Transform` validation | NaN/Infinity の拒否、速度超過の検知、ワールド境界超過の検知 |
| `Interest` 可視集合 | 同一入力 view に対する同一出力（純粋関数）、ヒステリシス（30m/35m） |
| sequence 管理 | 単調増加、gap 検知、重複 drop |
| latest-wins queue | 同一 entity の上書き、reliable との分離 |
| resume 判定 | 履歴窓内の差分 replay、履歴欠落時の ResyncRequired |
| 権限計算 | RBAC の allow-only 評価、Permission 文字列のマッチング |

### 2.2 Integration Test

**[SPEC]** 仕様 §32.2 が定める integration test の対象：

- PostgreSQL repository
- migration
- REST API
- WebSocket handshake
- login → join → state update → leave
- reconnect → resume
- user disable → session revoke

**[SPEC]** Testcontainers 等により実 DB を使用する（仕様 §32.2）。

**[REC]** integration test の規約：

| 規約 | 理由 |
|---|---|
| 実 PostgreSQL を使用する（Testcontainers 等） | mock との実環境乖離を防止 |
| テストごとに DB を初期化する | テスト間の独立性 |
| REST API は HTTP クライアントで叩く | 公開契約の検証 |
| WebSocket は protocol クライアントで接続する | 公開 protocol の検証 |
| テスト用の設定（port、DB URL）を環境変数で注入する | 環境依存の排除 |

**[REC]** 主要な integration test のシナリオ：

| シナリオ | 手順 | 合格基準 |
|---|---|---|
| 認証フロー | login → token 取得 → /auth/me → refresh → logout | 各 step が正常応答。logout 後に token 無効 |
| 入室フロー | login → WS接続 → ClientHello → JoinInstance → Snapshot受信 → `active` | SnapshotにInterest適用後の状態が含まれる |
| 状態更新 | 入室 → TransformInput 送信 → StateDelta 受信 | 更新が反映された StateDelta が届く |
| 退室フロー | 入室 → Leave → 接続維持 → 別インスタンスへ再入室 | Leave 後に状態がクリアされる |
| 再接続 | 入室 → 切断 → バックオフ → 再接続 → ResumeSession → 差分 replay | resume 成功、revision 一致 |
| ユーザー無効化 | login → admin が user disable → 既存セッションの失効確認 | disable 後に token 無効、WS 切断 |
| 所有権 | entity spawn → owner 以外が更新 → ErrorMessage | 所有権違反が拒否される |

**[ADR]** Testcontainers の Rust crate 選定（`testcontainers`、`testcontainers-rs` 等）、コンテナイメージのバージョン固定は TC-01 で決める。

### 2.3 Protocol Compatibility Test

**[SPEC]** 仕様 §32.3 が定める protocol compatibility test の対象：

- 旧クライアント fixture を保存
- 新サーバーで decode 可能か
- reserved field number を再利用していないか
- breaking change を CI で検出
- Golden test vectors を SDK 間で共有

**設計前提** `.proto` を公開契約の正本とする（TD-04）。field number を再利用しない、削除 field は reserved とする（仕様 §34.4、`transport-boundaries.md` §3.4）。

**[REC]** protocol compatibility test の構成：

| テスト | 方法 | 実行タイミング |
|---|---|---|
| decode 互換 | `sdk/test-vectors/protocol/` の golden binary を新サーバーで decode | PR CI |
| reserved field 検査 | `.proto` の lint（buf lint 等）で reserved の再利用を検出 | PR CI |
| breaking change 検出 | `.proto` の差分検査（buf breaking 等）で破壊的変更を検出 | PR CI |
| round trip | encode → decode → encode の結果が一致する | PR CI |
| SDK 間共有 | golden test vectors を `sdk/test-vectors/` に配置し、TypeScript SDK のテストでも使用する | PR CI |

**設計前提** schema lint/breaking change検査はADR-004によりBufを使用する。

### 2.4 Property Test

**[SPEC]** 仕様 §32.4 が定める property test の対象：

- 任意の有限 Transform
- encode/decode round trip
- Spatial Grid の包含条件
- 権限組合せ
- sequence order

**[REC]** property test は unit test を補完する。ランダム生成された入力に対して不変条件が成り立つことを検証する。

**[REC]** 主要な property：

| 対象 | 不変条件 |
|---|---|
| Transform | 有限値の Transform は常に validation を通過する。NaN/Infinity は常に拒否される |
| encode/decode | 任意の valid メッセージに対して `decode(encode(m)) == m` |
| Spatial Grid | entity が cell に登録されている ⇔ entity の位置が cell の範囲内にある |
| 権限 | Permission が Role に含まれる ⇔ その Role を持つ User が権限を持つ |
| sequence | 任意の送信列に対して sequence は単調増加する |

**[ADR]** property test framework（`proptest`、`quickcheck` 等）の選定は TC-02 で決める。

### 2.5 Fuzz Test

**[SPEC]** 仕様 §32.5 が定める fuzz test の対象：

- Protobuf decoder
- WebSocket frame 処理
- custom component validation
- login ID parser
- configuration parser

**[REC]** fuzz test は nightly CI で短時間実行し、release 前に長時間実行する。

**[REC]** fuzzing の対象と方法：

| 対象 | 方法 |
|---|---|
| Protobuf decoder | 任意バイト列を Envelope として decode。panic しないこと |
| WebSocket frame | 任意フレームを gateway で処理。panic しないこと、接続切断で済むこと |
| custom component | 任意の component payload を validation。panic しないこと |
| login ID parser | 任意文字列を LoginId として parse。panic しないこと |
| config parser | 任意 TOML を parse。panic しないこと、検証エラーで済むこと |

**[ADR]** fuzzing framework（`cargo-fuzz`、`afl.rs` 等）の選定は TC-03 で決める。

### 2.6 Load Test

**[SPEC]** 仕様 §32.6 が定める load test のシナリオ：

1. 大量同時ログイン
2. 段階的接続増加
3. 全員が 10 Hz で移動
4. 10% が頻繁に再接続
5. slow consumer 混在
6. 1 インスタンス集中
7. 複数インスタンス分散

**[SPEC]** 仕様 §32.6 が定める記録項目：

- CPU
- RSS memory
- network throughput
- P50/P95/P99 latency
- dropped update
- queue depth
- disconnect
- snapshot size

**設計前提** 負荷試験は Core 実行サーバー外から `load_test_tooling` で駆動する（`architecture.md` §3、`scale-and-nfr.md` §0.2）。試験環境と測定条件を必ず記録する（仕様 §26.2、`scale-and-nfr.md` §9.1）。

**設計前提** Phase 1/2 の合格基準は `scale-and-nfr.md` §9.2, §9.3 が定める。

**[REC]** load test の実行構成：

```text
load-generator（apps/load-generator）
  │ 公開 API / protocol（WSS + HTTPS）
  ▼
OrbiSync サーバー（被测対象）
  │
  ├── PostgreSQL
  └── Prometheus（metrics scrape）
```

**[REC]** load test は nightly CI で smoke test（短時間・低負荷）、release CI で full test（長時間・高負荷）を実行する。

### 2.7 Soak Test

**[SPEC]** 仕様 §32.7 が定める soak test：

24〜72 時間連続で実行し、以下を確認する：

- memory leak
- connection leak
- task leak
- revision overflow 兆候
- DB connection 枯渇
- queue 増大

**設計前提** Phase 1 の soak test は 50 クライアントで 24 時間（`scale-and-nfr.md` §9.2 P1-03）。

**[REC]** soak test の判定基準：

| 項目 | 判定基準 |
|---|---|
| RSS memory | 増加率が 5% 以内（24 時間） |
| Tokio task 数 | 増加率が 5% 以内 |
| DB 接続数 | プール上限を超えない |
| エラーレート | 安定（増加傾向なし） |
| tick 処理時間 | 安定（増加傾向なし） |

**[REC]** soak test は release CI または定期スケジュールで実行する。PR CI では実行しない（時間がかかりすぎるため）。

### 2.8 Chaos Test

**[SPEC]** 仕様 §32.8 が定める chaos test の対象：

- DB 一時停止
- ネットワーク遅延
- パケットロス
- プロセス SIGTERM
- client 強制切断
- reverse proxy 再起動
- clock skew

**設計前提** failure isolation の階層と障害モードは `scale-and-nfr.md` §8 が定める。

**[REC]** chaos test のシナリオと合格基準：

| シナリオ | 注入方法 | 合格基準 |
|---|---|---|
| DB 一時停止 | PostgreSQL の停止/再開 | ready=false。一時状態は継続。DB 復旧後に永続化再開（`scale-and-nfr.md` §8.2） |
| ネットワーク遅延 | tc/netem 等で遅延注入 | heartbeat timeout 内で吸収。接続維持 |
| パケットロス | tc/netem 等でロス注入 | latest-wins で吸収。reliable は再送 |
| SIGTERM | プロセスへ SIGTERM 送信 | graceful shutdown。drain 後に終了（`architecture.md` §4.3） |
| client 強制切断 | TCP reset | resume grace 内に再接続可能。他接続に影響なし |
| proxy 再起動 | reverse proxy の再起動 | 再接続で回復。他接続に影響なし |
| clock skew | コンテナの clock をずらす | heartbeat の誤判定がない。timestamp 検証で検知 |

**[REC]** chaos test は nightly CI で軽微なシナリオ、release CI で全シナリオを実行する。

**[ADR]** chaos test は外部の専用ツールを導入せず、テストコード内で障害を注入する。DB 停止は PostgreSQL コンテナの停止・再開、instance panic はテスト専用フックを使う。P2-04/P2-05 の所有と判定は [ADR-022](../adr/ADR-022-chaos-testing.md) および E-4 テストを参照する。

### 2.9 Migration Test

**[SPEC]** リリース前に既存 DB からの移行テストを行う（仕様 §22.6）。

**設計前提** 前方移行を基本とし、Expand → Migrate → Contract 方式を推奨する（TD-06、仕様 §22.6）。

**[REC]** migration test の構成：

| テスト | 方法 |
|---|---|
| 新規 migration | 空 DB に全 migration を適用し、schema が正しいことを検証 |
| 既存 DB からの移行 | 前バージョンの schema に fixture データを投入し、新 migration を適用。データが正しいことを検証 |
| 前方互換 | 新 schema で旧バージョンのサーバーが起動できることを検証（Expand 段階） |
| rollback / recovery | Contract前にbackup restore、Expand schemaでの旧binary起動、forward corrective migrationを検証する。down migration fileは要求しない（ADR-017） |

**[REC]** migration test は PR CI で実行する。migration ファイルの変更がある PR では必須とする。

### 2.10 Security Test

**設計前提** セキュリティ要件は `scale-and-nfr.md` §7.3、TD-06 が定める。

**[REC]** security test の構成：

| テスト | 方法 | 実行タイミング |
|---|---|---|
| TLS 必須 | 平文接続の拒否をテスト | PR CI |
| rate limit | 閾値超過時の拒否/切断を検証 | PR CI |
| request size limit | 上限超過メッセージの拒否をテスト | PR CI |
| SQL injection | パラメータ化クエリの compile-time check（SQLx） | PR CI（compile-time） |
| 依存脆弱性 | `cargo deny` / `cargo audit` | PR CI + Nightly |
| コンテナ権限 | OCI image の USER 指令と runtime 検証 | Release CI |
| CORS | デフォルト拒否の確認 | PR CI |
| origin 検証 | WebSocket upgrade 時の origin 検証 | PR CI |

## 3. CI/CD

### 3.1 PR CI

**[SPEC]** 仕様 §33.1 が定める PR CI の必須項目：

1. format
2. clippy
3. unit tests
4. integration tests
5. documentation build
6. dependency license check
7. vulnerability audit
8. protocol compatibility
9. generated code 差分確認
10. migration check

**[REC]** PR CI の stage 構成：

```text
PR CI
├── Stage 1: Static Analysis（並行）
│   ├── cargo fmt --check
│   ├── cargo clippy --all-targets --all-features -- -D warnings
│   ├── cargo deny check（license + advisory）
│   ├── cargo doc --no-deps（documentation build）
│   └── proto lint / breaking change 検査
│
├── Stage 2: Build & Unit Test（並行）
│   ├── cargo build --workspace
│   ├── cargo test --workspace（unit tests）
│   ├── property tests
│   └── generated code 差分確認
│
├── Stage 3: Integration Test（Stage 2 依存）
│   ├── PostgreSQL container 起動
│   ├── migration check
│   ├── integration tests（REST + WebSocket）
│   ├── protocol compatibility tests
│   └── security tests（TLS, rate limit, size limit）
│
└── Stage 4: Summary
    └── 全 stage の結果を集約し、PR status を決定
```

**[REC]** Stage 1 と Stage 2 は並行実行してよい。Stage 3 は Stage 2 のビルド成果物を再利用する。

**[REC]** PR CI の所要時間目標：15 分以内（Stage 1〜3 合計）。超過する場合はテストの並行化または分割を検討する。

**[ADR]** CI プラットフォーム（GitHub Actions 等）、runner の仕様、cache の構成は TC-05 で決める。

### 3.2 Nightly CI

**[SPEC]** 仕様 §33.2 が定める nightly CI の項目：

- load smoke test
- fuzz 短時間実行
- sanitizer 可能範囲
- unused dependency check
- documentation link check
- container scan

**[REC]** nightly CI の構成：

```text
Nightly CI
├── load smoke test（短時間・低負荷）
├── fuzz test（短時間、各対象 5 分程度）
├── sanitizer（ASan/LSan/TSan、対応範囲）
├── cargo machete（unused dependency check）
├── documentation link check（lychee 等）
├── container scan（Trivy 等）
└── soak test（24 時間、定期スケジュール）
```

**[REC]** nightly CI の失敗は PR CI とは異なり、merge をブロックしない。失敗は issue として記録し、次リリースまでに修正する。

**[ADR]** sanitizer の対象範囲（Rust の safe コードでは ASan/LSan のみ有効等）、container scan ツールの選定は TC-06 で決める。

### 3.3 Release CI

**[SPEC]** 仕様 §33.3 が定める release CI の項目：

- tag 検証
- changelog 検証
- reproducible build に近い手順
- Linux amd64/arm64 image
- SBOM 生成
- checksum
- container signing を推奨
- GitHub Release 生成
- migration notes 添付

**[REC]** release CI の構成：

```text
Release CI（tag push で起動）
├── Stage 1: Validation
│   ├── tag の形式検証（semver）
│   ├── changelog の検証（tag との一致）
│   └── PR CI の全項目を再実行
│
├── Stage 2: Build
│   ├── cargo build --release（Linux amd64）
│   ├── cargo build --release（Linux arm64、cross-compile）
│   ├── OCI image build（amd64 + arm64）
│   └── SBOM 生成
│
├── Stage 3: Test
│   ├── full load test（Phase 2 シナリオ）
│   ├── full chaos test
│   ├── soak test（24 時間以上）
│   └── migration test（前バージョン DB から）
│
├── Stage 4: Publish
│   ├── checksum 生成（SHA256）
│   ├── container signing（推奨）
│   ├── GitHub Release 生成
│   ├── migration notes 添付
│   └── OCI image push（registry）
│
└── Stage 5: Post-release
    └── release の動作確認（smoke test）
```

**[REC]** release CI の Stage 3（load/chaos/soak）は時間がかかるため、Stage 1/2 と並行開始してよい。

**[ADR]** reproducible build の達成度、container signing の方式（cosign 等）、OCI registry の選定は TC-07 で決める。

### 3.4 CI の Cache と Artifact

**[REC]** CI の cache 方針：

| cache 対象 | 方法 | 効果 |
|---|---|---|
| cargo registry | `~/.cargo/registry` の cache | 依存 crate の再ダウンロード回避 |
| cargo build | `target/` の cache | 再ビルドの回避 |
| Rust toolchain | `rust-toolchain.toml` で固定 | 再現性の確保 |
| proto 生成物 | 生成結果の cache | 再生成の回避（差分確認時のみ再生成） |
| test DB image | コンテナイメージの pull cache | DB 起動の高速化 |

**[REC]** CI の artifact：

| artifact | 保持期間 | 用途 |
|---|---|---|
| テストレポート | 30 日 | 失敗の診断 |
| load test 結果 | 90 日 | 性能の経年比較 |
| OCI image | release のみ永続 | デプロイ |
| SBOM | release のみ永続 | サプライチェーン追跡 |

**[ADR]** cache のサイズ上限、artifact の保存先（GitHub Actions cache/artifact、S3 等）は TC-05 で決める。

### 3.5 Security Gates

**[REC]** CI の security gate：

| gate | 実行タイミング | ブロック条件 |
|---|---|---|
| `cargo deny check advisories` | PR CI | 既知脆弱性の検出（Critical/High） |
| `cargo deny check licenses` | PR CI | 非互換ライセンスの検出 |
| `cargo audit` | PR CI + Nightly | 既知脆弱性の検出 |
| container scan | Nightly + Release | Critical/High 脆弱性の検出 |
| secret scan | PR CI | リポジトリ内の secret 検出 |

**[REC]** Critical/High の脆弱性は PR の merge をブロックする。Medium/Low は警告とし、次リリースまでの修正を推奨する。

**[ADR]** 脆弱性修正 SLA（Critical 7 日、High 30 日）の運用プロセスは SN-05 / `scale-and-nfr.md` §7.3 で決める。

### 3.6 Release Gates

**[REC]** release の gate：

| gate | 条件 |
|---|---|
| PR CI 全項目 pass | merge の必要条件 |
| nightly CI の直近実行が pass | release の必要条件 |
| load test の合格基準達成 | `scale-and-nfr.md` §9 の Phase 基準 |
| soak test の合格基準達成 | `scale-and-nfr.md` §9.4 の判定基準 |
| migration test pass | 既存 DB からの移行が正常 |
| changelog の更新 | release notes の準備 |
| tag の形式 | semver に準拠 |

**[REC]** release は tag push で起動する。tag は maintainer のみが push できる（CODEOWNERS / branch protection）。

## 4. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 4.1 Unit / Integration

1. unit test が I/O なしで実行され、全テストの合計が 5 分以内で完了する。
2. integration test が実 PostgreSQL（Testcontainers）で実行され、login → join → state update → leave のフローが正常に完了する。
3. reconnect → resume の integration test が、差分 replay 後に revision 一致で完了する。

### 4.2 Protocol Compatibility

4. `sdk/test-vectors/protocol/` の golden binary を新サーバーで decode できる。
5. `.proto` の reserved field number が再利用されていない（lint で検出）。
6. encode → decode → encode の round trip が一致する。

### 4.3 Load / Soak

7. Phase 1 load test（50 クライアント、10 Hz）が合格基準（`scale-and-nfr.md` §9.2）を満たす。
8. 24 時間 soak test で RSS memory 増加率が 5% 以内である。

### 4.4 Chaos / Migration

9. DB 一時停止中にリアルタイム一時状態の処理が継続し、DB 復旧後に永続化が再開する。
10. SIGTERM 後に graceful shutdown が完了し、active connection が drain される。
11. 前バージョンの DB schema に新 migration を適用し、データが正しく移行される。

### 4.5 CI

12. PR CI の全必須項目（§3.1 の 10 項目）が pass しない PR は merge できない。
13. release CI の load/chaos/soak test が合格基準を満たさない場合、release がブロックされる。
14. Critical/High の脆弱性が検出された PR は merge できない。

## 5. PR チェックリスト

**[REC]** 仕様 §44 が示す PR チェックリスト案を、PR テンプレートとして採用してよい。以下は仕様例であり、確定したテンプレートではない。

```markdown
- [ ] Issue または RFC へリンクした
- [ ] 変更範囲を説明した
- [ ] Unit test を追加した
- [ ] Integration test を追加または不要理由を記載した
- [ ] Protocol 互換性を確認した
- [ ] DB migration の後方互換性を確認した
- [ ] セキュリティ影響を確認した
- [ ] メトリクス/ログへの影響を確認した
- [ ] ドキュメントを更新した
- [ ] CHANGELOG 対象か確認した
- [ ] Generated code を再生成した
- [ ] 負荷影響がある場合ベンチマークを添付した
```

**[REC]** 上記チェックリストは `.github/PULL_REQUEST_TEMPLATE.md` に配置する（`repo-crate-conventions.md` §1.2）。

## 6. 要 ADR 事項

本書が主担当となる判断を TC ID で管理する。他文書が正本の判断（ADR-004、ADR-005、TD-13、SN-05 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| TC-01 | Testcontainers の Rust crate 選定とコンテナイメージのバージョン固定 | `testcontainers` crate、PostgreSQL イメージは対応バージョンで固定 | 仕様 §32.2 の実 DB 使用。具体 crate は評価後に確定 |
| TC-02 | property test framework の選定 | `proptest` | 仕様 §32.4。 shrinking と永続的 failure case の保存を重視 |
| TC-03 | fuzzing framework の選定 | `cargo-fuzz`（libFuzzer ベース） | 仕様 §32.5。Rust ecosystem との統合しやすさ |
| TC-04 | chaos test の注入ツール | `toxiproxy`（ネットワーク）、`stress-ng`（リソース） | 仕様 §32.8。Docker Compose との統合しやすさ |
| TC-05 | CI プラットフォーム、runner 仕様、cache/artifact 構成 | GitHub Actions（仕様 §29 の例に `.github/workflows/` あり） | 仕様 §29 のリポジトリ構成例と整合。具体 runner/cache は評価後に確定 |
| TC-06 | sanitizer の対象範囲、container scan ツール | ASan/LSan（safe コード対象）、Trivy | 仕様 §33.2。Rust の safe コードでは TSan の効果は限定的 |
| TC-07 | reproducible build の達成度、container signing、OCI registry | reproducible build は best-effort、cosign で signing、registry は運用 ADR | 仕様 §33.3 の「推奨」を具体化。確定は運用環境に依存 |
