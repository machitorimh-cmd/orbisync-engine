-- WH2: one retry state per event and extension destination.

CREATE TABLE extension_deliveries (
    delivery_id UUID PRIMARY KEY,
    event_id UUID NOT NULL REFERENCES outbox_events (event_id),
    extension_id UUID NOT NULL REFERENCES extension_registrations (extension_id),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at TIMESTAMPTZ NOT NULL,
    delivered_at TIMESTAMPTZ NULL,
    dead_lettered_at TIMESTAMPTZ NULL,
    last_error_code TEXT NULL,
    UNIQUE (event_id, extension_id)
);

CREATE INDEX extension_deliveries_due_idx
    ON extension_deliveries (available_at)
    WHERE delivered_at IS NULL AND dead_lettered_at IS NULL;

CREATE TABLE extension_dead_letters (
    delivery_id UUID PRIMARY KEY,
    event_id UUID NOT NULL,
    extension_id UUID NOT NULL,
    event_kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    attempt_count INTEGER NOT NULL CHECK (attempt_count > 0),
    error_code TEXT NOT NULL,
    dead_lettered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    retained_until TIMESTAMPTZ NOT NULL
);

CREATE INDEX extension_dead_letters_retention_idx
    ON extension_dead_letters (retained_until);
