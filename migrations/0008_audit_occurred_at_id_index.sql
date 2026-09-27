-- CR-06: audit ordering must be (occurred_at DESC, id DESC) with keyset pagination
-- using WHERE (occurred_at, id) < ($1, $2). Existing indexes are single-column
-- on occurred_at / actor / action, so a composite index is required for the
-- new sort and to avoid full scan as row count grows.
CREATE INDEX audit_events_occurred_at_id_idx ON audit_events (occurred_at DESC, id DESC);
