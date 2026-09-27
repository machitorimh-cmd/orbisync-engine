# OrbiSync リリース・互換性・OSS メンテナンス・ライセンス設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §34（リリースと互換性）、§35（OSS メンテナンス方針）、§36（ライセンス案）を、versioning/schema evolution/deprecation/release artifact/SBOM、maintenance policy/issue-security/release process、ライセンス候補の未決管理の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `RL-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| Protocol 互換性、field number、version negotiation | `transport-boundaries.md` §3.4、RP 書 §2.3、ADR-004 | 前提として参照。protocol 変更ルールの技術詳細は同書へ委ねる |
| REST API versioning、OpenAPI | `rest-api-persistence.md`、TD-05、ADR-003 | 前提として参照 |
| Migration 方針 | `repo-crate-conventions.md` §5、TD-06、ADR-005 | 前提として参照 |
| Release CI、artifact、SBOM | `test-and-ci.md` §3.3 | 前提として参照。本書は release process の意味論を定義 |
| SDK 互換性 | `client-sdk.md` §9 | 前提として参照 |
| 依存関係とサプライチェーン | TD-05、TD-13 | 前提として参照 |
| セキュリティ報告、脆弱性 SLA | `scale-and-nfr.md` §7.3、SN-05 | 前提として参照 |
| 命名と識別子（プロジェクト名） | ADR-001 | Accepted。公開名`OrbiSync`、package接頭辞`orbisync-*` |

## 1. リリースと互換性

### 1.1 Semantic Versioning

**[SPEC]** 仕様 §34.1 が定める Semantic Versioning：

| 変更種別 | バージョン |
|---|---|
| 公開 API または protocol major の破壊的変更 | MAJOR |
| 後方互換機能追加 | MINOR |
| バグ修正、セキュリティ修正 | PATCH |

**[REC]** semver の適用対象はサーバーバイナリのリリースバージョンとする。各サブシステムのバージョンは §1.2 で独立管理する。

### 1.2 バージョンの独立管理

**[SPEC]** 仕様 §34.2 が定める独立管理対象：

| バージョン | 管理対象 | 変更のトリガー |
|---|---|---|
| Server Version | サーバーバイナリ全体 | リリースごと |
| REST API Version | `/v1` 配下の公開契約 | OpenAPI の変更 |
| Realtime Protocol Version | `.proto` の Envelope/payload | protocol 変更 |
| Database Schema Version | migration の通番 | schema 変更 |
| TypeScript SDK Version | `@orbisync/client` package | SDK リリースごと |
| Extension API Version | Webhook/拡張の公開契約 | 拡張 API 変更 |

**[REC]** 各バージョンは独立して増加する。Server Version の MINOR 増加が REST API Version の増加を意味しない。

**[REC]** バージョンの対応表をリリースごとに文書化する。例：Server 0.3.0 は REST API v1、Protocol v1.2、DB Schema 0005、SDK 0.2.0、Extension API v1 と互換。

**[ADR]** 各バージョンの表記形式（semver の適用範囲、protocol version の major.minor 表記等）は RL-01 で決める。

### 1.3 Deprecation

**[SPEC]** 仕様 §34.3 が定める deprecation の確定要件：

- 非推奨化を先に行う
- 削除予定バージョンを明記
- telemetry で旧機能利用を確認可能にする

**[REC]** 少なくとも 1 MINOR 期間の移行猶予を設ける（仕様 §34.3 の推奨）。

**[REC]** deprecation の手順：

1. 非推奨の告知（CHANGELOG、ドキュメント、ログ警告）
2. telemetry で旧機能の利用状況を計測
3. 移行ガイドの提供
4. 1 MINOR 期間以上の猶予
5. 削除（MAJOR リリース）

**[REC]** 非推奨の告知は、該当機能の使用時に warning ログを出力することで行う。telemetry のメトリクスで旧機能の利用回数を集計し、削除前に利用者がいないことを確認する。

**[ADR]** 移行猶予期間の確定値（「少なくとも 1 MINOR」の具体期間）は RL-02 で決める。

### 1.4 Protocol 変更ルール

**[SPEC]** 仕様 §34.4 が定める protocol 変更ルール：

- field number を再利用しない
- 削除 field は reserved にする
- required 相当の後付けを避ける
- unknown field を許容する
- enum 追加を想定する
- major negotiation を実装する

**設計前提** 上記の技術詳細は `transport-boundaries.md` §3.4、RP 書 §2.3、ADR-004 が定める。本書は release process との整合のみを扱う。

**[REC]** protocol の破壊的変更は MAJOR リリースでのみ行う。MINOR/PATCH では後方互換の変更（field 追加、enum 追加）のみ許可する。

### 1.5 Release Artifact と SBOM

**設計前提** release CI は SBOM 生成、checksum、container signing（推奨）、GitHub Release 生成、migration notes 添付を行う（`test-and-ci.md` §3.3、仕様 §33.3）。

**[REC]** release artifact の候補構成。正式な support matrix（対象 OS/arch、bare binary の配布有無）は release policy として ADR で決める。

| Artifact | 形式 | 内容 |
|---|---|---|
| サーバーバイナリ | 少なくとも Linux で検証可能 | 単一バイナリ（`architecture.md` §1） |
| OCI image | 少なくとも Linux ベースで検証可能 | サーバー + 最小 runtime |
| SBOM | SPDX または CycloneDX | 依存関係の全リスト |
| checksum | SHA256 | バイナリ/image の整合性検証 |
| migration notes | Markdown | DB migration の注意事項 |
| changelog | Markdown | リリースの変更点 |

**[REC]** OCI image のタグは固定し、`latest` を本番で推奨しない（仕様 §37.2）。

**[ADR]** 正式な support matrix（Linux amd64/arm64、OCI multi-arch、他 OS の対応可否）、SBOM の形式（SPDX vs CycloneDX）、container signing の方式（cosign 等）、OCI registry の選定は TC-07 / RL-07 / `test-and-ci.md` §3.3 で決める。bare binary の配布や他 OS 対応を暗黙に排除しない。

## 2. OSS メンテナンス方針

### 2.1 必須ドキュメント

**[SPEC]** 仕様 §35.1 が定める必須ドキュメント：

| ドキュメント | 用途 |
|---|---|
| README.md | プロジェクト概要、起動手順 |
| CONTRIBUTING.md | 貢献ガイド、開発環境構築 |
| CODE_OF_CONDUCT.md | 行動規範 |
| SECURITY.md | セキュリティ報告方法 |
| GOVERNANCE.md | ガバナンスモデル |
| MAINTAINERS.md | メンテナーリスト |
| CHANGELOG.md | 変更履歴 |
| LICENSE | ライセンス本文 |
| Roadmap | 将来計画 |
| Architecture Decision Records | 設計判断の記録 |

**[REC]** 上記はリポジトリのルートまたは `docs/` に配置する（`repo-crate-conventions.md` §1.2）。

### 2.2 Governance

**[REC]** 仕様 §35.2 が示す初期ガバナンス案：

| ロール | 権限 |
|---|---|
| Maintainer | merge 権限、release 権限、セキュリティ対応 |
| Reviewer | PR レビュー、承認 |
| Contributor | PR 提出、Issue 起票 |

**[REC]** 権限昇格条件の文書化（仕様 §35.2 の例）：

- 継続的な質の高い貢献
- レビュー実績
- 行動規範の遵守
- 特定モジュールへの理解
- セキュリティ情報の取扱能力

**[ADR]** ガバナンスモデルの確定（BDFL vs 合議制、昇格の具体的基準）は RL-03 で決める。仕様 §35.2 は初期案であり確定ではない。

### 2.3 CODEOWNERS

**[SPEC]** 高リスク領域は明示的な owner 承認を必要とする（仕様 §35.3）。

**[REC]** 仕様 §35.3 が示す CODEOWNERS の例：

```text
/proto/                 @protocol-maintainers
/migrations/            @database-maintainers
/crates/auth/           @security-maintainers
/crates/realtime/       @realtime-maintainers
/.github/workflows/     @release-maintainers
/SECURITY.md            @security-maintainers
```

**[REC]** 上記は仕様例である。実際のpackage pathはADR-001の`orbisync-*`命名と`repo-crate-conventions.md` §2に従って最終化する。

### 2.4 Issue Labels

**[REC]** 仕様 §35.4 が示す issue labels の例を採用してよい。ラベル体系はプロジェクトの成長に応じて見直す。

### 2.5 変更提案

**[SPEC]** 仕様 §35.5 が定める RFC または ADR 必須の変更：

- protocol 変更
- DB 破壊的変更
- 認証方式変更
- plugin model 変更
- 依存方向変更
- 新しいネットワーク transport
- unsafe 導入
- ライセンス変更

**[REC]** 小変更は Issue/PR で扱う。上記に該当する変更は RFC または ADR を作成し、関係 owner の承認を得てから実装する。

### 2.6 セキュリティ報告

**[SPEC]** 仕様 §35.6 が定めるセキュリティ報告のプロセス：

- 公開 Issue へ脆弱性を書かせない
- SECURITY.md に非公開報告方法を記載
- 受付確認
- 影響調査
- 修正版準備
- advisory 公開
- CVE 取得を検討

**設計前提** 脆弱性修正 SLA（Critical 7 日、High 30 日）は `scale-and-nfr.md` §7.3 / SN-05 が定める。

**[REC]** セキュリティ報告のフロー：

```text
報告者 → SECURITY.md の非公開経路（GitHub Security Advisory 等）
  → Maintainer が受付確認（48 時間以内）
  → 影響調査（严重度の判定）
  → 修正版準備（非公開ブランチ）
  → advisory 公開（修正リリースと同時）
  → CVE 取得（該当する場合）
```

**[ADR]** 非公開報告の具体的手段（GitHub Security Advisory、メールアドレス、Keybase 等）は RL-04 で決める。

### 2.7 Release Process

**[REC]** release の手順：

1. release ブランチの作成（main から）
2. CHANGELOG の最終化
3. バージョン番号の確定（§1.2 の対応表を含む）
4. release CI の実行（`test-and-ci.md` §3.3）
5. release gate の確認（`test-and-ci.md` §3.6）
6. tag の付与（semver）
7. release の公開（GitHub Release、OCI image push）
8. release の告知（CHANGELOG、ドキュメント）

**[REC]** security release は通常の手順を短縮し、修正の準備が整い次第 advisory と同時に公開する。

## 3. ライセンス

### 3.1 ライセンス決定

**[SPEC]** 仕様 §36 が示すライセンス候補：

| 案 | ライセンス | 特徴 |
|---|---|---|
| 案 A | Apache-2.0 | 商用利用しやすい、特許条項が明確、企業導入との相性がよい |
| 案 B | MIT OR Apache-2.0 | Rust エコシステムで一般的なデュアルライセンス。利用者が選択可能 |

**[REC]** RL-05をAcceptedとし、`MIT OR Apache-2.0`（案B）を採用する。利用者はMIT LicenseまたはApache License 2.0のいずれかを選択できる。決定記録は `docs/adr/RL-05-license.md` を正本とする。

**[REC]** 仕様 §36 の検討メモ：

- AGPL は改変公開を強く求められる一方、企業導入の障壁になり得る
- 導入支援や個別フロント開発を収益源にする場合、Apache-2.0 系の方が普及しやすい
- 名称、ロゴ、公式サービス名はソフトウェアライセンスと別に商標ポリシーを定めてよい

**[REC]** ライセンス決定後も以下を継続して整理する：

| 整理項目 | 内容 |
|---|---|
| 依存 crate のライセンス互換性 | `cargo deny check licenses` で検査（`test-and-ci.md` §3.5） |
| 貢献者ライセンス契約（CLA/DCO） | 要否と方式 |
| 商標ポリシー | 名称・ロゴの使用規則 |
| 特許条項 | Apache-2.0 の特許授与と終了条件 |

**[ADR]** CLA/DCO の要否と方式、商標ポリシーの内容は RL-06 で決める。

### 3.2 ライセンスの適用

**[REC]** デュアルライセンスを次のように適用する：

- repository rootに `LICENSE-MIT` と `LICENSE-APACHE` を配置する
- Cargo packageには `license = "MIT OR Apache-2.0"` を指定する
- release artifactへ両ライセンス本文を同梱する
- 依存 crate のライセンス検査は CI で継続する
- contributionは別途明示しない限り同じデュアルライセンスで提供される旨をCONTRIBUTINGへ記載する
- ライセンス変更は仕様 §35.5 の RFC/ADR 必須変更である

## 4. テスト可能な受入条件

**[REC]** 実装は次の受入条件を満たすことをテストで示す。

### 4.1 リリースと互換性

1. release CI が semver に準拠した tag を検証し、不正な tag を拒否する。
2. protocol の破壊的変更が MINOR/PATCH リリースに含まれない（CI の breaking change 検出）。
3. 非推奨機能の使用時に warning ログが出力され、telemetry で利用回数が集計される。
4. release artifact に SBOM、checksum、migration notes が含まれる。
5. OCI image のタグが固定されており、`latest` が本番例で使用されない。

### 4.2 OSS メンテナンス

6. 必須ドキュメント（§2.1 の 10 項目）がリポジトリに存在する。
7. CODEOWNERS で高リスク領域（proto、migrations、auth、realtime、workflows、SECURITY.md）の owner 承認が必須となっている。
8. セキュリティ報告が公開 Issue ではなく非公開経路で受け付けられる。
9. 仕様 §35.5 の RFC/ADR 必須変更が、RFC/ADR なしで merge できない（CODEOWNERS + PR テンプレートで強制）。

### 4.3 ライセンス

10. `cargo deny check licenses` が CI で実行され、非互換ライセンスが検出される。
11. `LICENSE-MIT`と`LICENSE-APACHE`が存在し、package metadataとCONTRIBUTINGのライセンス表記が `MIT OR Apache-2.0` で一致する。

## 5. 要 ADR 事項

本書が主担当となる判断を RL ID で管理する。他文書が正本の判断（ADR-001、ADR-003、ADR-004、TC-07、SN-05 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| RL-01 | 各バージョンの表記形式 | Server/SDK/Extension API は semver、Protocol は major.minor、DB Schema は migration 通番 | 仕様 §34.2 の独立管理。表記の統一は運用の明確化 |
| RL-02 | deprecation の移行猶予期間 | 少なくとも 1 MINOR 期間（仕様 §34.3 の推奨）。具体期間は release cycle に依存 | 仕様 §34.3 は「推奨」。確定は運用実績で調整 |
| RL-03 | ガバナンスモデルの確定 | 初期は Maintainer 合議制。BDFL は置かない | 仕様 §35.2 は初期案。OSS としての持続可能性 |
| RL-04 | セキュリティ報告の非公開経路 | GitHub Security Advisory | 仕様 §35.6。GitHub を使う場合の標準機能 |
| RL-05 | ライセンスの確定 | **Accepted: MIT OR Apache-2.0（案 B）** | `docs/adr/RL-05-license.md`。Rustエコシステムとの整合、利用者の選択、Apache-2.0の明示的な特許条項 |
| RL-06 | CLA/DCO の要否と商標ポリシー | DCO（Developer Certificate of Origin）を推奨。商標ポリシーは別途策定 | 貢献の法的明確化。名称保護 |
| RL-07 | 正式な release support matrix（対象 OS/arch、bare binary 配布、OCI multi-arch） | 少なくとも Linux/OCI で検証可能。正式 matrix は release policy で確定 | 仕様 §33.3 は Linux amd64/arm64 image を例示するが確定ではない。他 OS 対応を暗黙に排除しない |
