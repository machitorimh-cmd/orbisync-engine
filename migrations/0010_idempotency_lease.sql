-- P2-C2: idempotency lease and owner token for safe retry/abandon.
ALTER TABLE idempotency_records ADD COLUMN owner_token UUID;
ALTER TABLE idempotency_records ADD COLUMN lease_until TIMESTAMPTZ;
-- Existing completed rows keep NULL; in_progress rows must have owner/lease (enforced in code).
-- Cleanup keeps 24h retention; lease is short (30s) for concurrent retry.
