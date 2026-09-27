# Security Policy

OrbiSyncでは、利用者と開発者の安全を損なわずに脆弱性を修正するため、責任ある非公開報告をお願いしています。

## 現在のサポート状況

OrbiSyncは開発中で、serverとSDKの実装を含みます。サポート対象の公開releaseやsecurity update期間はまだ定めていません。

| Version | Supported |
|---|---|
| 公開releaseなし | — |

実装またはreleaseが公開された後、この表へサポート対象versionとsecurity update期間を追加します。

## 脆弱性の報告

脆弱性または機密性のあるセキュリティ問題は、**公開Issue、公開Discussion、公開Pull Requestへ投稿しないでください**。

GitHubリポジトリの次の経路から非公開で報告してください。

1. リポジトリの **Security** タブを開く
2. **Advisories** を開く
3. **Report a vulnerability** または **New draft security advisory** を選ぶ
4. 下記の情報を可能な範囲で記載する

GitHubのPrivate Vulnerability Reportingがまだ有効でない場合は、問題の詳細を公開せず、機密性のある報告経路が必要であることだけを通常の連絡手段で管理者へ知らせてください。公開前にPrivate Vulnerability Reportingを有効化します。

## 報告に含めてほしい情報

- 影響するcomponent、endpoint、protocol message、versionまたはcommit
- 脆弱性の種類と想定される影響
- 再現に必要な最小手順またはproof of concept
- 必要な権限、設定、network条件
- 実際の結果と期待する安全な結果
- 回避策または修正案（分かる場合）
- 公開予定、第三者への共有状況
- 希望するcredit表記

password、access token、refresh token、private key、実利用者の個人情報、本番データは送らないでください。再現には無効なsample credentialと最小化したデータを使用してください。

## 対応方針

報告を受けた場合、maintainerは次の流れで対応します。

1. 受領と機密性を確認する
2. 再現性、影響範囲、severityを評価する
3. 修正、回避策、testを非公開で準備する
4. 必要に応じてtoken、secret、artifactを失効・更新する
5. 修正版とsecurity advisoryを調整して公開する
6. creditと開示時期を報告者と調整する
7. [`docs/security/threat-model.md`](docs/security/threat-model.md) と回帰testを更新する

設計上の目標は、Criticalを7日以内、Highを30日以内に修正することです。ただし、これは現段階の目標であり、応答・修正を法的または契約的に保証するSLAではありません。

## 対象となる問題の例

- 認証回避、token偽造・再利用・失効不備
- 権限昇格、他利用者Entityの不正操作
- Snapshotまたはログからの情報漏洩
- WebSocket frame、rate limit、queueを利用したDoS
- NaN/Infinity等による正準状態破損
- Webhook SSRF
- SQL injection、migration、backup、secret管理の問題
- 監査ログの改ざんまたは欠落
- dependency、CI、release artifactのサプライチェーン侵害

一般的な機能要望、設計上の意見、機密性のないbugは通常のIssueを使用してください。

## Safe Harbor

善意で、必要最小限の範囲にとどめ、利用者データを侵害せず、可用性を損なわず、取得した情報を悪用せずに行われる調査を歓迎します。

この記述は適用法を変更するものでも、法的助言を提供するものでもありません。調査が第三者のsystemやdataへ及ぶ場合は、その所有者から別途許可を得てください。

## Security設計

信頼境界、脅威台帳、対策、検証方法、残存リスクは [`docs/security/threat-model.md`](docs/security/threat-model.md) で管理しています。
