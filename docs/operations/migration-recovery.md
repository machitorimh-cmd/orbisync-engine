# Migration Recovery Runbook

## Deploy前

1. migration ID、checksum、forward/backward compatibility、lock見積りをreviewする。
2. production相当データ量のcopyで適用時間とrollback/recoveryを試験する。
3. backupのchecksumとrestore drill成功時刻を確認する。
4. expand → migrate/backfill → contractを別releaseへ分離する。

## 失敗時

1. 新規server rolloutと追加migrationを停止する。
2. `schema_migrations`、PostgreSQL activity/locks、server revision、失敗SQLSTATEを記録する。
3. transaction内で失敗したmigrationはrollback完了を確認する。transaction外DDLは実状態をinspectし、再実行可能性を判断する。
4. destructive down migrationを即時実行しない。旧serverが新schemaを読めるならapplication rollbackを優先する。
5. 修復migrationを新しい不変IDで作る。既に共有されたmigration fileを編集しない。
6. データ破損時は[Backup / Restore](backup-restore.md)に切り替え、旧DBを保全する。

## 完了条件

全processが同じschema versionを報告し、readiness、auth、world read/write、audit write、outbox deliveryのsmoke testが成功する。原因と再発防止をincident記録へ残す。
