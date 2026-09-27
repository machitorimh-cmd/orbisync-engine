-- LOW-002: operator-only audit event retention.
--
-- The application role remains append-only. Retention is exposed through
-- SECURITY DEFINER functions instead of granting DELETE on audit_events to an
-- operator login. Every purge call is one bounded transaction.

DO $roles$
BEGIN
    IF NOT EXISTS (
        SELECT FROM pg_catalog.pg_roles
        WHERE rolname = 'orbisync_audit_maintenance'
    ) THEN
        CREATE ROLE orbisync_audit_maintenance
            NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT;
    ELSE
        ALTER ROLE orbisync_audit_maintenance
            NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT;
    END IF;
END
$roles$;

CREATE SCHEMA audit_operations;

CREATE TABLE audit_operations.retention_archives (
    archive_id UUID PRIMARY KEY,
    cutoff TIMESTAMPTZ NOT NULL,
    exported_rows BIGINT NOT NULL CHECK (exported_rows >= 0),
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-fA-F]{64}$'),
    deleted_rows BIGINT NOT NULL DEFAULT 0 CHECK (deleted_rows >= 0),
    verified_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    verified_by TEXT NOT NULL DEFAULT session_user
);

REVOKE ALL ON SCHEMA audit_operations FROM PUBLIC;
REVOKE ALL ON audit_operations.retention_archives FROM PUBLIC;

-- Reassert the append-only boundary in the retention migration as defense in
-- depth. The maintenance role receives no direct table privilege.
REVOKE UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER
    ON public.audit_events FROM orbisync_runtime;
GRANT SELECT, INSERT ON public.audit_events TO orbisync_runtime;
REVOKE ALL ON public.audit_events FROM orbisync_audit_maintenance;
REVOKE ALL ON audit_operations.retention_archives FROM orbisync_runtime;

-- Keep the policy bound in the database as well as in the application config
-- and shell wrapper. The upper bound prevents accidental multi-century purges.
CREATE FUNCTION audit_operations.validate_retention_days(p_retention_days INTEGER)
RETURNS INTEGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, audit_operations
AS $function$
BEGIN
    IF p_retention_days IS NULL OR p_retention_days < 1 OR p_retention_days > 3650 THEN
        RAISE EXCEPTION 'retention_days must be in 1..3650'
            USING ERRCODE = '22023';
    END IF;
    RETURN p_retention_days;
END
$function$;

-- Record the export manifest only after the operator has produced and checked
-- the archive. The advisory lock serializes this job with purge calls. Runtime
-- inserts are not made to take the lock, so purge rechecks the count before
-- every batch and fails closed if an old row appears during the handoff.
CREATE FUNCTION audit_operations.record_verified_archive(
    p_archive_id UUID,
    p_cutoff TIMESTAMPTZ,
    p_retention_days INTEGER,
    p_exported_rows BIGINT,
    p_sha256 TEXT
)
RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, audit_operations
SET lock_timeout = '5s'
SET statement_timeout = '30s'
AS $function$
DECLARE
    actual_rows BIGINT;
BEGIN
    PERFORM audit_operations.validate_retention_days(p_retention_days);
    PERFORM pg_advisory_xact_lock(
        hashtextextended('orbisync.audit_events.retention', 0)
    );

    IF p_archive_id IS NULL OR p_cutoff IS NULL OR p_cutoff >= clock_timestamp() THEN
        RAISE EXCEPTION 'retention cutoff must be in the past and archive_id is required'
            USING ERRCODE = '22023';
    END IF;
    IF p_exported_rows IS NULL OR p_exported_rows < 0 OR p_sha256 IS NULL
        OR p_sha256 !~ '^[0-9a-fA-F]{64}$' THEN
        RAISE EXCEPTION 'invalid archive manifest'
            USING ERRCODE = '22023';
    END IF;

    SELECT count(*) INTO actual_rows
    FROM public.audit_events
    WHERE occurred_at < p_cutoff;

    IF actual_rows <> p_exported_rows THEN
        RAISE EXCEPTION
            'archive row count changed: expected %, found %',
            p_exported_rows, actual_rows
            USING ERRCODE = '40001';
    END IF;

    INSERT INTO audit_operations.retention_archives
        (archive_id, cutoff, exported_rows, sha256)
    VALUES
        (p_archive_id, p_cutoff, p_exported_rows, lower(p_sha256));

    RETURN actual_rows;
END
$function$;

-- Delete one bounded batch. Repeated calls with the same archive_id are
-- idempotent. The candidate CTE is intentionally ordered and row-locked;
-- SKIP LOCKED lets an unrelated row lock be deferred to a later batch while
-- the transaction-level advisory lock prevents duplicate retention jobs.
CREATE FUNCTION audit_operations.purge_audit_events(
    p_archive_id UUID,
    p_batch_size INTEGER
)
RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, audit_operations
SET lock_timeout = '5s'
SET statement_timeout = '30s'
AS $function$
DECLARE
    archive_row audit_operations.retention_archives%ROWTYPE;
    actual_rows BIGINT;
    deleted BIGINT;
    remaining BIGINT;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended('orbisync.audit_events.retention', 0)
    );

    IF p_archive_id IS NULL OR p_batch_size IS NULL
        OR p_batch_size < 1 OR p_batch_size > 10000 THEN
        RAISE EXCEPTION 'archive_id and batch size (1..10000) are required'
            USING ERRCODE = '22023';
    END IF;

    SELECT * INTO archive_row
    FROM audit_operations.retention_archives
    WHERE archive_id = p_archive_id
    FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'verified archive is required before purge'
            USING ERRCODE = '55000';
    END IF;

    IF archive_row.deleted_rows >= archive_row.exported_rows THEN
        RETURN 0;
    END IF;

    -- Recheck the export boundary in the same transaction. If a runtime
    -- writer added an eligible row after export, no row is deleted.
    SELECT count(*) INTO actual_rows
    FROM public.audit_events
    WHERE occurred_at < archive_row.cutoff;
    remaining := archive_row.exported_rows - archive_row.deleted_rows;

    IF actual_rows <> remaining THEN
        RAISE EXCEPTION
            'audit rows changed since archive verification: expected %, found %',
            remaining, actual_rows
            USING ERRCODE = '40001';
    END IF;

    WITH candidates AS (
        SELECT id
        FROM public.audit_events
        WHERE occurred_at < archive_row.cutoff
        ORDER BY occurred_at, id
        LIMIT LEAST(p_batch_size::BIGINT, remaining)
        FOR UPDATE SKIP LOCKED
    )
    DELETE FROM public.audit_events AS events
    USING candidates
    WHERE events.id = candidates.id;
    GET DIAGNOSTICS deleted = ROW_COUNT;

    UPDATE audit_operations.retention_archives
    SET deleted_rows = deleted_rows + deleted
    WHERE archive_id = p_archive_id;

    RETURN deleted;
END
$function$;

-- This status query is intentionally operator-only so the runbook can report
-- the post-run oldest eligible row and count without exposing a new runtime
-- capability.
CREATE FUNCTION audit_operations.retention_status(p_archive_id UUID)
RETURNS TABLE (
    cutoff TIMESTAMPTZ,
    remaining_count BIGINT,
    remaining_oldest TIMESTAMPTZ
)
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, audit_operations
SET statement_timeout = '30s'
AS $function$
    SELECT a.cutoff,
           count(e.id)::BIGINT,
           min(e.occurred_at)
    FROM audit_operations.retention_archives AS a
    LEFT JOIN public.audit_events AS e
      ON e.occurred_at < a.cutoff
    WHERE a.archive_id = $1
    GROUP BY a.archive_id, a.cutoff
$function$;

REVOKE ALL ON FUNCTION audit_operations.validate_retention_days(INTEGER) FROM PUBLIC;
REVOKE ALL ON FUNCTION audit_operations.record_verified_archive(UUID, TIMESTAMPTZ, INTEGER, BIGINT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION audit_operations.purge_audit_events(UUID, INTEGER) FROM PUBLIC;
REVOKE ALL ON FUNCTION audit_operations.retention_status(UUID) FROM PUBLIC;
GRANT USAGE ON SCHEMA audit_operations TO orbisync_audit_maintenance;
GRANT EXECUTE ON FUNCTION audit_operations.validate_retention_days(INTEGER)
    TO orbisync_audit_maintenance;
GRANT EXECUTE ON FUNCTION audit_operations.record_verified_archive(UUID, TIMESTAMPTZ, INTEGER, BIGINT, TEXT)
    TO orbisync_audit_maintenance;
GRANT EXECUTE ON FUNCTION audit_operations.purge_audit_events(UUID, INTEGER)
    TO orbisync_audit_maintenance;
GRANT EXECUTE ON FUNCTION audit_operations.retention_status(UUID)
    TO orbisync_audit_maintenance;
