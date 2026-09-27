//! Opt-in PostgreSQL generation adapter. Legacy activation remains separate.
mod driver_boundary;
mod transaction;
use transaction::{Cleanup, OwnedTransaction};
mod projection;
mod reconciliation;
pub use reconciliation::{LegacyInventory, PgReconciliationOperator, ReconciliationDecision};
#[cfg(test)]
mod tests;
mod validate;
mod writer;
pub use writer::PgWriterOwnership;

use orbisync_application::{
    ApplicationError, CheckpointLimits,
    checkpoint_admission::{
        GenerationAttempt, GenerationResolution, GenerationSource, GenerationStore, WriterPermit,
    },
    checkpoint_stream::StreamManifest,
};
use orbisync_domain::InstanceId;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Default)]
struct Pending {
    attempt: Option<GenerationAttempt>,
    absent: bool,
}
type Coordinators = Arc<Mutex<HashMap<InstanceId, Arc<Mutex<Pending>>>>>;

pub use orbisync_application::checkpoint_admission::ReconciledAuthority;

fn invalid() -> ApplicationError {
    ApplicationError::port_failure("invalid or conflicting checkpoint generation")
}
fn db(error: sqlx::Error) -> ApplicationError {
    #[cfg(test)]
    if let Some(error) = error.as_database_error() {
        eprintln!("PG contract failure: {}", error.message());
    }
    let _error = error;
    ApplicationError::port_failure("checkpoint database operation unavailable")
}

/// Explicit generation store; construction never changes instance authority.
#[derive(Debug, Clone)]
pub struct PgGenerationStore {
    pool: PgPool,
    limits: CheckpointLimits,
    writer: WriterPermit,
    coordinators: Coordinators,
    cleanup: Arc<Cleanup>,
}
impl PgGenerationStore {
    /// Bind a primary pool and the permit from a held startup ownership guard.
    pub fn new(pool: PgPool, limits: CheckpointLimits, writer: &PgWriterOwnership) -> Self {
        Self {
            pool,
            limits,
            writer: writer.permit(),
            coordinators: Arc::clone(&writer.coordinators),
            cleanup: Arc::new(Cleanup::default()),
        }
    }
    async fn coordinator(&self, instance: InstanceId) -> Arc<Mutex<Pending>> {
        self.coordinators
            .lock()
            .await
            .entry(instance)
            .or_default()
            .clone()
    }
    /// Retire a fixed attempt only after primary absence proof. The actor must
    /// also retire its matching pinned snapshot before capturing a new identity.
    pub async fn retire_absent(&self, a: &GenerationAttempt) -> Result<(), ApplicationError> {
        self.cleanup.drain().await;
        crate::generation_execution::check()?;
        let c = self.coordinator(a.instance_id).await;
        let mut pending = c.lock().await;
        if !pending.absent || pending.attempt.as_ref() != Some(a) {
            return Err(invalid());
        }
        pending.attempt = None;
        pending.absent = false;
        Ok(())
    }
    async fn locked(&self, instance: InstanceId) -> Result<OwnedTransaction, ApplicationError> {
        if !self.writer.is_live() {
            return Err(invalid());
        }
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
        sqlx::query("SELECT set_config('orbisync.writer_epoch',$1,true),set_config('orbisync.writer_boot',$2,true)")
            .bind(self.writer.token().epoch.to_string()).bind(self.writer.token().boot.to_string())
            .execute(&mut *tx).await.map_err(db)?;
        tx.check()?;
        let valid: bool = sqlx::query_scalar("SELECT checkpoint_lock_writer($1,$2)")
            .bind(self.writer.token().epoch)
            .bind(self.writer.token().boot)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        if !valid {
            self.writer.invalidate();
            return Err(invalid());
        }
        tx.check()?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,19419))")
            .bind(instance.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        Ok(tx)
    }
    async fn evidence(
        &self,
        tx: &mut OwnedTransaction,
        a: &GenerationAttempt,
    ) -> Result<GenerationResolution, ApplicationError> {
        if a.writer != self.writer.token()
            || a.expected_head < 0
            || a.expected_head == i64::MAX
            || a.codec != orbisync_application::checkpoint_record::canonical::VERSION
        {
            return Ok(GenerationResolution::Fenced);
        }
        tx.check()?;
        let row=sqlx::query("SELECT publish_seq,epoch,boot,codec,completed_millis,total,digest,chunk_bytes,chunk_count FROM checkpoint_generations WHERE instance_id=$1 AND generation_id=$2")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).fetch_optional(&mut **tx).await.map_err(db)?;
        if let Some(r) = row {
            if r.get::<i64, _>("publish_seq") != a.expected_head + 1
                || r.get::<i64, _>("epoch") != a.writer.epoch
                || r.get::<Uuid, _>("boot") != a.writer.boot
                || r.get::<i32, _>("codec") != a.codec as i32
                || r.get::<i64, _>("completed_millis") != a.completed_at_millis
                || r.get::<i64, _>("total") as u64 != a.serialized_bytes
                || r.get::<Vec<u8>, _>("digest") != a.digest
                || r.get::<i64, _>("chunk_bytes") as u64 != a.chunk_bytes
                || r.get::<i64, _>("chunk_count") as u64 != a.chunk_count
            {
                return Ok(GenerationResolution::Fenced);
            }
            return Ok(GenerationResolution::Committed {
                publish_seq: a.expected_head + 1,
            });
        }
        tx.check()?;
        let head: Option<i64> = sqlx::query_scalar(
            "SELECT publish_seq FROM checkpoint_heads WHERE instance_id=$1 FOR UPDATE",
        )
        .bind(a.instance_id.as_uuid())
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        Ok(if head.unwrap_or(0) == a.expected_head {
            GenerationResolution::ProvenAbsent
        } else {
            GenerationResolution::Fenced
        })
    }
    async fn publish_inner(
        &self,
        a: &GenerationAttempt,
        source: &mut dyn GenerationSource,
        approval: Option<&ReconciledAuthority>,
    ) -> Result<GenerationResolution, ApplicationError> {
        let m = source.manifest().clone();
        self.limits.validate_manifest(&m)?;
        if source.limits() != self.limits
            || m.serialized_bytes != a.serialized_bytes
            || m.digest != a.digest
            || m.chunk_bytes != a.chunk_bytes
            || m.chunk_count != a.chunk_count
            || a.generation_id.is_nil()
        {
            return Err(invalid());
        }
        let mut tx = self.locked(a.instance_id).await?;
        let evidence = self.evidence(&mut tx, a).await?;
        if evidence != GenerationResolution::ProvenAbsent {
            return Ok(evidence);
        }
        tx.check()?;
        let old_codec: Option<i32> = sqlx::query_scalar("SELECT g.codec FROM checkpoint_heads h JOIN checkpoint_generations g USING(instance_id,generation_id) WHERE h.instance_id=$1")
            .bind(a.instance_id.as_uuid()).fetch_optional(&mut *tx).await.map_err(db)?;
        if old_codec.is_some_and(|codec| codec != a.codec as i32) {
            return Err(ApplicationError::port_failure(
                "incompatible generation head codec; preserve source and reconcile/export explicitly",
            ));
        }
        tx.check()?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM checkpoint_authority WHERE instance_id=$1")
                .bind(a.instance_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        if let Some(approval) = approval {
            if a.expected_head != 0 || status.as_deref() != Some("reconciled_ready") {
                return Err(invalid());
            }
            tx.check()?;
            sqlx::query("SELECT checkpoint_begin_conversion($1,$2,$3,$4)")
                .bind(a.instance_id.as_uuid())
                .bind(approval.source_id)
                .bind(approval.source_digest.map(|d| d.to_vec()))
                .bind(&approval.report)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        } else if !matches!(status.as_deref(), Some("generation")) {
            // Conversion must explicitly publish a reconciled baseline through phase 4.
            return Err(ApplicationError::port_failure(
                "instance generation authority is not established",
            ));
        }
        let mut validator = validate::Validator::new(a);
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut total = 0u64;
        while let Some(bytes) = source.next_chunk().await? {
            if count >= m.chunk_count {
                return Err(invalid());
            }
            let expected = if count + 1 == m.chunk_count {
                m.serialized_bytes - count * m.chunk_bytes
            } else {
                m.chunk_bytes
            };
            total = total.checked_add(bytes.len() as u64).ok_or_else(invalid)?;
            if bytes.len() as u64 != expected || total > self.limits.max_serialized_bytes() as u64 {
                return Err(invalid());
            }
            // Bounded synchronous work before yielding to cancellation/SQL.
            for page in bytes.chunks(512) {
                validator.feed_async(page).await?;
                validate_owners(&mut tx, &mut validator.owners).await?;
                tokio::task::yield_now().await;
            }
            let mut chunk_hash = Sha256::new();
            for page in bytes.chunks(16) {
                orbisync_application::checkpoint_record::canonical::work::observe(
                    &std::sync::atomic::AtomicBool::new(false),
                    orbisync_application::checkpoint_record::canonical::work::Stage::Hash,
                    8192,
                )
                .map_err(|_| invalid())?;
                hash.update(page);
                chunk_hash.update(page);
                tokio::task::yield_now().await;
            }
            driver_boundary::run(&self.writer, async {
                tx.check()?;
                sqlx::query("INSERT INTO checkpoint_chunks(instance_id,generation_id,chunk_index,data,byte_length,digest) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(count as i64)
                .bind(&bytes).bind(bytes.len() as i64).bind(chunk_hash.finalize().to_vec())
                .execute(&mut *tx).await.map_err(db)
            }).await?;
            count += 1;
        }
        validator.finish()?;
        if count != m.chunk_count
            || total != m.serialized_bytes
            || hash.finalize().as_slice() != m.digest
        {
            return Err(invalid());
        }
        let state = validator.state.clone().finalize().to_vec();
        tx.check()?;
        let old=sqlx::query("SELECT g.revision,g.state_digest FROM checkpoint_heads h JOIN checkpoint_generations g USING(instance_id,generation_id) WHERE h.instance_id=$1")
            .bind(a.instance_id.as_uuid()).fetch_optional(&mut *tx).await.map_err(db)?;
        if let Some(old) = old {
            let revision: i64 = old.get("revision");
            if validator.revision < revision
                || (validator.revision == revision
                    && old.get::<Vec<u8>, _>("state_digest") != state)
            {
                return Err(invalid());
            }
        }
        tx.check()?;
        sqlx::query("INSERT INTO checkpoint_generations(instance_id,generation_id,publish_seq,revision,epoch,boot,codec,completed_millis,total,chunk_bytes,chunk_count,digest,state_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(a.expected_head+1).bind(validator.revision)
            .bind(a.writer.epoch).bind(a.writer.boot).bind(a.codec as i32).bind(a.completed_at_millis)
            .bind(m.serialized_bytes as i64).bind(m.chunk_bytes as i64).bind(m.chunk_count as i64).bind(m.digest.to_vec()).bind(state)
            .execute(&mut *tx).await.map_err(db)?;
        for (id, r) in &validator.receipts {
            tx.check()?;
            sqlx::query("INSERT INTO checkpoint_generation_receipts VALUES($1,$2,$3,$4,$5,$6)")
                .bind(a.instance_id.as_uuid())
                .bind(a.generation_id)
                .bind(id)
                .bind(r.digest)
                .bind(r.created)
                .bind(r.expires)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        // Preserve all still-live identities byte-for-byte; no merge or renewal.
        tx.check()?;
        let conflict:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM checkpoint_heads h JOIN checkpoint_generation_receipts o USING(instance_id,generation_id) LEFT JOIN checkpoint_generation_receipts n ON n.instance_id=o.instance_id AND n.generation_id=$2 AND n.command_id=o.command_id WHERE h.instance_id=$1 AND (o.expires_millis>$3 OR n.command_id IS NOT NULL) AND (n.digest IS DISTINCT FROM o.digest OR n.created_millis IS DISTINCT FROM o.created_millis OR n.expires_millis IS DISTINCT FROM o.expires_millis))")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(a.completed_at_millis).fetch_one(&mut *tx).await.map_err(db)?;
        if conflict {
            return Err(invalid());
        }
        tx.check()?;
        let expired_new:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM checkpoint_generation_receipts n WHERE n.instance_id=$1 AND n.generation_id=$2 AND (n.created_millis<>$3 OR n.expires_millis <= floor(extract(epoch FROM clock_timestamp())*1000)::bigint) AND NOT EXISTS(SELECT 1 FROM checkpoint_heads h JOIN checkpoint_generation_receipts o USING(instance_id,generation_id) WHERE h.instance_id=n.instance_id AND o.command_id=n.command_id AND o.digest=n.digest))")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(a.completed_at_millis).fetch_one(&mut *tx).await.map_err(db)?;
        if expired_new {
            return Err(invalid());
        }
        tx.check()?;
        sqlx::query("INSERT INTO checkpoint_heads VALUES($1,$2,$3) ON CONFLICT(instance_id) DO UPDATE SET generation_id=excluded.generation_id,publish_seq=excluded.publish_seq")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(a.expected_head+1).execute(&mut *tx).await.map_err(db)?;
        tx.check()?;
        sqlx::query("DELETE FROM checkpoint_retained WHERE instance_id=$1")
            .bind(a.instance_id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        sqlx::query("INSERT INTO checkpoint_retained SELECT instance_id,generation_id FROM checkpoint_generations g WHERE instance_id=$1 AND NOT EXISTS(SELECT 1 FROM checkpoint_retiring r WHERE r.instance_id=g.instance_id AND r.generation_id=g.generation_id) ORDER BY publish_seq DESC LIMIT 3")
            .bind(a.instance_id.as_uuid()).execute(&mut *tx).await.map_err(db)?;
        tx.check()?;
        sqlx::query("INSERT INTO checkpoint_projection(instance_id,generation_id,target_seq) VALUES($1,$2,$3) ON CONFLICT(instance_id) DO UPDATE SET generation_id=excluded.generation_id,target_seq=excluded.target_seq")
            .bind(a.instance_id.as_uuid()).bind(a.generation_id).bind(a.expected_head+1).execute(&mut *tx).await.map_err(db)?;
        if !self.writer.is_live() {
            return Err(invalid());
        }
        // Never turn a COMMIT transport error into absence or a fresh attempt.
        Ok(match tx.commit().await {
            Ok(()) => GenerationResolution::Committed {
                publish_seq: a.expected_head + 1,
            },
            Err(_) => GenerationResolution::Uncertain,
        })
    }

    /// Publish an explicitly approved baseline and change authority atomically.
    /// A missing/mismatching reconciliation report fails closed without changes.
    pub async fn publish_reconciled(
        &self,
        a: &GenerationAttempt,
        source: &mut dyn GenerationSource,
        approval: &ReconciledAuthority,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.publish_coordinated(a, source, Some(approval)).await
    }
    async fn publish_coordinated(
        &self,
        a: &GenerationAttempt,
        source: &mut dyn GenerationSource,
        approval: Option<&ReconciledAuthority>,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.cleanup.drain().await;
        crate::generation_execution::check()?;
        let c = self.coordinator(a.instance_id).await;
        let mut pending = c.lock().await;
        if pending.attempt.as_ref().is_some_and(|old| old != a) {
            source.cancel().await;
            return Ok(GenerationResolution::Fenced);
        }
        pending.attempt = Some(a.clone());
        pending.absent = false;
        let result = self.publish_inner(a, source, approval).await;
        source.cancel().await;
        if matches!(result, Ok(GenerationResolution::Committed { .. })) {
            pending.attempt = None;
        }
        result
    }

    /// Select exactly one head in an RR snapshot. No predecessor or legacy fallback.
    pub async fn load(&self, instance: InstanceId) -> Result<PgGenerationSource, ApplicationError> {
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
        let r=sqlx::query("SELECT g.*,a.cutoff_millis FROM checkpoint_heads h JOIN checkpoint_authority a USING(instance_id) JOIN checkpoint_generations g USING(instance_id,generation_id) WHERE h.instance_id=$1 AND a.status='generation' AND NOT EXISTS(SELECT 1 FROM checkpoint_retiring r WHERE r.instance_id=g.instance_id AND r.generation_id=g.generation_id)")
            .bind(instance.as_uuid()).fetch_one(&mut *tx).await.map_err(db)?;
        let digest: Vec<u8> = r.get("digest");
        let manifest = StreamManifest {
            serialized_bytes: r.get::<i64, _>("total") as u64,
            chunk_bytes: r.get::<i64, _>("chunk_bytes") as u64,
            chunk_count: r.get::<i64, _>("chunk_count") as u64,
            digest: digest.try_into().map_err(|_| invalid())?,
        };
        self.limits.validate_manifest(&manifest)?;
        let selected = GenerationAttempt {
            instance_id: instance,
            generation_id: r.get("generation_id"),
            expected_head: r.get::<i64, _>("publish_seq") - 1,
            writer: orbisync_application::checkpoint_admission::WriterToken {
                epoch: r.get("epoch"),
                boot: r.get("boot"),
            },
            completed_at_millis: r.get("completed_millis"),
            codec: r.get::<i32, _>("codec") as u32,
            serialized_bytes: manifest.serialized_bytes,
            chunk_bytes: manifest.chunk_bytes,
            chunk_count: manifest.chunk_count,
            digest: manifest.digest,
        };
        Ok(PgGenerationSource {
            cutoff_millis: r.get("cutoff_millis"),
            tx: Some(tx),
            instance: instance.as_uuid(),
            generation: r.get("generation_id"),
            manifest,
            limits: self.limits,
            index: 0,
            total: 0,
            hash: Sha256::new(),
            writer: self.writer.clone(),
            cancelled: false,
            validator: validate::Validator::new(&selected),
            revision: r.get("revision"),
            state_digest: r.get("state_digest"),
            selected,
        })
    }

    /// One bounded GC pass. Caller must hold its instance coordinator and exclude
    /// unresolved attempts. SQL independently protects every durable reference.
    pub async fn cleanup(&self, instance: InstanceId) -> Result<u64, ApplicationError> {
        let c = self.coordinator(instance).await;
        let pending = c.lock().await;
        if pending.attempt.is_some() {
            return Err(ApplicationError::port_failure(
                "generation attempt unresolved",
            ));
        }
        let mut tx = self.locked(instance).await?;
        tx.check()?;
        let candidate:Option<Uuid>=sqlx::query_scalar("SELECT generation_id FROM checkpoint_generations g WHERE instance_id=$1 AND NOT checkpoint_is_referenced(instance_id,generation_id) ORDER BY publish_seq LIMIT 1")
            .bind(instance.as_uuid()).fetch_optional(&mut *tx).await.map_err(db)?;
        let Some(g) = candidate else {
            tx.rollback().await?;
            return Ok(0);
        };
        tx.check()?;
        sqlx::query("INSERT INTO checkpoint_retiring VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(instance.as_uuid())
            .bind(g)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.check()?;
        let n=sqlx::query("DELETE FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2 AND chunk_index IN(SELECT chunk_index FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2 ORDER BY chunk_index LIMIT 256)")
            .bind(instance.as_uuid()).bind(g).execute(&mut *tx).await.map_err(db)?.rows_affected();
        tx.check()?;
        sqlx::query("DELETE FROM checkpoint_generation_receipts WHERE instance_id=$1 AND generation_id=$2 AND NOT EXISTS(SELECT 1 FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2)")
            .bind(instance.as_uuid()).bind(g).execute(&mut *tx).await.map_err(db)?;
        tx.check()?;
        sqlx::query("DELETE FROM checkpoint_generations WHERE instance_id=$1 AND generation_id=$2 AND NOT EXISTS(SELECT 1 FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2)")
            .bind(instance.as_uuid()).bind(g).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await?;
        Ok(n)
    }
}
#[async_trait::async_trait]
impl GenerationStore for PgGenerationStore {
    async fn finish_cleanup(&self) {
        self.cleanup.drain().await;
    }
    fn limits(&self) -> CheckpointLimits {
        self.limits
    }
    async fn retire_absent(&self, a: &GenerationAttempt) -> Result<(), ApplicationError> {
        PgGenerationStore::retire_absent(self, a).await
    }
    async fn publish(
        &self,
        a: &GenerationAttempt,
        source: &mut dyn GenerationSource,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.publish_coordinated(a, source, None).await
    }
    async fn resolve(
        &self,
        a: &GenerationAttempt,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.cleanup.drain().await;
        crate::generation_execution::check()?;
        let c = self.coordinator(a.instance_id).await;
        let mut pending = c.lock().await;
        if pending.attempt.as_ref().is_some_and(|old| old != a) {
            return Ok(GenerationResolution::Fenced);
        }
        let mut tx = match self.locked(a.instance_id).await {
            Ok(tx) => tx,
            Err(_) => return Ok(GenerationResolution::Uncertain),
        };
        let result = self.evidence(&mut tx, a).await;
        tx.rollback().await?;
        match &result {
            Ok(GenerationResolution::Committed { .. }) => pending.attempt = None,
            Ok(GenerationResolution::ProvenAbsent) => {
                pending.attempt = Some(a.clone());
                pending.absent = true;
            }
            _ => {}
        }
        result
    }
}

/// Page-one ordered source retaining its RR transaction through EOF/cancel.
pub struct PgGenerationSource {
    cutoff_millis: Option<i64>,
    tx: Option<OwnedTransaction>,
    instance: Uuid,
    generation: Uuid,
    manifest: StreamManifest,
    limits: CheckpointLimits,
    index: u64,
    total: u64,
    hash: Sha256,
    writer: WriterPermit,
    cancelled: bool,
    selected: GenerationAttempt,
    validator: validate::Validator,
    revision: i64,
    state_digest: Vec<u8>,
}

// At most 32 distinct UUIDs, no second full owner array. Clear only after the
// query succeeds; an interrupted await keeps its validation input owned.
async fn validate_owners(
    tx: &mut OwnedTransaction,
    owners: &mut Vec<Uuid>,
) -> Result<(), ApplicationError> {
    if owners.is_empty() {
        return Ok(());
    }
    tx.check()?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id=ANY($1)")
        .bind(owners.as_slice())
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if count as usize != owners.len() {
        return Err(invalid());
    }
    owners.clear();
    Ok(())
}
impl PgGenerationSource {
    /// Exact selected identity. Activation must finish verified decoding before
    /// using its sequence (expected_head + 1) and publishing an actor.
    pub fn selected(&self) -> &GenerationAttempt {
        &self.selected
    }
}
#[async_trait::async_trait]
impl GenerationSource for PgGenerationSource {
    fn limits(&self) -> CheckpointLimits {
        self.limits
    }
    fn manifest(&self) -> &StreamManifest {
        &self.manifest
    }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
        if self.cancelled || !self.writer.is_live() {
            return Err(invalid());
        }
        if self.index == self.manifest.chunk_count {
            if self.total != self.manifest.serialized_bytes
                || self.hash.clone().finalize().as_slice() != self.manifest.digest
            {
                return Err(invalid());
            }
            self.validator.finish()?;
            if self.validator.revision != self.revision
                || self.validator.state.clone().finalize().as_slice() != self.state_digest
            {
                return Err(invalid());
            }
            if let Some(tx) = self.tx.take() {
                tx.commit().await?;
            }
            return Ok(None);
        }
        let tx = self.tx.as_mut().ok_or_else(invalid)?;
        // SQL byte-length predicate rejects oversized corrupt storage before driver allocation.
        tx.check()?;
        let r=sqlx::query("SELECT data,digest FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2 AND chunk_index=$3 AND octet_length(data)<=$4")
            .bind(self.instance).bind(self.generation).bind(self.index as i64).bind(self.manifest.chunk_bytes as i64)
            .fetch_one(&mut **tx).await.map_err(db)?;
        // Borrow driver-owned data; Vec decoding would clone the entire C
        // buffer synchronously. Our output copy has explicit yield boundaries.
        let data: &[u8] = r.try_get("data").map_err(db)?;
        let mut bytes = Vec::with_capacity(data.len());
        for page in data.chunks(512) {
            orbisync_application::checkpoint_record::canonical::work::observe(
                &std::sync::atomic::AtomicBool::new(false),
                orbisync_application::checkpoint_record::canonical::work::Stage::Copy,
                8192,
            )
            .map_err(|_| invalid())?;
            bytes.extend_from_slice(page);
            tokio::task::yield_now().await;
        }
        let expected = if self.index + 1 == self.manifest.chunk_count {
            self.manifest.serialized_bytes - self.index * self.manifest.chunk_bytes
        } else {
            self.manifest.chunk_bytes
        };
        if bytes.len() as u64 != expected {
            return Err(invalid());
        }
        let mut chunk_hash = Sha256::new();
        for page in bytes.chunks(16) {
            orbisync_application::checkpoint_record::canonical::work::observe(
                &std::sync::atomic::AtomicBool::new(false),
                orbisync_application::checkpoint_record::canonical::work::Stage::Hash,
                8192,
            )
            .map_err(|_| invalid())?;
            chunk_hash.update(page);
            self.hash.update(page);
            tokio::task::yield_now().await;
        }
        if chunk_hash.finalize().as_slice() != r.get::<Vec<u8>, _>("digest") {
            return Err(invalid());
        }
        self.total += bytes.len() as u64;
        for page in bytes.chunks(512) {
            self.validator.feed_async(page).await?;
            validate_owners(tx, &mut self.validator.owners).await?;
            tokio::task::yield_now().await;
        }
        self.index += 1;
        Ok(Some(bytes))
    }
    async fn cancel(&mut self) {
        self.cancelled = true;
        if let Some(tx) = self.tx.take() {
            let _result = tx.rollback().await;
        }
    }
}
