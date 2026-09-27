CREATE TABLE idempotency_records (
    key UUID PRIMARY KEY,
    actor_user_id UUID NULL REFERENCES users(id),
    operation TEXT NOT NULL,
    request_hash BYTEA NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('in_progress', 'completed')),
    status_code SMALLINT NULL CHECK (status_code BETWEEN 100 AND 599),
    response_content_type TEXT NULL,
    response_body BYTEA NULL,
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK (expires_at = created_at + INTERVAL '24 hours'),
    CHECK (
        (state = 'in_progress' AND status_code IS NULL AND response_content_type IS NULL AND response_body IS NULL)
        OR
        (state = 'completed' AND status_code IS NOT NULL AND response_content_type IS NOT NULL AND response_body IS NOT NULL)
    )
);

CREATE INDEX idempotency_records_expires_at_idx ON idempotency_records (expires_at);
