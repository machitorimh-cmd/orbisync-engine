# OrbiSync 運用Runbook

# Runbooks

- [Audit event retention](audit-retention.md) - operator-only export, verification, and batched purge for `audit_events`.

本ディレクトリはv1 deploymentを安全に復旧・変更するための実行手順である。設計判断は[ADR-009](../adr/ADR-009-deployment-topology.md)、永続化は[ADR-005](../adr/ADR-005-persistence.md)、認証secretは[ADR-002](../adr/ADR-002-authentication-session.md)を正本とする。

| Runbook | 使用場面 | 破壊的操作前の必須条件 |
|---|---|---|
| [Backup / Restore](backup-restore.md) | 定期backup、復旧訓練、データ復元 | 復元先、復旧時点、現行backupの保全 |
| [Incident Response](incident-response.md) | security/availability incident | incident commanderと証跡保全 |
| [Secret Rotation](secret-rotation.md) | JWT、DB、Webhook secret更新 | overlap期間とrollback secret |
| [Migration Recovery](migration-recovery.md) | schema migration失敗 | migration状態とbackup確認 |
| [Graceful Shutdown](graceful-shutdown.md) | deploy、保守、緊急停止 | drain期限と接続影響の周知 |
| [Deployment](deployment.md) | Docker Composeでのローカル起動 | ComposeはTLS・backup・監視を提供しない前提の確認 |

各手順はコマンド名を契約として示すが、サーバー実装前のため実在を保証しない。実装時はCLI名を維持するか、このRunbookと同時に変更する。
