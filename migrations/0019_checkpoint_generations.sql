-- Additive, opt-in generation storage. No legacy credentials or rows change.
CREATE TABLE checkpoint_writer_control (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    epoch BIGINT NOT NULL DEFAULT 0 CHECK (epoch >= 0),
    boot UUID,
    protocol INTEGER NOT NULL DEFAULT 4 CHECK (protocol = 4),
    deployment TEXT,
    writer_role NAME,
    old_writer_roles NAME[] NOT NULL DEFAULT '{}',
    exclusion_report TEXT
);
INSERT INTO checkpoint_writer_control(singleton) VALUES (TRUE);

-- The migration owner retains control-table DML. Only this checked function
-- is granted to the new runtime role during explicit operator cutover.
CREATE FUNCTION checkpoint_start_writer(identity TEXT, new_boot UUID) RETURNS BIGINT
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE c checkpoint_writer_control; t TEXT; old_role NAME;
BEGIN
    SELECT * INTO c FROM checkpoint_writer_control WHERE singleton FOR UPDATE;
    IF c.writer_role IS DISTINCT FROM session_user::name OR c.deployment IS DISTINCT FROM identity
       OR new_boot IS NULL OR new_boot='00000000-0000-0000-0000-000000000000'::uuid THEN
        RAISE EXCEPTION 'deployment not approved';
    END IF;
    IF cardinality(c.old_writer_roles)=0 OR c.exclusion_report IS NULL OR length(c.exclusion_report)=0 THEN
        RAISE EXCEPTION 'old-writer exclusion inventory and stop attestation required';
    END IF;
    FOREACH old_role IN ARRAY c.old_writer_roles LOOP
        IF EXISTS(SELECT 1 FROM pg_roles WHERE rolname=old_role AND rolcanlogin)
           OR EXISTS(SELECT 1 FROM pg_stat_activity WHERE usename=old_role)
           OR has_table_privilege(old_role,'instance_checkpoints','INSERT,UPDATE,DELETE,TRUNCATE')
           OR has_table_privilege(old_role,'persistent_entities','INSERT,UPDATE,DELETE,TRUNCATE')
           OR has_table_privilege(old_role,'persistent_entity_components','INSERT,UPDATE,DELETE,TRUNCATE') THEN
            RAISE EXCEPTION 'old writer remains enabled or connected';
        END IF;
    END LOOP;
    IF EXISTS(SELECT 1 FROM pg_roles WHERE rolname=session_user AND (rolsuper OR rolcreaterole OR rolbypassrls)) THEN
        RAISE EXCEPTION 'privileged checkpoint writer forbidden';
    END IF;
    FOREACH t IN ARRAY ARRAY['instance_checkpoints','persistent_entities','persistent_entity_components','checkpoint_writer_control','checkpoint_authority'] LOOP
        IF has_table_privilege(session_user,t,'INSERT,UPDATE,DELETE,TRUNCATE') THEN
            RAISE EXCEPTION 'legacy DML privilege must be excluded before startup';
        END IF;
    END LOOP;
    IF NOT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid()
        AND classid=19419 AND objid=4 AND objsubid=2 AND granted) THEN
        RAISE EXCEPTION 'dedicated deployment lock missing';
    END IF;
    UPDATE checkpoint_writer_control SET epoch=epoch+1,boot=new_boot WHERE singleton RETURNING epoch INTO c.epoch;
    RETURN c.epoch;
END $$;
REVOKE ALL ON FUNCTION checkpoint_start_writer(TEXT,UUID) FROM PUBLIC;

CREATE FUNCTION checkpoint_lock_writer(e BIGINT,b UUID) RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE c checkpoint_writer_control;
BEGIN
    SELECT * INTO c FROM checkpoint_writer_control WHERE singleton FOR SHARE;
    RETURN c.epoch=e AND c.boot=b AND c.protocol=4 AND c.writer_role=session_user::name;
END $$;
REVOKE ALL ON FUNCTION checkpoint_lock_writer(BIGINT,UUID) FROM PUBLIC;

CREATE TABLE checkpoint_authority (
    instance_id UUID PRIMARY KEY REFERENCES world_instances(id),
    status TEXT NOT NULL CHECK (status IN ('legacy_blocked','reconciled_ready','generation')),
    source_id UUID,
    source_digest BYTEA CHECK (octet_length(source_digest) = 32),
    report TEXT,
    cutoff_millis BIGINT,
    CHECK (status = 'legacy_blocked' OR (report IS NOT NULL AND length(report) > 0))
);
CREATE FUNCTION checkpoint_authority_lock() RETURNS TRIGGER
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    PERFORM 1 FROM checkpoint_writer_control WHERE singleton FOR SHARE;
    PERFORM pg_advisory_xact_lock(hashtextextended(COALESCE(NEW.instance_id,OLD.instance_id)::text,19419));
    IF TG_OP='DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;
END $$;
REVOKE ALL ON FUNCTION checkpoint_authority_lock() FROM PUBLIC;
CREATE TRIGGER authority_lock BEFORE INSERT OR UPDATE OR DELETE ON checkpoint_authority
FOR EACH ROW EXECUTE FUNCTION checkpoint_authority_lock();

CREATE FUNCTION checkpoint_begin_conversion(i UUID, source UUID, digest BYTEA, approval TEXT) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    IF NOT checkpoint_lock_writer(current_setting('orbisync.writer_epoch')::bigint,
        current_setting('orbisync.writer_boot')::uuid) THEN RAISE EXCEPTION 'writer fenced'; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(i::text,19419));
    UPDATE checkpoint_authority SET status='generation' WHERE instance_id=i AND status='reconciled_ready'
        AND source_id IS NOT DISTINCT FROM source AND source_digest IS NOT DISTINCT FROM digest AND report=approval;
    IF NOT FOUND THEN RAISE EXCEPTION 'reconciliation approval changed or unavailable'; END IF;
END $$;
REVOKE ALL ON FUNCTION checkpoint_begin_conversion(UUID,UUID,BYTEA,TEXT) FROM PUBLIC;
CREATE TABLE checkpoint_generations (
    instance_id UUID NOT NULL REFERENCES checkpoint_authority(instance_id),
    generation_id UUID NOT NULL,
    publish_seq BIGINT NOT NULL CHECK (publish_seq > 0),
    revision BIGINT NOT NULL CHECK (revision >= 0),
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    boot UUID NOT NULL,
    codec INTEGER NOT NULL CHECK (codec = 4),
    completed_millis BIGINT NOT NULL,
    total BIGINT NOT NULL CHECK (total BETWEEN 1 AND 67108864),
    chunk_bytes BIGINT NOT NULL CHECK (chunk_bytes BETWEEN 16384 AND 1048576),
    chunk_count BIGINT NOT NULL CHECK (chunk_count BETWEEN 1 AND 4096),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    state_digest BYTEA NOT NULL CHECK (octet_length(state_digest) = 32),
    PRIMARY KEY(instance_id,generation_id),
    UNIQUE(instance_id,publish_seq),
    UNIQUE(instance_id,generation_id,publish_seq),
    CHECK (chunk_count = 1 + (total - 1) / chunk_bytes)
);
CREATE TABLE checkpoint_chunks (
    instance_id UUID NOT NULL,
    generation_id UUID NOT NULL,
    chunk_index BIGINT NOT NULL CHECK (chunk_index >= 0),
    data BYTEA NOT NULL,
    byte_length BIGINT NOT NULL CHECK (byte_length BETWEEN 1 AND 1048576),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    PRIMARY KEY(instance_id,generation_id,chunk_index),
    FOREIGN KEY(instance_id,generation_id) REFERENCES checkpoint_generations DEFERRABLE INITIALLY DEFERRED,
    CHECK (octet_length(data) = byte_length),
    CHECK (sha256(data) = digest)
);
CREATE TABLE checkpoint_heads (
    instance_id UUID PRIMARY KEY REFERENCES checkpoint_authority(instance_id),
    generation_id UUID NOT NULL,
    publish_seq BIGINT NOT NULL,
    FOREIGN KEY(instance_id,generation_id,publish_seq)
        REFERENCES checkpoint_generations(instance_id,generation_id,publish_seq)
);
CREATE TABLE checkpoint_retained (
    instance_id UUID NOT NULL,
    generation_id UUID NOT NULL,
    PRIMARY KEY(instance_id,generation_id),
    FOREIGN KEY(instance_id,generation_id) REFERENCES checkpoint_generations
);
CREATE TABLE checkpoint_projection (
    instance_id UUID PRIMARY KEY REFERENCES checkpoint_authority(instance_id),
    generation_id UUID NOT NULL,
    target_seq BIGINT NOT NULL,
    applied_seq BIGINT NOT NULL DEFAULT 0 CHECK (applied_seq >= 0 AND applied_seq <= target_seq),
    FOREIGN KEY(instance_id,generation_id,target_seq)
        REFERENCES checkpoint_generations(instance_id,generation_id,publish_seq)
);
CREATE TABLE checkpoint_projection_pins (
    instance_id UUID NOT NULL,
    generation_id UUID NOT NULL,
    PRIMARY KEY(instance_id,generation_id),
    FOREIGN KEY(instance_id,generation_id) REFERENCES checkpoint_generations
);
CREATE TABLE checkpoint_retiring (
    instance_id UUID NOT NULL,
    generation_id UUID NOT NULL,
    PRIMARY KEY(instance_id,generation_id),
    FOREIGN KEY(instance_id,generation_id) REFERENCES checkpoint_generations ON DELETE CASCADE
);
-- Compact immutable receipt identities, used to reject loss/TTL renewal even
-- at equal actor revision. Payload bytes stay exclusively in the chunk stream.
CREATE TABLE checkpoint_generation_receipts (
    instance_id UUID NOT NULL,
    generation_id UUID NOT NULL,
    command_id UUID NOT NULL,
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    created_millis BIGINT NOT NULL,
    expires_millis BIGINT NOT NULL,
    PRIMARY KEY(instance_id,generation_id,command_id),
    FOREIGN KEY(instance_id,generation_id) REFERENCES checkpoint_generations ON DELETE CASCADE,
    CHECK (expires_millis > created_millis AND
        expires_millis::numeric - created_millis::numeric = 86400000)
);

CREATE FUNCTION checkpoint_is_referenced(i UUID, g UUID) RETURNS BOOLEAN
LANGUAGE SQL STABLE SET search_path = pg_catalog, public, pg_temp AS $$
    SELECT EXISTS(SELECT 1 FROM checkpoint_heads WHERE instance_id=i AND generation_id=g)
        OR EXISTS(SELECT 1 FROM checkpoint_retained WHERE instance_id=i AND generation_id=g)
        OR EXISTS(SELECT 1 FROM checkpoint_projection WHERE instance_id=i AND generation_id=g)
        OR EXISTS(SELECT 1 FROM checkpoint_projection_pins WHERE instance_id=i AND generation_id=g)
$$;

-- All generation DML follows writer shared lock -> instance lock. Neither
-- possession of a token nor an application mutex can bypass the role gate.
CREATE FUNCTION checkpoint_guard() RETURNS TRIGGER
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE c checkpoint_writer_control; i UUID; g UUID; prior checkpoint_generations; incoming checkpoint_generations;
BEGIN
    SELECT * INTO c FROM checkpoint_writer_control WHERE singleton FOR SHARE;
    IF c.writer_role IS DISTINCT FROM session_user::name OR c.epoch <= 0 OR
       c.epoch::text IS DISTINCT FROM current_setting('orbisync.writer_epoch',true) OR
       c.boot::text IS DISTINCT FROM current_setting('orbisync.writer_boot',true) THEN
        RAISE EXCEPTION 'checkpoint writer fenced' USING ERRCODE='42501';
    END IF;
    IF TG_OP='DELETE' THEN i:=OLD.instance_id; g:=OLD.generation_id;
    ELSE i:=NEW.instance_id; g:=NEW.generation_id; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(i::text, 19419));
    IF TG_OP='DELETE' AND TG_TABLE_NAME IN ('checkpoint_heads','checkpoint_projection') THEN
        RAISE EXCEPTION 'published head and projection target cannot be removed';
    END IF;
    IF TG_TABLE_NAME='checkpoint_generations' AND TG_OP='INSERT' THEN
        IF NEW.epoch<>c.epoch OR NEW.boot<>c.boot OR NEW.codec<>c.protocol OR NOT EXISTS
            (SELECT 1 FROM checkpoint_authority WHERE instance_id=i AND status='generation') THEN
            RAISE EXCEPTION 'generation writer or authority mismatch';
        END IF;
        SELECT x.* INTO prior FROM checkpoint_heads h JOIN checkpoint_generations x USING(instance_id,generation_id) WHERE h.instance_id=i;
        IF NEW.publish_seq<>coalesce(prior.publish_seq,0)+1 OR NEW.revision<coalesce(prior.revision,0) OR
           (NEW.revision=prior.revision AND NEW.state_digest<>prior.state_digest) THEN
            RAISE EXCEPTION 'generation expected head or revision conflict';
        END IF;
    END IF;
    IF TG_TABLE_NAME='checkpoint_heads' AND TG_OP<>'DELETE' THEN
        SELECT * INTO incoming FROM checkpoint_generations WHERE instance_id=i AND generation_id=g;
        IF incoming.epoch<>c.epoch OR incoming.boot<>c.boot OR
            (TG_OP='UPDATE' AND NEW.publish_seq<>OLD.publish_seq+1) THEN
            RAISE EXCEPTION 'stale head publication';
        END IF;
        IF TG_OP='UPDATE' AND EXISTS(
            SELECT 1 FROM checkpoint_generation_receipts o LEFT JOIN checkpoint_generation_receipts n
            ON n.instance_id=o.instance_id AND n.generation_id=g AND n.command_id=o.command_id
            WHERE o.instance_id=i AND o.generation_id=OLD.generation_id
            AND (o.expires_millis>incoming.completed_millis OR n.command_id IS NOT NULL)
            AND (n.digest IS DISTINCT FROM o.digest OR n.created_millis IS DISTINCT FROM o.created_millis
                OR n.expires_millis IS DISTINCT FROM o.expires_millis)) THEN
            RAISE EXCEPTION 'receipt identity loss or renewal';
        END IF;
    END IF;
    IF TG_OP='UPDATE' AND TG_TABLE_NAME IN
        ('checkpoint_generations','checkpoint_chunks','checkpoint_generation_receipts','checkpoint_retiring') THEN
        RAISE EXCEPTION 'immutable checkpoint content';
    END IF;
    IF TG_OP='INSERT' AND TG_TABLE_NAME='checkpoint_retiring' THEN
        IF checkpoint_is_referenced(i,g) THEN RAISE EXCEPTION 'generation is pinned'; END IF;
        IF NOT EXISTS(SELECT 1 FROM checkpoint_generations x JOIN checkpoint_heads h USING(instance_id)
            WHERE x.instance_id=i AND x.generation_id=g AND x.publish_seq<h.publish_seq-2) THEN
            RAISE EXCEPTION 'only published surplus may retire';
        END IF;
    ELSIF TG_OP<>'DELETE' AND EXISTS
        (SELECT 1 FROM checkpoint_retiring WHERE instance_id=i AND generation_id=g) THEN
        RAISE EXCEPTION 'generation is retiring';
    END IF;
    IF TG_OP='DELETE' AND TG_TABLE_NAME IN
        ('checkpoint_generations','checkpoint_chunks','checkpoint_generation_receipts') THEN
        IF checkpoint_is_referenced(i,g) OR NOT EXISTS
            (SELECT 1 FROM checkpoint_retiring WHERE instance_id=i AND generation_id=g) THEN
            RAISE EXCEPTION 'generation is not retired';
        END IF;
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;
END $$;

DO $$ DECLARE t TEXT; BEGIN
    FOREACH t IN ARRAY ARRAY['checkpoint_generations','checkpoint_chunks',
        'checkpoint_heads','checkpoint_retained','checkpoint_projection',
        'checkpoint_projection_pins','checkpoint_retiring','checkpoint_generation_receipts'] LOOP
        EXECUTE format('CREATE TRIGGER writer_guard BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION checkpoint_guard()',t);
    END LOOP;
END $$;

CREATE FUNCTION checkpoint_complete() RETURNS TRIGGER
LANGUAGE plpgsql SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE i UUID; g UUID; m checkpoint_generations; n BIGINT; bytes BIGINT;
BEGIN
    IF TG_OP='DELETE' THEN i:=OLD.instance_id; g:=OLD.generation_id;
    ELSE i:=NEW.instance_id; g:=NEW.generation_id; END IF;
    IF TG_TABLE_NAME IN ('checkpoint_heads','checkpoint_projection') AND EXISTS(
        SELECT 1 FROM checkpoint_heads h FULL JOIN checkpoint_projection p USING(instance_id)
        WHERE coalesce(h.instance_id,p.instance_id)=i AND
        (h.generation_id IS DISTINCT FROM p.generation_id OR h.publish_seq IS DISTINCT FROM p.target_seq)) THEN
        RAISE EXCEPTION 'head/projection target mismatch';
    END IF;
    SELECT * INTO m FROM checkpoint_generations WHERE instance_id=i AND generation_id=g;
    IF NOT FOUND THEN RETURN NULL; END IF;
    IF TG_TABLE_NAME='checkpoint_generations' AND TG_OP='INSERT' AND NOT checkpoint_is_referenced(i,g) THEN
        RAISE EXCEPTION 'unpublished generation staging forbidden';
    END IF;
    IF EXISTS(SELECT 1 FROM checkpoint_retiring WHERE instance_id=i AND generation_id=g)
       AND NOT checkpoint_is_referenced(i,g) THEN RETURN NULL; END IF;
    SELECT count(*),coalesce(sum(byte_length),0) INTO n,bytes FROM checkpoint_chunks
        WHERE instance_id=i AND generation_id=g;
    IF n<>m.chunk_count OR bytes<>m.total OR EXISTS (
        SELECT 1 FROM checkpoint_chunks WHERE instance_id=i AND generation_id=g AND
        (chunk_index>=m.chunk_count OR byte_length<>CASE WHEN chunk_index=m.chunk_count-1
            THEN m.total-chunk_index*m.chunk_bytes ELSE m.chunk_bytes END)) THEN
        RAISE EXCEPTION 'incomplete checkpoint generation';
    END IF;
    RETURN NULL;
END $$;
DO $$ DECLARE t TEXT; BEGIN
    FOREACH t IN ARRAY ARRAY['checkpoint_generations','checkpoint_chunks',
        'checkpoint_heads','checkpoint_retained','checkpoint_projection',
        'checkpoint_projection_pins','checkpoint_retiring'] LOOP
        EXECUTE format('CREATE CONSTRAINT TRIGGER generation_complete AFTER INSERT OR UPDATE OR DELETE ON %I DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION checkpoint_complete()',t);
    END LOOP;
END $$;

-- New objects are private until explicit operator cutover. No startup grants,
-- role creation, revocation or authority conversion is performed by migration.
REVOKE ALL ON FUNCTION checkpoint_guard(), checkpoint_complete(),
    checkpoint_is_referenced(UUID,UUID) FROM PUBLIC;

CREATE FUNCTION checkpoint_authority_complete() RETURNS TRIGGER
LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM checkpoint_authority WHERE instance_id=NEW.instance_id AND status='generation')
        AND NOT EXISTS(SELECT 1 FROM checkpoint_heads WHERE instance_id=NEW.instance_id) THEN
        RAISE EXCEPTION 'generation authority requires an atomic first head';
    END IF;
    RETURN NULL;
END $$;
REVOKE ALL ON FUNCTION checkpoint_authority_complete() FROM PUBLIC;
CREATE CONSTRAINT TRIGGER authority_complete AFTER INSERT OR UPDATE ON checkpoint_authority
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION checkpoint_authority_complete();
