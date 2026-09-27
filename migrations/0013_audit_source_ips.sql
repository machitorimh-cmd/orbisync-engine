-- Keep privacy-sensitive source IPs separate from the append-only audit record.
CREATE TABLE audit_source_ips (
    audit_event_id UUID PRIMARY KEY REFERENCES audit_events(id) ON DELETE CASCADE,
    source_ip INET NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

-- Preserve source IPs already recorded before the split. The audit timestamp
-- is the only available creation timestamp for those rows.
INSERT INTO audit_source_ips (audit_event_id, source_ip, created_at)
SELECT id, source_ip, occurred_at
FROM audit_events
WHERE source_ip IS NOT NULL;

CREATE INDEX audit_source_ips_created_at_idx
    ON audit_source_ips (created_at, audit_event_id);

ALTER TABLE audit_events DROP COLUMN source_ip;

REVOKE ALL ON audit_source_ips FROM PUBLIC;
-- SELECT/INSERT are required by the existing audit read/write paths;
-- DELETE is the only mutation privilege used by source-IP retention.
GRANT SELECT, INSERT, DELETE ON audit_source_ips TO orbisync_runtime;
