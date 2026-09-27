-- Opt-in projection procedures. No runtime mutation grants or automatic conversion.
CREATE FUNCTION checkpoint_projection_begin(i UUID,g UUID,s BIGINT) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    IF NOT checkpoint_lock_writer(current_setting('orbisync.writer_epoch')::bigint,
        current_setting('orbisync.writer_boot')::uuid) THEN RAISE EXCEPTION 'writer fenced'; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(i::text,19419));
    IF NOT EXISTS(SELECT 1 FROM checkpoint_heads h JOIN checkpoint_authority a USING(instance_id)
        WHERE h.instance_id=i AND h.generation_id=g AND h.publish_seq=s AND a.status='generation')
        THEN RAISE EXCEPTION 'projection head changed'; END IF;
    PERFORM set_config('orbisync.projection_instance',i::text,true);
    -- FK cascade removes old components. The enclosing transaction makes the
    -- replacement invisible until all rows and applied_seq are committed.
    DELETE FROM persistent_entities WHERE instance_id=i;
END $$;

-- Narrow read-only gate; callers retain their existing authorization checks.
-- No authority reports or control-table contents are exposed to legacy roles.
CREATE FUNCTION checkpoint_projection_readable(i UUID,e BIGINT,b UUID) RETURNS BOOLEAN
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
    SELECT CASE WHEN e IS NULL AND b IS NULL THEN
        NOT EXISTS(SELECT 1 FROM checkpoint_authority WHERE instance_id=i)
    ELSE EXISTS(
        SELECT 1 FROM checkpoint_writer_control c, checkpoint_heads h
        JOIN checkpoint_authority a USING(instance_id)
        JOIN checkpoint_projection p USING(instance_id)
        WHERE c.singleton AND c.epoch=e AND c.boot=b AND c.protocol=5
        AND c.writer_role=session_user::name AND h.instance_id=i AND a.status='generation'
        AND h.publish_seq=p.applied_seq AND h.publish_seq=p.target_seq AND h.generation_id=p.generation_id)
    END
$$;

CREATE FUNCTION checkpoint_projection_entity(i UUID,e UUID,k TEXT,o UUID,t JSONB,v JSONB,r BIGINT,c TIMESTAMPTZ,u TIMESTAMPTZ) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    IF NOT checkpoint_lock_writer(current_setting('orbisync.writer_epoch')::bigint,
        current_setting('orbisync.writer_boot')::uuid)
        OR current_setting('orbisync.projection_instance',true) IS DISTINCT FROM i::text
        THEN RAISE EXCEPTION 'projection not started'; END IF;
    INSERT INTO persistent_entities(id,instance_id,kind,owner_id,transform,visibility,revision,created_at,updated_at)
        VALUES(e,i,k,o,t,v,r,c,u);
END $$;

CREATE FUNCTION checkpoint_projection_component(i UUID,e UUID,k TEXT,p BYTEA) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
    IF NOT checkpoint_lock_writer(current_setting('orbisync.writer_epoch')::bigint,
        current_setting('orbisync.writer_boot')::uuid)
        OR current_setting('orbisync.projection_instance',true) IS DISTINCT FROM i::text
        OR NOT EXISTS(SELECT 1 FROM persistent_entities WHERE id=e AND instance_id=i)
        THEN RAISE EXCEPTION 'projection not started'; END IF;
    INSERT INTO persistent_entity_components(entity_id,component_key,payload) VALUES(e,k,p);
END $$;
REVOKE ALL ON FUNCTION checkpoint_projection_begin(UUID,UUID,BIGINT),
    checkpoint_projection_entity(UUID,UUID,TEXT,UUID,JSONB,JSONB,BIGINT,TIMESTAMPTZ,TIMESTAMPTZ),
    checkpoint_projection_component(UUID,UUID,TEXT,BYTEA) FROM PUBLIC;

-- Revalidate the fixed legacy source at conversion, including its selection
-- order. Approval is still operator-owned; this function never creates it.
CREATE OR REPLACE FUNCTION checkpoint_begin_conversion(i UUID, source UUID, digest BYTEA, approval TEXT) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE selected UUID; selected_digest BYTEA;
BEGIN
    IF NOT checkpoint_lock_writer(current_setting('orbisync.writer_epoch')::bigint,
        current_setting('orbisync.writer_boot')::uuid) THEN RAISE EXCEPTION 'writer fenced'; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(i::text,19419));
    SELECT id,sha256(convert_to(data::text,'UTF8')) INTO selected,selected_digest
        FROM instance_checkpoints WHERE instance_id=i ORDER BY revision DESC,created_at DESC,id DESC LIMIT 1;
    IF selected IS DISTINCT FROM source OR selected_digest IS DISTINCT FROM digest
        THEN RAISE EXCEPTION 'selected legacy source changed'; END IF;
    UPDATE checkpoint_authority SET status='generation' WHERE instance_id=i AND status='reconciled_ready'
        AND source_id IS NOT DISTINCT FROM source AND source_digest IS NOT DISTINCT FROM digest AND report=approval;
    IF NOT FOUND THEN RAISE EXCEPTION 'reconciliation approval changed or unavailable'; END IF;
END $$;

-- Explicit operator approval only. Runtime has no authority DML, so cannot use
-- this procedure even if a deployment accidentally grants EXECUTE too broadly.
CREATE FUNCTION checkpoint_approve_reconciliation(i UUID, source UUID, digest BYTEA,
    approval TEXT, route TEXT, last_old BIGINT, cutoff BIGINT, invalidated BOOLEAN) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE selected UUID; selected_digest BYTEA;
BEGIN
    IF NOT has_table_privilege(session_user,'checkpoint_authority','INSERT')
        OR NOT has_table_privilege(session_user,'checkpoint_authority','UPDATE')
        OR approval IS NULL OR octet_length(approval)=0 OR octet_length(approval)>65536
        THEN RAISE EXCEPTION 'explicit operator approval required'; END IF;
    PERFORM 1 FROM checkpoint_writer_control WHERE singleton AND epoch>0
        AND exclusion_report IS NOT NULL FOR SHARE;
    IF NOT FOUND THEN RAISE EXCEPTION 'old-writer exclusion required'; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(i::text,19419));
    SELECT id,sha256(convert_to(data::text,'UTF8')) INTO selected,selected_digest
        FROM instance_checkpoints WHERE instance_id=i ORDER BY revision DESC,created_at DESC,id DESC LIMIT 1;
    IF selected IS DISTINCT FROM source OR selected_digest IS DISTINCT FROM digest
        THEN RAISE EXCEPTION 'selected legacy source changed'; END IF;
    IF route='baseline' THEN
        IF last_old IS NULL OR cutoff IS NULL OR NOT coalesce(invalidated,false)
            OR cutoff::numeric < last_old::numeric+86400000
            OR cutoff::numeric > extract(epoch FROM clock_timestamp())*1000
            THEN RAISE EXCEPTION 'baseline retry horizon/session evidence incomplete'; END IF;
    ELSIF route='new_empty' THEN
        IF source IS NOT NULL OR EXISTS(SELECT 1 FROM persistent_entities WHERE instance_id=i)
            OR cutoff IS NOT NULL THEN RAISE EXCEPTION 'new-empty evidence conflicts with stored data'; END IF;
    ELSIF route='trusted_history' THEN
        IF cutoff IS NOT NULL THEN RAISE EXCEPTION 'trusted history cannot reset receipt boundary'; END IF;
    ELSE RAISE EXCEPTION 'ambiguous history remains blocked'; END IF;
    INSERT INTO checkpoint_authority(instance_id,status,source_id,source_digest,report,cutoff_millis)
        VALUES(i,'reconciled_ready',source,digest,approval,cutoff)
        ON CONFLICT(instance_id) DO UPDATE SET status='reconciled_ready',source_id=EXCLUDED.source_id,
            source_digest=EXCLUDED.source_digest,report=EXCLUDED.report,cutoff_millis=EXCLUDED.cutoff_millis
        WHERE checkpoint_authority.status<>'generation';
    IF NOT FOUND THEN RAISE EXCEPTION 'generation authority cannot be rewritten'; END IF;
END $$;
REVOKE ALL ON FUNCTION checkpoint_approve_reconciliation(UUID,UUID,BYTEA,TEXT,TEXT,BIGINT,BIGINT,BOOLEAN) FROM PUBLIC;
