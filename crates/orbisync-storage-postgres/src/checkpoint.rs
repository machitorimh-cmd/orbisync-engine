//! Checkpoint persistence for `instance_runtime` (`state-and-runtime.md` §1.3).

use std::collections::HashSet;

use sqlx::PgPool;
use uuid::Uuid;

use orbisync_application::{
    AppCheckpoint as Checkpoint, ApplicationError, ApplicationErrorKind, CheckpointSaveReceipt,
    CheckpointStore, MAX_CHECKPOINT_PAYLOAD_BYTES,
};
use orbisync_domain::{InstanceId, Revision, Timestamp, UserId};

/// PostgreSQL checkpoint store.
///
/// Persists and loads instance checkpoints from `instance_checkpoints` table.
/// The `data` column is `JSONB`; the application payload is stored as JSON
/// without logging its contents verbatim.
#[derive(Debug, Clone)]
pub struct PgCheckpointStore {
    pool: PgPool,
}

impl PgCheckpointStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn map_db_err(error: sqlx::Error) -> ApplicationError {
        ApplicationError::new(ApplicationErrorKind::PortFailure, error.to_string())
    }

    fn map_json_err(detail: String) -> ApplicationError {
        ApplicationError::new(ApplicationErrorKind::PortFailure, detail)
    }

    fn conflict(detail: &'static str) -> ApplicationError {
        ApplicationError::new(ApplicationErrorKind::Conflict, detail)
    }

    fn checkpoint_state(data: &serde_json::Value) -> Option<serde_json::Value> {
        let mut state = data.as_object()?.clone();
        state.remove("timestamp");
        state.remove("dedup");
        Some(serde_json::Value::Object(state))
    }

    fn dedup_identity(entry: &serde_json::Value) -> Option<serde_json::Value> {
        let mut identity = entry.as_object()?.clone();
        identity.remove("created_at_millis");
        identity.remove("expires_at_millis");
        Some(serde_json::Value::Object(identity))
    }

    fn dedup_receipts(
        data: &serde_json::Value,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError> {
        let Some(entries) = data.get("dedup").and_then(serde_json::Value::as_array) else {
            return Ok(Vec::new());
        };
        entries
            .iter()
            .map(|entry| {
                let command_id = entry
                    .get("command_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint dedup")))?;
                let created_at_millis = entry
                    .get("created_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint dedup")))?;
                let expires_at_millis = entry
                    .get("expires_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint dedup")))?;
                Ok(CheckpointSaveReceipt {
                    command_id: command_id.to_owned(),
                    created_at_millis,
                    expires_at_millis,
                })
            })
            .collect()
    }

    fn validate_payload_metadata(
        data: &serde_json::Value,
        checkpoint: &Checkpoint,
    ) -> Result<Vec<Uuid>, ApplicationError> {
        let payload_instance_id = data
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Self::map_json_err(String::from("checkpoint instance id is missing")))?;
        let payload_instance_id = InstanceId::parse(payload_instance_id)
            .map_err(|error| Self::map_json_err(error.to_string()))?;
        if payload_instance_id != checkpoint.instance_id {
            return Err(Self::map_json_err(String::from(
                "checkpoint payload instance id does not match its storage row",
            )));
        }

        let payload_revision = data
            .get("revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Self::map_json_err(String::from("checkpoint revision is missing")))?;
        if payload_revision != checkpoint.revision.as_u64() {
            return Err(Self::map_json_err(String::from(
                "checkpoint payload revision does not match its storage row",
            )));
        }

        let payload_timestamp = data
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Self::map_json_err(String::from("checkpoint timestamp is missing")))?;
        let payload_timestamp = Timestamp::parse_rfc3339(payload_timestamp)
            .map_err(|error| Self::map_json_err(error.to_string()))?;
        // PostgreSQL TIMESTAMPTZ stores microsecond precision. Compare at that
        // precision so a valid system-clock checkpoint is not rejected merely
        // because its nanoseconds were truncated by the driver/database.
        let payload_micros = payload_timestamp
            .as_offset_date_time()
            .unix_timestamp_nanos()
            .div_euclid(1_000);
        let row_micros = checkpoint
            .created_at
            .as_offset_date_time()
            .unix_timestamp_nanos()
            .div_euclid(1_000);
        if payload_micros != row_micros {
            return Err(Self::map_json_err(String::from(
                "checkpoint payload timestamp does not match its storage row",
            )));
        }

        let entities = data
            .get("entities")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| Self::map_json_err(String::from("checkpoint entities are missing")))?;
        entities
            .iter()
            .filter_map(|entity| entity.get("owner"))
            .filter(|owner| !owner.is_null())
            .map(|owner| {
                let owner = owner.as_str().ok_or_else(|| {
                    Self::map_json_err(String::from("checkpoint owner is not a string"))
                })?;
                UserId::parse(owner)
                    .map(|user_id| user_id.as_uuid())
                    .map_err(|error| Self::map_json_err(error.to_string()))
            })
            .collect()
    }

    /// Same actor revisions may legitimately add command outcomes without
    /// changing world state. Merge those outcomes monotonically while still
    /// rejecting a stale snapshot whose entity state differs.
    fn merge_same_revision(
        durable: &serde_json::Value,
        incoming: &mut serde_json::Value,
    ) -> Result<(), ApplicationError> {
        if Self::checkpoint_state(durable) != Self::checkpoint_state(incoming) {
            return Err(Self::conflict(
                "checkpoint revision conflicts with durable instance state",
            ));
        }

        let durable_entries = durable
            .get("dedup")
            .and_then(serde_json::Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let incoming_object = incoming
            .as_object_mut()
            .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint payload")))?;
        let incoming_dedup = incoming_object
            .entry(String::from("dedup"))
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let incoming_entries = incoming_dedup
            .as_array_mut()
            .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint dedup")))?;

        let mut incoming_by_id = std::collections::HashMap::with_capacity(incoming_entries.len());
        for (index, entry) in incoming_entries.iter().enumerate() {
            let command_id = entry
                .get("command_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Self::map_json_err(String::from("invalid checkpoint dedup")))?;
            if incoming_by_id
                .insert(command_id.to_owned(), index)
                .is_some()
            {
                return Err(Self::map_json_err(String::from(
                    "duplicate checkpoint dedup command id",
                )));
            }
        }

        for durable_entry in durable_entries {
            let command_id = durable_entry
                .get("command_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    Self::map_json_err(String::from("invalid durable checkpoint dedup"))
                })?;
            if let Some(index) = incoming_by_id.get(command_id).copied() {
                if Self::dedup_identity(durable_entry)
                    != Self::dedup_identity(&incoming_entries[index])
                {
                    return Err(Self::conflict(
                        "checkpoint command outcome conflicts with durable result",
                    ));
                }
                // A timed-out write may already have committed. Preserve the
                // first durable completion timestamp on a retry.
                incoming_entries[index] = durable_entry.clone();
            } else {
                incoming_by_id.insert(command_id.to_owned(), incoming_entries.len());
                incoming_entries.push(durable_entry.clone());
            }
        }
        incoming_entries.sort_by(|left, right| {
            left.get("command_id")
                .and_then(serde_json::Value::as_str)
                .cmp(&right.get("command_id").and_then(serde_json::Value::as_str))
        });
        Ok(())
    }

    async fn validate_owner_references(&self, owners: &[Uuid]) -> Result<(), ApplicationError> {
        let owners = owners.iter().copied().collect::<HashSet<_>>();
        if owners.is_empty() {
            return Ok(());
        }
        let found: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1::uuid[])")
                .bind(owners.iter().copied().collect::<Vec<_>>())
                .fetch_all(&self.pool)
                .await
                .map_err(Self::map_db_err)?;
        if found.len() != owners.len() {
            return Err(Self::map_json_err(String::from(
                "checkpoint owner references an unknown user",
            )));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl CheckpointStore for PgCheckpointStore {
    async fn save_checkpoint(
        &self,
        checkpoint: Checkpoint,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError> {
        if checkpoint.payload.len() > MAX_CHECKPOINT_PAYLOAD_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorKind::CheckpointTooLarge,
                format!(
                    "checkpoint payload exceeds {} byte limit",
                    MAX_CHECKPOINT_PAYLOAD_BYTES
                ),
            ));
        }
        let revision = i64::try_from(checkpoint.revision.as_u64()).map_err(|_| {
            ApplicationError::new(
                ApplicationErrorKind::DomainRule,
                "checkpoint revision out of range",
            )
        })?;

        let mut data: serde_json::Value = serde_json::from_slice(&checkpoint.payload)
            .map_err(|_| Self::map_json_err(String::from("invalid checkpoint payload")))?;
        let owners = Self::validate_payload_metadata(&data, &checkpoint)?;
        self.validate_owner_references(&owners).await?;

        let id = Uuid::now_v7();
        let instance_id = checkpoint.instance_id.as_uuid();

        // The process-local fence is not enough when two server processes
        // checkpoint the same instance. Serialize the database boundary too.
        // Actor revision and durable command-outcome progress are separate:
        // a rejected command adds a dedup result without advancing the actor.
        // Serialize the boundary, reject older/different world state, and
        // monotonically merge same-revision command outcomes.
        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
            .bind(instance_id)
            .execute(&mut *tx)
            .await
            .map_err(Self::map_db_err)?;
        let latest: Option<(i64, serde_json::Value)> = sqlx::query_as(
            "SELECT revision, data FROM instance_checkpoints \
             WHERE instance_id = $1 \
             ORDER BY revision DESC, created_at DESC, id DESC LIMIT 1 FOR UPDATE",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;
        if let Some((durable_revision, durable_data)) = latest {
            if durable_revision > revision {
                return Err(Self::conflict(
                    "checkpoint revision is older than the durable instance checkpoint",
                ));
            }
            if durable_revision == revision {
                Self::merge_same_revision(&durable_data, &mut data)?;
            }
        }

        let receipts = Self::dedup_receipts(&data)?;

        sqlx::query(
            "INSERT INTO instance_checkpoints (id, instance_id, revision, data, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(instance_id)
        .bind(revision)
        .bind(data)
        .bind(checkpoint.created_at.as_offset_date_time())
        .execute(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;

        // Retention: keep only latest 3 checkpoints per instance (or 30 days).
        // The delete keeps the 3 most recent by revision and is scoped to the
        // same instance_id so other instances are unaffected. A separate age
        // cutoff (30 days) is also applied.
        let retention_result = sqlx::query(
            "DELETE FROM instance_checkpoints WHERE instance_id = $1 AND id NOT IN (SELECT id FROM instance_checkpoints WHERE instance_id = $1 ORDER BY revision DESC, created_at DESC, id DESC LIMIT 3)",
        )
        .bind(instance_id)
        .execute(&mut *tx)
        .await;

        if let Err(error) = retention_result {
            tracing::warn!(
                event = "checkpoint.retention_cleanup_failed",
                instance_id = %instance_id,
                error = %error
            );
        }

        // Age-based retention: remove checkpoints older than 30 days.
        let age_result = sqlx::query(
            "DELETE FROM instance_checkpoints WHERE instance_id = $1 AND created_at < NOW() - INTERVAL '30 days'",
        )
        .bind(instance_id)
        .execute(&mut *tx)
        .await;

        if let Err(error) = age_result {
            tracing::warn!(
                event = "checkpoint.age_cleanup_failed",
                instance_id = %instance_id,
                error = %error
            );
        }

        tx.commit().await.map_err(Self::map_db_err)?;
        Ok(receipts)
    }

    async fn load_latest(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<Checkpoint>, ApplicationError> {
        // HIGH-001 (round 3): reject a grossly oversized row before PostgreSQL
        // sends the payload to this process. The previous implementation
        // fetched the whole JSONB column and measured the serialized copy
        // afterwards, so a 500 MiB row was fully materialized in memory before
        // the rejection. `octet_length(data::text)` measures the canonical
        // JSONB text inside PostgreSQL without transferring the payload. That
        // text re-serializes with `, ` / `: ` separators and normalizes every
        // number through PostgreSQL `numeric`, so it is longer than the
        // compact payload this store hands to the runtime decoder. The
        // expansion has no proven bound: measured on the current checkpoint
        // schema it aggregates to ~1.73x, but a single exponent-notation
        // float such as `1e38` expands ~8.2x under numeric normalization
        // (measured, verifier round 4). The 2x gate is therefore an empirical
        // filter, not a proven safety bound: a row that passes it can still be
        // rejected by the exact post-fetch check below, which remains the
        // authority. The pre-fetch gate exists only so that grossly oversized
        // rows (the case this guard exists for) never reach this process.
        let mut tx = self.pool.begin().await.map_err(Self::map_db_err)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(Self::map_db_err)?;
        let legacy: bool =
            sqlx::query_scalar("SELECT checkpoint_projection_readable($1,NULL,NULL)")
                .bind(instance_id.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(Self::map_db_err)?;
        if !legacy {
            return Err(ApplicationError::port_failure(
                "instance requires explicit generation activation",
            ));
        }
        let selected: Option<(Uuid, i64)> = sqlx::query_as(
            "SELECT id, octet_length(data::text)::bigint FROM instance_checkpoints WHERE instance_id = $1 ORDER BY revision DESC, created_at DESC, id DESC LIMIT 1",
        )
        .bind(instance_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::map_db_err)?;
        let Some((selected_id, byte_length)) = selected else {
            return Ok(None);
        };
        if byte_length > 2 * MAX_CHECKPOINT_PAYLOAD_BYTES as i64 {
            return Err(ApplicationError::new(
                ApplicationErrorKind::CheckpointTooLarge,
                format!(
                    "checkpoint payload exceeds {} byte limit (rejected before reading payload)",
                    2 * MAX_CHECKPOINT_PAYLOAD_BYTES
                ),
            ));
        }

        let row: Option<(Uuid, Uuid, i64, serde_json::Value, time::OffsetDateTime)> =
            sqlx::query_as(
                "SELECT id, instance_id, revision, data, created_at FROM instance_checkpoints WHERE instance_id = $1 AND id = $2",
            )
            .bind(instance_id.as_uuid())
            .bind(selected_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Self::map_db_err)?;

        let Some((_id, db_instance_id, revision_i64, data, created_at)) = row else {
            return Ok(None);
        };

        let instance_id = InstanceId::new(db_instance_id)
            .map_err(|error| Self::map_json_err(error.to_string()))?;

        let revision_u64 = u64::try_from(revision_i64)
            .map_err(|_| Self::map_json_err(String::from("revision out of range")))?;
        let revision = Revision::from_u64(revision_u64);

        let created_at = Timestamp::from_offset_date_time(created_at);

        let payload =
            serde_json::to_vec(&data).map_err(|error| Self::map_json_err(error.to_string()))?;

        if payload.len() > MAX_CHECKPOINT_PAYLOAD_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorKind::CheckpointTooLarge,
                format!(
                    "checkpoint payload exceeds {} byte limit",
                    MAX_CHECKPOINT_PAYLOAD_BYTES
                ),
            ));
        }
        let probe = Checkpoint::new(instance_id, revision, payload.clone(), created_at);
        let owners = Self::validate_payload_metadata(&data, &probe)?;
        self.validate_owner_references(&owners).await?;

        tx.commit().await.map_err(Self::map_db_err)?;

        Ok(Some(Checkpoint::new(
            instance_id,
            revision,
            payload,
            created_at,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HIGH-001 (round 4): the over-limit save rejection must be detectable
    /// as `CheckpointTooLarge` (it feeds `checkpoint_save_rejected_total`
    /// and an `error`-level log), and it must happen before any database
    /// access — the pool here points at an unreachable address on purpose.
    #[tokio::test]
    async fn save_checkpoint_rejects_over_limit_payload_without_touching_database() {
        let options = "postgres://orbisync:invalid@127.0.0.1:1/none"
            .parse()
            .expect("connect options");
        let pool = PgPool::connect_lazy_with(options);
        let store = PgCheckpointStore::new(pool);
        let instance_id = InstanceId::new(Uuid::now_v7()).expect("instance id");
        let checkpoint = Checkpoint::new(
            instance_id,
            Revision::from_u64(1),
            vec![b'x'; MAX_CHECKPOINT_PAYLOAD_BYTES + 1],
            Timestamp::from_unix_millis(1_788_000_000_000).expect("timestamp"),
        );

        let error = store
            .save_checkpoint(checkpoint)
            .await
            .expect_err("over-limit payload must be rejected");

        assert_eq!(error.kind(), ApplicationErrorKind::CheckpointTooLarge);
    }
}
