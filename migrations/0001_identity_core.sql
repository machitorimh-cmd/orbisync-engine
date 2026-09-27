CREATE TABLE users (
    id UUID PRIMARY KEY,
    login_id TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'disabled')),
    must_change_password BOOLEAN NOT NULL DEFAULT TRUE,
    revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE user_credentials (
    user_id UUID PRIMARY KEY REFERENCES users(id),
    password_hash TEXT NOT NULL,
    password_changed_at TIMESTAMPTZ NOT NULL,
    failed_login_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_login_count >= 0),
    locked_until TIMESTAMPTZ NULL
);

CREATE TABLE roles (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NULL
);

CREATE TABLE permissions (
    name TEXT PRIMARY KEY
);

CREATE TABLE role_permissions (
    role_id UUID NOT NULL REFERENCES roles(id),
    permission_name TEXT NOT NULL REFERENCES permissions(name),
    PRIMARY KEY (role_id, permission_name)
);

CREATE TABLE user_roles (
    user_id UUID NOT NULL REFERENCES users(id),
    role_id UUID NOT NULL REFERENCES roles(id),
    PRIMARY KEY (user_id, role_id)
);

CREATE TABLE auth_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id),
    status TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ NULL,
    revocation_reason TEXT NULL,
    revision BIGINT NOT NULL CHECK (revision >= 0)
);

CREATE INDEX auth_sessions_user_status_idx ON auth_sessions (user_id, status);
CREATE INDEX auth_sessions_expires_at_idx ON auth_sessions (expires_at);

CREATE TABLE refresh_tokens (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES auth_sessions(id) ON DELETE CASCADE,
    family_id UUID NOT NULL,
    token_digest BYTEA NOT NULL UNIQUE,
    issued_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ NULL,
    replaced_by UUID NULL REFERENCES refresh_tokens(id),
    reuse_detected_at TIMESTAMPTZ NULL
);

CREATE INDEX refresh_tokens_session_family_idx ON refresh_tokens (session_id, family_id);
CREATE INDEX refresh_tokens_expires_at_idx ON refresh_tokens (expires_at);
