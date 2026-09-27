-- Authentication method selection (ADR-026).
--
-- Adds the storage for guest, name-only and external subjects. Existing rows
-- become 'account' through the column default, so the local login path keeps
-- reading and writing exactly what it did before.
--
-- None of these subjects gets a `user_credentials` row. `find_login` and
-- `find_account` both INNER JOIN that table, so a subject without one cannot
-- reach the password login path at all. That is what "no permanent login
-- credential" means here -- not the absence of a row in `users`, which has to
-- exist because `auth_sessions.user_id` and `persistent_entities.owner_id`
-- are foreign keys into it.

ALTER TABLE users ADD COLUMN kind TEXT NOT NULL DEFAULT 'account'
    CHECK (kind IN ('account', 'guest', 'name_only', 'external'));

CREATE INDEX users_kind_idx ON users (kind) WHERE kind <> 'account';

-- Ledger for temporary subjects (guest, name-only).
--
-- `expires_at` is the absolute deadline. It is written once when the session
-- is issued and never updated: refresh rotation reads it to clamp the new
-- session and token expiry, so repeated refresh cannot extend a temporary
-- subject's life. Access is refused by comparing the clock against this
-- deadline, independently of whether the revocation job has run.
--
-- `allowed_worlds` is a snapshot of the configured boundary taken at issue
-- time. Removing a world from the configuration therefore does not eject a
-- subject that already holds a session; the effect is bounded by the session
-- lifetime, and an operator who needs an immediate cutoff revokes the session.
CREATE TABLE ephemeral_subjects (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    method TEXT NOT NULL CHECK (method IN ('guest', 'name_only')),
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    allowed_worlds UUID[] NOT NULL CHECK (cardinality(allowed_worlds) > 0),
    CHECK (expires_at > created_at)
);

CREATE INDEX ephemeral_subjects_expires_at_idx ON ephemeral_subjects (expires_at);

-- Stable mapping from an external issuer's subject to an internal user.
--
-- The unique constraint is what guarantees that the same (issuer, subject)
-- always resolves to the same UserId and that two issuers using the same
-- subject string stay separate users. Self-asserted claims such as email are
-- deliberately absent: they are never an input to identity here, so an issuer
-- cannot take over a local account by asserting its address.
CREATE TABLE external_identities (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 512),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 512),
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE (issuer, subject)
);
