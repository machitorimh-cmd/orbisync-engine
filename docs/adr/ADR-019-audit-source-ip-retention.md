# ADR-019: Audit source-IP separation and retention

- Status: Accepted
- Date: 2026-08-29
- Decision Owners: avistoria

## Context

`audit_events` is append-only under ADR-008, while source IPs are privacy-
sensitive data with an independently configurable retention period. Keeping
the source IP in the append-only row makes it impossible to honor that
retention period without weakening the audit immutability contract.

## Decision

1. Store source IPs in `audit_source_ips`, keyed by `audit_event_id`, with a
   `created_at` timestamp. The audit event itself no longer contains a
   `source_ip` column.
2. Migration 0013 copies existing non-null source IPs to the new table before
   dropping the old column. The copy uses the event timestamp so existing
   retention age is preserved.
3. Audit writes insert the event and its optional source IP in the same
   PostgreSQL transaction. Audit reads use a left join, so a retained audit
   event remains readable after its source IP is removed.
4. The runtime role may `SELECT`, `INSERT`, and `DELETE` source-IP rows, but
   it may not `UPDATE` or `DELETE` rows in `audit_events`. Source-IP deletion
   is the only destructive audit-related operation exposed to the runtime
   role.
5. The existing retention worker performs bounded source-IP cleanup using
   `retention.source_ip_days`, which defaults to 365 days. No additional
   worker is introduced.

## Consequences

- Source IPs can be deleted without altering the append-only audit record.
- Historical audit events remain available with a null source IP after
  source-IP retention expires.
- The migration adds a table and performs a data-preserving copy; operators
  should verify the migration on a backup or staging database before rollout.
- Source-IP queries require the join and its index, while cleanup can use the
  `created_at` index and bounded `SKIP LOCKED` batches.

## Alternatives rejected

- Deleting or updating `audit_events` would violate ADR-008.
- A second cleanup worker would duplicate scheduling and supervision already
  provided by the retention worker.
- Retaining source IPs forever would not satisfy the privacy retention policy.
