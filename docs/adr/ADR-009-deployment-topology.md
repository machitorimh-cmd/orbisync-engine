# ADR-009: TLS・trusted proxy・初期deployment topology

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

仕様の「1インスタンス=1組織」はprocess/database topologyを確定しない。公開前に初期運用単位、TLS終端、proxy header、health endpointを決める必要がある。

## Decision

- v1の標準運用は1 deployment = 1 organizationとする。
- 1 deploymentは1`orbisync-server` processと1 PostgreSQL logical database/schemaを持つ。
- 複数World Instanceは同じserver process内で実行する。
- production TLSは外部reverse proxyで終端し、Coreへのhopも同一host/private networkに限定する。
- trusted proxy CIDRを明示設定し、未設定時はforwarded headerを信用しない。
- public URL、client IP、schemeの解決はtrusted proxy検証後だけ行う。
- `X-Forwarded-For` は、transport peer が trusted proxy の場合だけ右端
  （peer に最も近い側）から trusted hop をたどり、最初の untrusted address
  を client IP として採用する。peer がない、untrusted、header が不正、または
  chain 全体が trusted の場合は peer へ fail closed する。
- `/health/live`と`/health/ready`は認証なしだがlocalhost/internal networkだけへ公開する。
- `/metrics`もlocalhost/internal network限定とする。
- Docker Composeを最小検証・小規模運用構成としてsupportする。Kubernetesはv1 support matrixへ含めない。
- backupは毎日、30日保持、restore testは月次、graceful drainは30秒とする。

## Alternatives

- process/database共有のmulti-tenant: isolationと運用責任が複雑になる。
- process内TLS: 証明書rotationとproxy機能がCore責務へ混入する。
- forwarded headerを常時信用: spoofingを許す。

## Consequences

- 組織ごとにdeployment運用費が発生する。
- 将来multi-tenant化する場合、OrganizationId routing、schema/table isolation、migration戦略の新ADRが必要になる。
- reverse proxy製品は固定せず、契約と設定例だけをsupportする。

## Migration

Compose例、proxy設定例、health exposure test、backup/restore runbookを作成する。
