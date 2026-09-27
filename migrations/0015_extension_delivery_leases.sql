-- MEDIUM-003/004: fenced leases and terminal-state invariants for extension delivery.

ALTER TABLE extension_deliveries
    ADD COLUMN lease_owner UUID NULL,
    ADD COLUMN lease_token UUID NULL,
    ADD COLUMN lease_expires_at TIMESTAMPTZ NULL;

CREATE INDEX extension_deliveries_claim_idx
    ON extension_deliveries (available_at, lease_expires_at, delivery_id)
    WHERE delivered_at IS NULL AND dead_lettered_at IS NULL;

ALTER TABLE extension_deliveries
    ADD CONSTRAINT extension_deliveries_terminal_state_check
    CHECK (NOT (delivered_at IS NOT NULL AND dead_lettered_at IS NOT NULL))
    NOT VALID;

-- Preserve legacy orphan history while enforcing the relationship for new rows.
ALTER TABLE extension_dead_letters
    ADD CONSTRAINT extension_dead_letters_delivery_fk
    FOREIGN KEY (delivery_id) REFERENCES extension_deliveries (delivery_id)
    NOT VALID;
