-- A coordinator retains the same UUID until a persistence effect succeeds.
-- Commit this receipt and the entity/component/audit change together, so an
-- uncertain commit can be retried without repeating a write or audit insert.
-- Successful acknowledgements remove receipts; a crash may leave an orphan,
-- which remains safe because effect UUIDs are never reused.
CREATE TABLE runtime_effect_receipts (
    event_id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
GRANT SELECT, INSERT, DELETE ON runtime_effect_receipts TO orbisync_runtime;
