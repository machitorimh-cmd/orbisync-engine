DO $roles$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'orbisync_runtime') THEN
        CREATE ROLE orbisync_runtime NOLOGIN;
    END IF;
END
$roles$;

CREATE TABLE audit_events (
    id UUID PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL,
    actor_user_id UUID NULL,
    action TEXT NOT NULL,
    target_type TEXT NULL,
    target_id TEXT NULL,
    request_id TEXT NULL,
    source_ip INET NULL,
    result TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX audit_events_occurred_at_idx ON audit_events (occurred_at);
CREATE INDEX audit_events_actor_user_id_idx ON audit_events (actor_user_id);
CREATE INDEX audit_events_action_idx ON audit_events (action);

CREATE TABLE outbox_events (
    id UUID PRIMARY KEY,
    owner_module TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    available_at TIMESTAMPTZ NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    delivered_at TIMESTAMPTZ NULL,
    last_error_code TEXT NULL
);

CREATE INDEX outbox_events_pending_idx
    ON outbox_events (available_at)
    WHERE delivered_at IS NULL;

REVOKE ALL ON audit_events FROM PUBLIC;
GRANT SELECT, INSERT ON audit_events TO orbisync_runtime;
