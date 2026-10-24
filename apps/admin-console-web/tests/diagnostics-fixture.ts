import type { DiagnosticsSnapshot } from "../src/diagnostics.js";
export const diagnosticNow = Date.parse("2026-10-24T10:00:00Z");
const now = diagnosticNow;
export const diagnosticFixture = (): DiagnosticsSnapshot => ({
  collected_at: new Date(now).toISOString(), active_connections: 2,
  checkpoint: { latest_at: null, save_failures_total: 0, restore_failures_total: 0, save_rejected_too_large_total: 0, restore_rejected_too_large_total: 0 },
  queues: { control: { last_observed_depth: 0, capacity_per_instance: 64 }, transform: { last_observed_depth: 0, capacity_per_instance: 256 }, entity: { last_observed_depth: 0, capacity_per_instance: 256 }, outbound: { last_observed_depth: 0, capacity_per_connection: 256, depth_high_water: 0, last_observed_bytes: 0, bytes_high_water: 0 } },
  presence: { observed_at: new Date(now).toISOString(), connected: 2, grace_waiting: 7, reclaim_waiting: 0, memberships: 9, memberships_without_binding: 0, bindings_without_membership: 0, joined_connections: 2, connection_ownership_mismatch: 0 },
  persistence: { pending: 0, oldest_pending_seconds: 0, entity_events: 0, command_outcomes: 0, generation_snapshots: 0 },
  resume_rejections_total: { token_mismatch: 0, history_unavailable: 0, revision_gap: 0, future_revision: 0, not_authorized: 0, ownership_lost: 0 },
  extensions: { outbox_pending: 0, deliveries_in_flight: 0, dead_letters: 0 },
  rate_limit_rejections_total: { connection: 0, user: 0, ip: 0, password_hash: 0, instance: 0 },
  retention: { realtime_tickets_last_success_at: null, idempotency_records_last_success_at: null, audit_source_ips_last_success_at: null, extension_outbox_last_success_at: null },
});
