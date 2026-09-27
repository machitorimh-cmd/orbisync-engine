-- W-28: roles revision was missing; make delivery honest.
-- Existing rows (e.g. bootstrap admin role) backfill to 1 to satisfy
-- openapi minimum 1 and parse_if_match rejecting 0.
ALTER TABLE roles
    ADD COLUMN revision BIGINT NOT NULL DEFAULT 1 CHECK (revision >= 1);
