-- Persistent entity and component rows (state-and-runtime.md section 1.2).
--
-- Persistent entity definitions are saved on spawn/delete and persistent
-- components on update. Ephemeral velocity, animation, and presence state has
-- no column here and must never be persisted (state-and-runtime.md section
-- 1.1). The JSONB encoding for transform and visibility mirrors the durable
-- checkpoint representation in `world-runtime::Checkpoint`.

CREATE TABLE persistent_entities (
    id UUID PRIMARY KEY,
    instance_id UUID NOT NULL REFERENCES world_instances(id),
    kind TEXT NOT NULL CHECK (kind IN ('avatar', 'object', 'trigger')),
    owner_id UUID NULL REFERENCES users(id),
    transform JSONB NULL,
    visibility JSONB NOT NULL,
    revision BIGINT NOT NULL CHECK (revision >= 1),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    CHECK (updated_at >= created_at)
);

CREATE INDEX persistent_entities_instance_revision_idx
    ON persistent_entities (instance_id, revision DESC);

CREATE TABLE persistent_entity_components (
    entity_id UUID NOT NULL REFERENCES persistent_entities(id) ON DELETE CASCADE,
    component_key TEXT NOT NULL
        -- DM-04: custom components are namespaced keys. The grammar mirrors
        -- `orbisync-domain::entity::validate_custom_component`: 1..=128 bytes
        -- of lowercase letters, digits, dots, hyphens, and underscores, with a
        -- non-empty namespace before the first dot and a non-empty name. The
        -- `core` namespace is server-owned and rejected.
        CHECK (component_key ~ '^[a-z0-9._-]{1,128}$')
        CHECK (component_key ~ '^[a-z0-9_-]+(\.[a-z0-9._-]+)+$')
        CHECK (component_key !~ '^core\.'),
    -- DM-04: at most 4096 bytes per component payload.
    payload BYTEA NOT NULL CHECK (octet_length(payload) <= 4096),
    PRIMARY KEY (entity_id, component_key)
);

-- DM-04: at most 16 components per entity. A cross-row limit cannot be a
-- column CHECK, so it is enforced by this trigger after every row change. A
-- statement that would exceed the limit fails and rolls back as a whole.
CREATE FUNCTION persistent_entity_components_enforce_limit()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    component_count INTEGER;
    scoped_entity_id UUID;
BEGIN
    scoped_entity_id := COALESCE(NEW.entity_id, OLD.entity_id);
    SELECT count(*) INTO component_count
    FROM persistent_entity_components
    WHERE entity_id = scoped_entity_id;

    IF component_count > 16 THEN
        RAISE EXCEPTION
            'entity % exceeds the maximum of 16 persistent components',
            scoped_entity_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END
$function$;

CREATE TRIGGER persistent_entity_components_limit_check
AFTER INSERT OR UPDATE OR DELETE ON persistent_entity_components
FOR EACH ROW
EXECUTE FUNCTION persistent_entity_components_enforce_limit();
