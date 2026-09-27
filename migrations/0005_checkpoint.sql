-- Checkpoint persistence for instance_runtime (state-and-runtime.md §1.3).

CREATE TABLE instance_checkpoints (
    id UUID PRIMARY KEY,
    instance_id UUID NOT NULL REFERENCES world_instances(id),
    revision BIGINT NOT NULL CHECK (revision >= 0),
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX instance_checkpoints_instance_revision_idx
    ON instance_checkpoints (instance_id, revision DESC);

CREATE INDEX instance_checkpoints_created_at_idx
    ON instance_checkpoints (created_at DESC);
