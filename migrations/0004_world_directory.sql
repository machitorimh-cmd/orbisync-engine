-- World directory for Milestone 2.

CREATE TABLE world_definitions (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
    capacity INTEGER NOT NULL CHECK (capacity >= 1 AND capacity <= 1000),
    default_spawn JSONB NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    revision BIGINT NOT NULL CHECK (revision >= 1),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE world_instances (
    id UUID PRIMARY KEY,
    world_id UUID NOT NULL REFERENCES world_definitions(id),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('created', 'running', 'stopping', 'stopped')),
    capacity INTEGER NOT NULL CHECK (capacity >= 1 AND capacity <= 1000),
    created_at TIMESTAMPTZ NOT NULL,
    started_at TIMESTAMPTZ NULL,
    revision BIGINT NOT NULL CHECK (revision >= 1)
);

CREATE INDEX world_instances_world_id_idx ON world_instances (world_id);
