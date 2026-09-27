CREATE TABLE realtime_tickets (
    token_digest BYTEA PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES auth_sessions(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    issued_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX realtime_tickets_session_idx ON realtime_tickets (session_id);
CREATE INDEX realtime_tickets_expires_at_idx ON realtime_tickets (expires_at);
