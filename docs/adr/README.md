# Architecture Decision Records

| ADR | Status | Decision |
|---|---|---|
| [ADR-001](ADR-001-naming-identifiers.md) | Accepted | Naming and identifiers |
| [ADR-002](ADR-002-authentication-session.md) | Accepted | Authentication session |
| [ADR-003](ADR-003-rest-api.md) | Accepted | REST API contract |
| [ADR-004](ADR-004-realtime-protocol.md) | Accepted | Realtime protocol |
| [ADR-005](ADR-005-persistence.md) | Accepted | Persistence |
| [ADR-006](ADR-006-instance-concurrency.md) | Accepted | Instance concurrency |
| [ADR-006A](ADR-006A-sequence-idempotency.md) | Accepted | Sequence and idempotency |
| [ADR-007](ADR-007-extension-api.md) | Accepted | Extension API |
| [ADR-008](ADR-008-observability-audit.md) | Accepted | Observability and audit |
| [ADR-009](ADR-009-deployment-topology.md) | Accepted | Deployment topology |
| [ADR-010](ADR-010-dependency-policy.md) | Proposed | Dependency policy |
| [ADR-011](ADR-011-sdk-release.md) | Proposed | SDK release |
| [ADR-012](ADR-012-service-extraction.md) | Proposed | Service extraction criteria |
| [ADR-013](ADR-013-generated-code-policy.md) | Accepted | Generated code policy and acceptance gates |
| [ADR-014](ADR-014-request-id.md) | Accepted | Request ID generation and propagation |
| [ADR-015](ADR-015-local-credential-security.md) | Accepted | Local credential security and bootstrap |
| [ADR-016](ADR-016-identity-administration-contract.md) | Accepted | Identity administration public contract |
| [ADR-017](ADR-017-identity-persistence-and-migrations.md) | Accepted | Identity persistence, audit transactions and migration recovery |
| [ADR-018](ADR-018-configuration-and-runtime-tuning.md) | Accepted | Configuration discovery and runtime tuning |
| [ADR-019](ADR-019-audit-source-ip-retention.md) | Accepted | Audit source-IP separation and retention |
| [ADR-020](ADR-020-namespace-separated-bootstrap-roles.md) | Accepted | Namespace-separated bootstrap roles |
| [ADR-021](ADR-021-property-and-fuzz-testing.md) | Accepted | Property and fuzz testing frameworks |
| [ADR-022](ADR-022-chaos-testing.md) | Accepted | Chaos test injection and P2 scenario ownership |
| [ADR-023](ADR-023-animation-and-presence-payloads.md) | Accepted | Animation and presence payloads |
| [ADR-024](ADR-024-speed-acceleration-check-toggle.md) | Accepted | Speed/acceleration check toggle |
| [ADR-025](ADR-025-pre-commit-extension-validation-hook.md) | Accepted | Pre-commit extension validation hook |
| [ADR-026](ADR-026-authentication-method-selection.md) | Accepted | Authentication method selection (account / guest / name-only / external) |
| [ADR-027](ADR-027-entity-ownership-transfer.md) | Accepted | Public entity ownership transfer |
| [SDK-02](SDK-02-send-queue.md) | Accepted | TypeScript SDK send-queue capacity, saturation behaviour and batch interval |
| [RL-05](RL-05-license.md) | Accepted | Dual-license decision |

ADRは判断の正本である。契約ファイルと矛盾する場合は、実装前にADRまたは契約を同じ変更で修正する。Accepted ADRを黙って上書きしない。
