## 概要

<!-- 何を、なぜ変更したかを簡潔に説明してください。 -->

## 関連Issue / RFC

<!-- `Closes #123`、Issue/RFCのURL、または該当なしの理由を記載してください。 -->

## 変更範囲

<!-- 影響するcrate、module、API、protocol、DB、SDK、運用構成、文書を列挙してください。 -->

## 変更種別

- [ ] Bug fix
- [ ] Feature
- [ ] Refactoring
- [ ] Protocol / API change
- [ ] Database migration
- [ ] Security
- [ ] Documentation
- [ ] Build / CI / Operations

## 検証結果

<!-- 実行したcommand、test、benchmarkと結果を記載してください。未実施の場合は理由を記載してください。 -->

```text
command:
result:
```

## レビュー時の注意点

<!-- 特に確認してほしい判断、既知の制約、残存リスク、後続作業を記載してください。 -->

## チェックリスト

該当しない項目はチェックしたうえで、理由をコメントまたは本文へ記載してください。

- [ ] IssueまたはRFCへリンクした
- [ ] 変更目的と変更範囲を説明した
- [ ] Unit testを追加・更新した
- [ ] Integration testを追加・更新した、または不要な理由を記載した
- [ ] Protocol / APIの後方互換性を確認した
- [ ] Protocol変更時にfield number、reserved、test vector、生成物を更新した
- [ ] DB migrationの前方適用と後方互換性を確認した
- [ ] 認証、認可、入力検証、情報漏洩への影響を確認した
- [ ] Security変更時に `docs/security/threat-model.md` の脅威ID、対策、残存リスクを更新した
- [ ] password、token、Authorization header、完全なpayloadをログへ追加していない
- [ ] メトリクス、ログ、トレース、監査への影響を確認した
- [ ] rate limit、size limit、queue、latency、memoryへの影響を確認した
- [ ] 負荷影響がある場合はbenchmarkまたはload test結果を添付した
- [ ] Generated codeを正規手順で再生成し、直接編集していない
- [ ] 利用者・運用者・開発者向けドキュメントを更新した
- [ ] CHANGELOGとmigration notesの対象か確認した
- [ ] 新規dependency、license、脆弱性、supply-chainへの影響を確認した
- [ ] 未解決のADR事項や残存リスクを明記した
