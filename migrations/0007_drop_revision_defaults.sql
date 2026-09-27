-- W-28 follow-up: DEFAULT on revision columns hides missing writes.
-- Backfill for roles was done in 0006 with DEFAULT 1; drop it so INSERT without
-- revision fails instead of silently filling 1 (mutation must be RED).
-- Same for users: DROP DEFAULT 0 so every INSERT must supply revision explicitly.
-- auth_sessions revision stays without DEFAULT and allows 0 because it is internal
-- and not exposed via OpenAPI (POST /v1/auth/login etc. do not return it);
-- its current code writes 0 for new sessions, so no change needed.
ALTER TABLE roles ALTER COLUMN revision DROP DEFAULT;
ALTER TABLE users ALTER COLUMN revision DROP DEFAULT;
