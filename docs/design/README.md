# OrbiSync 設計索引

初めて読む場合は、システム境界から契約、運用の順に進む。

1. [System Context](system-context.md)
2. [Architecture](architecture.md)
3. [Domain Model](domain-model.md)
4. [Transport Boundaries](transport-boundaries.md)
5. [REST API / Persistence](rest-api-persistence.md)
6. [Realtime Protocol / Connection](realtime-protocol-and-connection.md)
7. [State / Runtime](state-and-runtime.md)
8. [Mobile Resume / Interest / Backpressure](mobile-resume-interest-backpressure.md)
9. [Authentication / Authorization](auth-authorization.md)
10. [Extension Mechanism](extension-mechanism.md)
11. [Observability / Configuration](observability-and-config.md)
12. [Client SDK](client-sdk.md)
13. [Scale / NFR](scale-and-nfr.md)
14. [Repository / Crate Conventions](repo-crate-conventions.md)
15. [Test / CI](test-and-ci.md)
16. [Release / Maintenance / License](release-maintenance-license.md)
17. [Deployment / Threat Model](deployment-and-threat-model.md)
18. [Roadmap / Traceability](roadmap-and-traceability.md)
19. [Technology Decisions](technology-decisions.md)
20. [Mermaid Diagrams](diagrams.md)
21. 汎用アプリ基盤化に向けた改善提案 (internal record omitted from this source distribution)

21番目の文書は、他の文書と異なり確定した設計ではなく、今後の改善提案（計画のみ）をまとめたものである。

クライアントを実装する場合は、実装から起こした[フロントエンド接続ガイド](../guides/frontend-integration.md)を併読する。本索引の各文書は契約の意図を定め、同ガイドは現在の実装の呼び出し方を記述する。

公開契約:

- [OpenAPI v1](../../openapi/orbisync-v1.yaml)
- [REST error registry](../../openapi/errors.yaml)
- [Realtime Proto v1](../../proto/orbisync/v1/realtime.proto)
- [Realtime state machine](../../contracts/realtime-connection-state-machine.json)
- [Realtime resume token policy](../../contracts/realtime-resume-token-policy.json)
- [Test vectors](../../test-vectors/)

判断記録は[ADR索引](../adr/README.md)、運用手順は[Operations Runbook](../operations/README.md)を参照する。原仕様は[metaverse_core_specification.md](../../metaverse_core_specification.md)であり変更禁止とする。

レビュー・監査の記録はレビュー索引 (internal record omitted from this source distribution)にまとめている。設計レビュー、コードレビュー、独立レビュー、監査の全27件を種別ごとに日付順で並べており、現在の状態を知るための出発点も同索引の冒頭に示す。個別の文書を本索引から直接指すことはしない。追加や更新のたびに参照が古くなるためである。
