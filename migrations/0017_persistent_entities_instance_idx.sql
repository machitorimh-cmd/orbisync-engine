-- Instance restore reads every entity row for an instance (all revisions),
-- so the `revision DESC` component of the original index never helps that
-- query; it was copied from `instance_checkpoints`, which only fetches the
-- latest row and does need the ordering. Replace it with a plain
-- (instance_id) index; add ordering back if paging needs it later.

DROP INDEX persistent_entities_instance_revision_idx;

CREATE INDEX persistent_entities_instance_idx
    ON persistent_entities (instance_id);
