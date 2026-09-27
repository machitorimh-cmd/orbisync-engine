use super::*;
use crate::persistent_entity::PgPersistentEntityStore;
use orbisync_application::checkpoint_admission::{GenerationRecovery, SelectedGeneration};
use orbisync_domain::Entity;

#[async_trait::async_trait]
impl GenerationRecovery for PgGenerationStore {
    async fn cleanup(&self, instance: InstanceId) -> Result<u64, ApplicationError> {
        PgGenerationStore::cleanup(self, instance).await
    }
    async fn inventory(&self, instance: InstanceId) -> Result<LegacyInventory, ApplicationError> {
        self.inventory_legacy(instance).await
    }
    async fn pending_projections(
        &self,
        after: Option<InstanceId>,
    ) -> Result<Vec<InstanceId>, ApplicationError> {
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
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT instance_id FROM checkpoint_projection WHERE applied_seq < target_seq AND ($1::uuid IS NULL OR instance_id > $1) ORDER BY instance_id LIMIT 100")
            .bind(after.map(|id| id.as_uuid())).fetch_all(&mut *tx).await.map_err(db)?;
        tx.rollback().await?;
        ids.into_iter()
            .map(|id| InstanceId::new(id).map_err(|_| invalid()))
            .collect()
    }
    async fn select(&self, instance: InstanceId) -> Result<SelectedGeneration, ApplicationError> {
        let source = self.load(instance).await?;
        let cutoff_millis = source.cutoff_millis;
        Ok(SelectedGeneration {
            attempt: source.selected().clone(),
            source: Box::new(source),
            cutoff_millis,
        })
    }
    async fn convert(
        &self,
        attempt: &GenerationAttempt,
        source: &mut dyn GenerationSource,
        approval: &ReconciledAuthority,
    ) -> Result<GenerationResolution, ApplicationError> {
        self.publish_reconciled(attempt, source, approval).await
    }
    async fn project(
        &self,
        selected: &GenerationAttempt,
        entities: &[Entity],
    ) -> Result<(), ApplicationError> {
        if entities
            .iter()
            .any(|e| e.instance_id() != selected.instance_id)
        {
            return Err(invalid());
        }
        let seq = selected.expected_head.checked_add(1).ok_or_else(invalid)?;
        let mut tx = self.locked(selected.instance_id).await?;
        let result = async {
        tx.check()?;
        sqlx::query("SELECT checkpoint_projection_begin($1,$2,$3)")
            .bind(selected.instance_id.as_uuid()).bind(selected.generation_id).bind(seq)
            .execute(&mut *tx).await.map_err(db)?;
        for entity in entities {
            if !self.writer.is_live() { return Err(invalid()); }
            tx.check()?;
            sqlx::query("SELECT checkpoint_projection_entity($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(selected.instance_id.as_uuid()).bind(entity.id().as_uuid())
                .bind(entity.kind().as_str()).bind(entity.owner().map(|o| o.as_uuid()))
                .bind(entity.transform().map(PgPersistentEntityStore::transform_json))
                .bind(PgPersistentEntityStore::visibility_json(entity.visibility()))
                .bind(i64::try_from(entity.revision().as_u64()).map_err(|_| invalid())?)
                .bind(entity.created_at().as_offset_date_time()).bind(entity.updated_at().as_offset_date_time())
                .execute(&mut *tx).await.map_err(db)?;
            for (key, payload) in entity.components() {
                tx.check()?;
                sqlx::query("SELECT checkpoint_projection_component($1,$2,$3,$4)")
                    .bind(selected.instance_id.as_uuid()).bind(entity.id().as_uuid()).bind(key).bind(payload)
                    .execute(&mut *tx).await.map_err(db)?;
            }
        }
        tx.check()?;
        sqlx::query("UPDATE checkpoint_projection SET applied_seq=$2 WHERE instance_id=$1 AND target_seq=$2 AND generation_id=$3")
            .bind(selected.instance_id.as_uuid()).bind(seq).bind(selected.generation_id)
            .execute(&mut *tx).await.map_err(db)?;
        if !self.writer.is_live() { return Err(invalid()); }
        Ok::<(), ApplicationError>(())
        }.await;
        match result {
            Ok(()) => {}
            failure => {
                tx.rollback().await?;
                return failure;
            }
        }
        tx.commit().await
    }
}
