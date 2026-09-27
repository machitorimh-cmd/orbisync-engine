# ADR-012: Service extraction criteria

- Status: Proposed
- Date: 2026-07-31

## Context

将来分離可能性を、予定されたmicroservice化と誤解せず、測定可能なtriggerへ変換する必要がある。

## Recommendation

単一processでSLOを満たせず、module内最適化で解消できない場合だけ分離を検討する。CPU、memory、connection数、mailbox待ち、DB latency、failure blast radius、運用工数を30日以上計測し、remote contract、idempotency、observability、rollbackをADRへ記録する。

## Alternatives

初期から分離すると障害modeと運用負担が増える。閾値だけの自動分離は業務影響と費用を評価できない。

## Decision trigger

負荷試験またはproduction telemetryが単一processのSLO未達を示した時点。
