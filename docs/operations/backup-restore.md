# Backup / Restore Runbook

## 目的と目標

- PostgreSQLのlogical backupと、暗号化された保管物からの復元を再現可能にする。
- 仕様 §37.4 の定期backup、世代管理、restore testを満たす。
- **初期目標:** RPO 24時間、RTO 4時間。これは運用者がより厳しい値へ上書きできる初期値である。

## Backup

1. `/health/ready`とDB容量、直近backup結果を確認する。
2. deployment ID、DB version、schema migration version、server revisionを記録する。
3. least-privilege backup roleで`pg_dump --format=custom --no-owner --no-acl`を実行する。
4. SHA-256 checksumを生成し、backupとmanifestを別の障害domainへ暗号化保存する。
5. 保存物を再読込してchecksumを検証する。資格情報をlogへ出さない。
6. 日次backupを30日保持する（ADR-009）。より長い週次・月次archiveは組織の保持・privacy要件を確認して追加する。

## Restore drill

1. 本番DBへ直接restoreしない。隔離された新規databaseを作る。
2. 対象backup、checksum、暗号鍵、PostgreSQL互換versionを確認する。
3. `pg_restore --clean --if-exists --no-owner`で隔離DBへ復元する。
4. migration version、主要table件数、外部キー、直近audit eventを検証する。
5. OrbiSyncをreadiness非公開で接続し、login、world list、instance readのsmoke testを行う。
6. 復元所要時間とデータ最終時刻を記録し、RPO/RTOを評価する。少なくとも月次で実施する（ADR-009）。

### Automated drill

`scripts/restore-drill.sh` が上記 Backup / Restore drill と**同じコマンド**で自動化している（受け入れ条件の「手順書と同じコマンド」要件）。

- `pg_dump --format=custom --no-owner --no-acl` でバックアップし SHA-256 を記録
- 別名の隔離データベース（`orbisync_restore`）を作り `pg_restore --clean --if-exists --no-owner` で復元（D-16）
- 復元先の migration version と主要テーブル件数を比較
- 復元したデータベースに対してサーバーを起動し、バックアップ前に `bootstrap-admin` で作った管理者が実際に `POST /v1/auth/login` できることを検証（最重要の条件 4）
- 一時パスワードは `--password-output` でファイルへ書き、標準出力やログへ出さない（D-17）
- 失敗時は `trap` でコンテナを後片付けし、非ゼロで終了する（条件 5）

実行:

```bash
bash scripts/restore-drill.sh
```

### Periodic verification runner and history

`scripts/run_restore_verification.py` wraps the drill, preserves its exit code,
and appends one JSON object per run to an append-only JSONL history. Both
successful and failed runs are recorded with `backup_id`, start/completion
timestamps, elapsed milliseconds, verification mode, and exit code. The
history contains no database URL, password, token, or response body.

The self-contained mode remains useful for release checks:

```bash
python3 scripts/run_restore_verification.py \
  --backup-id release-drill-2026-09-22 \
  --history /var/lib/orbisync/restore-verification.jsonl
```

For the production backup catalog, decrypt/download one custom-format
`pg_dump` artifact to a protected local file, obtain its catalog ID and
checksum, and use a dedicated smoke-test account. The password is read only
from a file and is never placed in command output or the history:

```bash
python3 scripts/run_restore_verification.py \
  --backup-id pg-prod-2026-09-22T000000Z \
  --backup-file /secure/restore-input/backup.dump \
  --backup-sha256 "$BACKUP_SHA256" \
  --login-id restore_smoke \
  --password-file /run/secrets/orbisync-restore-smoke-password \
  --history /var/lib/orbisync/restore-verification.jsonl
```

External mode verifies the checksum when supplied, restores only into the
isolated `orbisync_restore` database, checks migration metadata and required
tables, starts OrbiSync against the restored database, and performs a real
login. Set `RESTORE_DRILL_POSTGRES_IMAGE` to the supported PostgreSQL image
matching the backup source. The source production database is never opened by
the drill.

Run the command at least monthly with cron, a systemd timer, or the deployment
platform scheduler. The scheduler should atomically resolve the latest backup
ID/file/checksum before invoking the runner and alert on a non-zero exit. It
must store the JSONL history on persistent operator-managed storage rather
than inside an ephemeral checkout or container. Example cron wrapper entry:

```cron
0 3 1 * * /usr/local/sbin/orbisync-verify-latest-backup
```

The wrapper at that path should retrieve the current catalog metadata and call
the command above. GitHub Actions remains manual-only while CI minutes are
constrained; periodic disaster-recovery verification belongs on the operator
scheduler and does not require hosted CI.

手動で `ci.yml` を起動した場合は `restore-drill` ジョブが `pr-ci` の判定に含まれる。現在のworkflowは無料枠制約のためmanual-onlyであり、自動scheduleではない。

## Production recovery

incident commander承認後、新規DBへ復元して接続先を切り替える。旧DBを上書きせずread-onlyで保全する。切替失敗時は旧接続先へ戻し、書込み二重化を避ける。復元後はJWT/DB/Webhook secret漏洩の可能性を評価し、必要ならrotationする。
