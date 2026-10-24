-- Exact compound audit searches keep the same bounded, descending keyset.
CREATE INDEX audit_events_request_page_idx ON audit_events (request_id, occurred_at DESC, id DESC)
    WHERE request_id IS NOT NULL;
CREATE INDEX audit_events_target_page_idx ON audit_events (target_type, target_id, occurred_at DESC, id DESC)
    WHERE target_type IS NOT NULL;
CREATE INDEX audit_events_target_id_page_idx ON audit_events (target_id, occurred_at DESC, id DESC)
    WHERE target_id IS NOT NULL;
