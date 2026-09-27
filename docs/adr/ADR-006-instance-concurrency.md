# ADR-006: World Instance concurrency・tick・shutdown

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

正準状態の所有者、command ordering、mailbox、tick、task配置、shutdown/recoveryをruntime実装前に固定する必要がある。

## Decision

- 1 World Instanceにつき1 actorを置き、単一command入口で正準状態更新を直列化する。
- 全Instanceは初期版で同一Tokio multi-thread runtime上に置く。socket taskは正準状態を所有しない。
- actor mailboxは3本のbounded channel（control 64 / transform 256 / entity 256）を初期値とし、`world.mailbox_*_capacity`で設定可能にする。これは負荷試験（E-1/E-5/E-6）未実施時点の暫定値であり、再評価時も設定互換性を維持する。
- transform inputは接続・entity単位latest-winsでactor投入前に集約する。reliable commandはdropしない。
- active Instanceは20Hz固定tick、idle maintenanceは1Hzとする。値は設定可能だが起動時に範囲検証する。
- 1 tick内は管理command → reliable command → latest-wins input → timeout/checkpointの順に処理する。
- overload時は新規latest-winsを置換し、reliable容量超過は当該接続を警告後切断する。Instance actorを無制限に待たせない。
- SIGTERM時は新規接続を停止し、最大30秒drain後に最終checkpointを試みて終了する。
- actor panicまたは正準状態破損疑いではInstanceをFailedへ遷移し、無条件継続しない。

## Alternatives

- lock共有状態: 競合とorderingが観測しにくいため不採用。
- InstanceごとのOS thread/runtime: 初期規模ではresource overheadが大きい。
- unbounded channel: memory exhaustionを防げない。

## Consequences

- 長時間commandをactor内で実行できない。外部I/Oはport経由で分離する。
- mailbox/tick値はload testで再評価するが、変更はconfiguration compatibilityの対象になる。

## Migration

deterministic clockとfake mailboxでordering、overload、panic、shutdown testを作成する。
