use super::*;
use orbisync_domain::{Revision, Timestamp};

pub use orbisync_application::checkpoint_admission::LegacyInventory;

/// Explicit operator decision, never selected by normal activation.
pub enum ReconciliationDecision {
    /// Report contains complete state and original outcome evidence.
    TrustedHistory,
    /// Report proves this is a newly created, never-admitted empty instance.
    NewEmpty,
    /// Report acknowledges historical uncertainty and a new command namespace.
    Baseline {
        /// Proven last old admission/commit, milliseconds.
        last_old_commit: i64,
        /// New UUIDv7 issuance cutoff after the full retry horizon.
        cutoff: i64,
        /// Explicit session/resume invalidation attestation.
        sessions_invalidated: bool,
    },
}

/// Separate operator credential port. Normal runtime composition never constructs
/// this adapter and cannot write authority rows. No credentials are created here.
pub struct PgReconciliationOperator {
    pool: PgPool,
    cleanup: Arc<Cleanup>,
}
impl PgReconciliationOperator {
    /// Bind an explicitly supplied operator connection pool after review.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cleanup: Arc::new(Cleanup::default()),
        }
    }
    /// Observe all retained terminal/connection cleanup for an approval attempt.
    pub async fn finish_cleanup(&self) {
        self.cleanup.drain().await;
    }
    /// Record a reviewed decision for one fixed source; neither source is changed.
    /// The SQL gate repeats selection, role, retry-horizon and non-generation checks.
    pub async fn approve(
        &self,
        instance: InstanceId,
        approval: &ReconciledAuthority,
        decision: ReconciliationDecision,
    ) -> Result<(), ApplicationError> {
        if approval.report.trim().is_empty()
            || approval.report.len() > 65_536
            || approval.source_id.is_some() != approval.source_digest.is_some()
        {
            return Err(invalid());
        }
        let (route, last_old, cutoff, invalidated) = match decision {
            ReconciliationDecision::TrustedHistory => ("trusted_history", None, None, false),
            ReconciliationDecision::NewEmpty => {
                if approval.source_id.is_some() {
                    return Err(invalid());
                }
                ("new_empty", None, None, false)
            }
            ReconciliationDecision::Baseline {
                last_old_commit,
                cutoff,
                sessions_invalidated,
            } => {
                if !sessions_invalidated
                    || last_old_commit
                        .checked_add(86_400_000)
                        .is_none_or(|end| cutoff < end)
                {
                    return Err(invalid());
                }
                (
                    "baseline",
                    Some(last_old_commit),
                    Some(cutoff),
                    sessions_invalidated,
                )
            }
        };
        let mut tx = OwnedTransaction::begin(&self.pool, Arc::clone(&self.cleanup)).await?;
        tx.check()?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        sqlx::query("SET LOCAL idle_in_transaction_session_timeout='5s'")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        let result =
            sqlx::query("SELECT checkpoint_approve_reconciliation($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(instance.as_uuid())
                .bind(approval.source_id)
                .bind(approval.source_digest.map(|d| d.to_vec()))
                .bind(&approval.report)
                .bind(route)
                .bind(last_old)
                .bind(cutoff)
                .bind(invalidated)
                .execute(&mut *tx)
                .await;
        if let Err(error) = result {
            tx.rollback().await?;
            return Err(db(error));
        }
        tx.commit().await
    }
}

impl PgGenerationStore {
    /// Read-only operator inventory. Caller must hold the global job and single
    /// legacy-conversion permits before calling. Oversize/corrupt selection fails
    /// without deleting it or falling back to an older/smaller checkpoint.
    pub async fn inventory_legacy(
        &self,
        instance: InstanceId,
    ) -> Result<LegacyInventory, ApplicationError> {
        if !self.writer.is_live() {
            return Err(invalid());
        }
        let mut tx = OwnedTransaction::begin(&self.pool, Arc::clone(&self.cleanup)).await?;
        tx.check()?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        sqlx::query("SET LOCAL idle_in_transaction_session_timeout='5s'")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        let selected: Option<(Uuid, i64)> = sqlx::query_as("SELECT id,octet_length(data::text)::bigint FROM instance_checkpoints WHERE instance_id=$1 ORDER BY revision DESC,created_at DESC,id DESC LIMIT 1")
            .bind(instance.as_uuid()).fetch_optional(&mut *tx).await.map_err(db)?;
        let mut inventory = LegacyInventory {
            source_id: selected.map(|r| r.0),
            source_digest: None,
            checkpoint: None,
            rows: Vec::new(),
        };
        if let Some((id, size)) = selected {
            if size > (2 * orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES) as i64 {
                return Err(invalid());
            }
            tx.check()?;
            let row = sqlx::query("SELECT revision,created_at,data,sha256(convert_to(data::text,'UTF8')) AS digest FROM instance_checkpoints WHERE instance_id=$1 AND id=$2")
                .bind(instance.as_uuid()).bind(id).fetch_one(&mut *tx).await.map_err(db)?;
            inventory.source_digest = Some(
                row.get::<Vec<u8>, _>("digest")
                    .try_into()
                    .map_err(|_| invalid())?,
            );
            let (payload, revision, created_at) = tokio::task::spawn_blocking(move || {
                let payload = serde_json::to_vec(&row.get::<serde_json::Value, _>("data"))
                    .map_err(|_| invalid())?;
                Ok::<_, ApplicationError>((
                    payload,
                    row.get::<i64, _>("revision"),
                    row.get::<time::OffsetDateTime, _>("created_at"),
                ))
            })
            .await
            .map_err(|_| invalid())??;
            tx.check()?;
            if payload.len() > orbisync_application::MAX_CHECKPOINT_PAYLOAD_BYTES
                || payload.len() > self.limits.max_serialized_bytes()
            {
                return Err(invalid());
            }
            inventory.checkpoint = Some(orbisync_application::AppCheckpoint::new(
                instance,
                Revision::from_u64(u64::try_from(revision).map_err(|_| invalid())?),
                payload,
                Timestamp::from_offset_date_time(created_at),
            ));
        }
        tx.check()?;
        let (count, bytes): (i64, i64) = sqlx::query_as("SELECT count(*)::bigint,coalesce(sum(octet_length(e::text)),0)::bigint + (SELECT coalesce(sum(octet_length(c.payload)+octet_length(c.component_key)),0)::bigint FROM persistent_entity_components c JOIN persistent_entities p ON p.id=c.entity_id WHERE p.instance_id=$1) FROM persistent_entities e WHERE e.instance_id=$1")
            .bind(instance.as_uuid()).fetch_one(&mut *tx).await.map_err(db)?;
        if count > 65_536 || bytes > self.limits.max_serialized_bytes() as i64 {
            return Err(invalid());
        }
        inventory.rows = crate::persistent_entity::PgPersistentEntityStore::list_in_transaction(
            instance, &mut tx,
        )
        .await?;
        tx.commit().await?;
        if !self.writer.is_live() {
            return Err(invalid());
        }
        Ok(inventory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn phase4_operator_refusal_precedes_any_database_access() {
        let pool = PgPool::connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("127.0.0.1")
                .port(1),
        );
        let operator = PgReconciliationOperator::new(pool);
        let instance = InstanceId::generate();
        let mut approval = ReconciledAuthority {
            source_id: None,
            source_digest: None,
            report: String::new(),
        };
        assert!(
            operator
                .approve(instance, &approval, ReconciliationDecision::TrustedHistory)
                .await
                .is_err()
        );
        approval.report = "explicit operator evidence".into();
        for (last_old_commit, cutoff, sessions_invalidated) in [
            (0, 86_399_999, true),
            (0, 86_400_000, false),
            (i64::MAX, i64::MAX, true),
        ] {
            assert!(
                operator
                    .approve(
                        instance,
                        &approval,
                        ReconciliationDecision::Baseline {
                            last_old_commit,
                            cutoff,
                            sessions_invalidated
                        }
                    )
                    .await
                    .is_err()
            );
        }
        approval.source_id = Some(Uuid::now_v7());
        approval.source_digest = Some([1; 32]);
        assert!(
            operator
                .approve(instance, &approval, ReconciliationDecision::NewEmpty)
                .await
                .is_err()
        );
        assert_eq!(approval.report, "explicit operator evidence");
    }
}
