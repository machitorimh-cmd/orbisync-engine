-- Add the staged closed profile without rewriting any codec-4 rows/hashes.
-- Existing control rows remain at protocol 4 and are deliberately blocked.
-- Operator-reviewed writer exclusion and transition to 5 are later gates.
ALTER TABLE checkpoint_writer_control DROP CONSTRAINT checkpoint_writer_control_protocol_check;
ALTER TABLE checkpoint_writer_control ADD CONSTRAINT checkpoint_writer_control_protocol_check CHECK (protocol IN (4,5));
ALTER TABLE checkpoint_writer_control ALTER COLUMN protocol SET DEFAULT 5;
ALTER TABLE checkpoint_generations DROP CONSTRAINT checkpoint_generations_codec_check;
ALTER TABLE checkpoint_generations ADD CONSTRAINT checkpoint_generations_codec_check CHECK (codec IN (4,5));

CREATE OR REPLACE FUNCTION checkpoint_lock_writer(e BIGINT,b UUID) RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE c checkpoint_writer_control;
BEGIN
    SELECT * INTO c FROM checkpoint_writer_control WHERE singleton FOR SHARE;
    RETURN c.epoch=e AND c.boot=b AND c.protocol=5 AND c.writer_role=session_user::name;
END $$;

-- The new binary declares its protocol at startup; no automatic row conversion
-- or control-table update occurs here. Existing deployment approval still runs.
CREATE FUNCTION checkpoint_start_writer(identity TEXT,new_boot UUID,requested_protocol INTEGER) RETURNS BIGINT
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE p INTEGER;
BEGIN
    SELECT protocol INTO p FROM checkpoint_writer_control WHERE singleton FOR UPDATE;
    IF requested_protocol<>5 OR p<>5 THEN
        RAISE EXCEPTION 'checkpoint protocol transition requires explicit reconciliation and writer exclusion';
    END IF;
    RETURN checkpoint_start_writer(identity,new_boot);
END $$;
REVOKE ALL ON FUNCTION checkpoint_start_writer(TEXT,UUID,INTEGER) FROM PUBLIC;

CREATE FUNCTION checkpoint_canonical_profile_guard() RETURNS TRIGGER
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE previous_codec INTEGER; incoming_codec INTEGER;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.instance_id::text,19419));
    SELECT g.codec INTO previous_codec FROM checkpoint_heads h
        JOIN checkpoint_generations g USING(instance_id,generation_id)
        WHERE h.instance_id=NEW.instance_id;
    IF TG_TABLE_NAME='checkpoint_generations' THEN incoming_codec:=NEW.codec;
    ELSE
        SELECT codec INTO incoming_codec FROM checkpoint_generations
            WHERE instance_id=NEW.instance_id AND generation_id=NEW.generation_id;
    END IF;
    IF incoming_codec IS DISTINCT FROM 5 OR (previous_codec IS NOT NULL AND previous_codec<>incoming_codec) THEN
        RAISE EXCEPTION 'incompatible checkpoint generation codec; preserve source and reconcile/export explicitly';
    END IF;
    RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION checkpoint_canonical_profile_guard() FROM PUBLIC;
CREATE TRIGGER canonical_generation_profile BEFORE INSERT ON checkpoint_generations
    FOR EACH ROW EXECUTE FUNCTION checkpoint_canonical_profile_guard();
CREATE TRIGGER canonical_head_profile BEFORE INSERT OR UPDATE ON checkpoint_heads
    FOR EACH ROW EXECUTE FUNCTION checkpoint_canonical_profile_guard();
