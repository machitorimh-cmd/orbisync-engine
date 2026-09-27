# Migrations

PostgreSQL 16 と 17 を対象とした前方移行専用の SQL migration を配置します（ADR-005）。

## 方針

- migration tool は `sqlx migrate` を使用します（ADR-005）。
- 前方移行を基本とし、破壊的変更は Expand → Migrate → Contract の複数リリースへ分割します（仕様 §22.6）。
- 適用済み migration ファイルは編集しません。sqlx が checksum を検証し、変更を検出すると起動に失敗します。
- migration 用 DB role と runtime 用 DB role を分離し、runtime role へ DDL 権限を与えません（ADR-005）。
- ID column は `UUID`、時刻は `TIMESTAMPTZ`、revision は `BIGINT` を使用します（ADR-005）。

## 現在の状態

Milestone 0 は migration 基盤（runner、CLI subcommand、CI job、PostgreSQL 16/17 matrix）だけを提供します。スキーマ本体は Milestone 1 の成果物であり、このディレクトリにはまだ SQL ファイルがありません。

## ファイル命名

```text
migrations/
├─ 0001_initial.sql
├─ 0002_roles.sql
└─ README.md
```

ADR-017により`NNNN_<snake_case>.sql`の4桁連番へ統一します。`sqlx migrate add <name>`が生成するtimestamp名はそのまま使用せず、未適用のうちに次の連番へrenameします。適用済みmigrationはrenameも編集もしません。

## Rollback / recovery

down migration fileは作成せず、前方移行専用とします。Contract前のrollback検証は、直前backupから隔離DBへrestoreできること、Expand schemaで旧binaryが起動できること、失敗をforward corrective migrationで修復できることのrehearsalを指します（ADR-017）。

## 適用方法

ローカル開発ではComposeのPostgreSQL管理userがmigrationを実行し、`orbisync_runtime` NOLOGIN roleも初回migrationで作成します。本番でmigration roleへ`CREATEROLE`を付与しない場合は、ADR-017に従ってDBAがruntime roleを事前provisioningし、schema migrationとrole lifecycleを分離してください。

```shell
# サーバーバイナリ経由（推奨。埋め込み migrator を使用）
DATABASE_URL=postgres://orbisync:orbisync@localhost:5432/orbisync \
  cargo run -p orbisync-server -- migrate

# sqlx-cli 経由
cargo install sqlx-cli --no-default-features --features rustls,postgres
DATABASE_URL=postgres://orbisync:orbisync@localhost:5432/orbisync sqlx migrate run
```

## 検証

`tests/integration/tests/migrations.rs` が空の DB へ全 migration を適用します。`DATABASE_URL` が設定されていない環境では skip されます。CI は PostgreSQL 16 と 17 の両方で実行します。


## LOW-002 audit retention

Migration `0014_audit_retention_operator.sql` is the sole migration for
operator-only `audit_events` retention (no conflicting `0014_*` migration is
present). It creates the `NOLOGIN`/non-privileged
`orbisync_audit_maintenance` role, an archive manifest table, and
`SECURITY DEFINER` functions in the `audit_operations` schema. `orbisync_runtime`
continues to have only `SELECT, INSERT` on `audit_events`; the maintenance role
has no table privileges and receives only function `EXECUTE`. A DBA must grant
that role to a separate operator login.

`validate_retention_days` accepts `1..3650`; `purge_audit_events` accepts a
`1..10000` batch and performs one transaction-scoped advisory-locked
`ORDER BY ... LIMIT ... FOR UPDATE SKIP LOCKED` batch. Function failures roll
back the batch and marker update. Apply the forward migration with the normal
`sqlx migrate` runner; recovery is backup/restore or a reviewed corrective
migration, not editing an applied migration.

## Persistent entities

Migration `0016_persistent_entities.sql` adds `persistent_entities` and
`persistent_entity_components` (state-and-runtime.md §1.2). Entity definitions
are saved on spawn/delete and components on update; velocity, animation, and
presence are ephemeral and have no column. The DM-04 limits (16 components per
entity, 4096 bytes per payload, namespaced non-`core` keys) are enforced by
CHECK constraints plus the `persistent_entity_components_limit_check` trigger.
The checkpoint table is unchanged and continues to coexist with these rows.

Migration `0017_persistent_entities_instance_idx.sql` replaces
`persistent_entities_instance_revision_idx` (`instance_id, revision DESC`)
with `persistent_entities_instance_idx` (`instance_id`). Instance restore
reads every row for an instance, so the `revision DESC` ordering never
helped that query; it was copied from `instance_checkpoints`, which only
needs the latest row.

## Staged canonical generation codec

`0020_checkpoint_canonical_codec.sql` adds codec 5 while preserving existing
codec-4 rows and digests. It does not change existing writer-control protocol
values or convert a generation head. The new startup overload declares protocol
5 explicitly; its execution grant and the operator-reviewed protocol transition
remain deployment work. Codec-4 heads block codec-5 advancement in both the
adapter and schema. Do not enable generation mode on the basis of local codec
tests: supported PostgreSQL version/transition tests and the existing rollout
gates still apply.
