-- Labels are advisory and never participate in authentication or authorization.
ALTER TABLE auth_sessions ADD COLUMN device_label text
    CHECK (device_label IS NULL OR (char_length(device_label) BETWEEN 1 AND 128));

-- Stable, owner-scoped keyset pagination. The leading owner also serves bulk revoke.
CREATE INDEX auth_sessions_user_id_id_idx ON auth_sessions (user_id, id);
