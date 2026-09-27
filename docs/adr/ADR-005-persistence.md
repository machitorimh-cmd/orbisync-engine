# ADR-005: PostgreSQL永続化境界

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

DB driver、migration、transaction、table ownership、outbox、対応PostgreSQL versionをschema作成前に決める必要がある。

## Decision

- PostgreSQL 16および17を初期support対象とし、CIは両versionでintegration testを実行する。
- Rust DB accessはSQLxを使用し、runtime queryはparameter bindingする。
- migrationは`sqlx migrate`、前方移行、Expand → Migrate → Contractを採用する。
- `cargo sqlx prepare --check`用metadataをcommitし、CIでoffline query checkを行う。
- moduleごとのtable ownershipを維持し、他moduleのrepository/tableを直接操作しない。
- v1ではcross-module Unit of Workを導入しない。module-local transactionだけを許可する。
- 外部配送はtransactionへ含めず、所有moduleのlocal transactionでdurable outboxへ記録する。
- migration用DB roleとruntime用DB roleを分離し、runtime roleへDDL権限を与えない。
- UUIDv7はnative`UUID`、timestampは`TIMESTAMPTZ`、revisionは`BIGINT`を使用する。

## Alternatives

- ORM: domain境界よりmapping都合が支配的になるため不採用。
- 一般化したcross-module transaction: table ownershipを破りやすいため不採用。
- dual-write: failure時の不整合を生むため不採用。

## Consequences

- PostgreSQL固有機能へ依存する。
- outbox relayとcleanupが必要になる。
- cross-module atomicityが必要になった場合は具体use caseを新ADRで列挙する。

## Migration

初期schema、role、migration test、backup/restore testを作成する。
