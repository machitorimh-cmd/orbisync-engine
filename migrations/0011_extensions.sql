-- WH1: extension registrations and durable webhook outbox metadata.

CREATE TABLE extension_registrations (
    extension_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NULL,
    endpoint TEXT NOT NULL,
    subscribed_events JSONB NOT NULL DEFAULT '[]'::jsonb,
    capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    token_scopes JSONB NOT NULL DEFAULT '[]'::jsonb,
    status TEXT NOT NULL CHECK (status IN ('active', 'suspended')),
    signing_secret_ref TEXT NOT NULL CHECK (length(signing_secret_ref) > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX extension_registrations_status_idx
    ON extension_registrations (status);

-- Keep the original generic outbox columns for compatibility with earlier
-- modules, while exposing the webhook contract's explicit names as well.
ALTER TABLE outbox_events ADD COLUMN event_id UUID;
ALTER TABLE outbox_events ADD COLUMN event_kind TEXT;

UPDATE outbox_events
SET event_id = id,
    event_kind = event_type;

ALTER TABLE outbox_events
    ALTER COLUMN event_id SET NOT NULL,
    ALTER COLUMN event_kind SET NOT NULL;

ALTER TABLE outbox_events
    ADD CONSTRAINT outbox_events_event_id_unique UNIQUE (event_id);

CREATE INDEX outbox_events_event_kind_idx ON outbox_events (event_kind);
