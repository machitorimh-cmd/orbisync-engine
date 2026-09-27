-- ADR-007: one active, digest-only service credential per extension.
CREATE TABLE extension_service_tokens (
    extension_id UUID PRIMARY KEY REFERENCES extension_registrations(extension_id) ON DELETE CASCADE,
    token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    scopes JSONB NOT NULL CHECK (jsonb_typeof(scopes) = 'array'),
    issued_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK (expires_at = issued_at + INTERVAL '720 hours')
);
GRANT SELECT, INSERT, UPDATE, DELETE ON extension_service_tokens TO orbisync_runtime;
GRANT SELECT ON extension_registrations TO orbisync_runtime;
