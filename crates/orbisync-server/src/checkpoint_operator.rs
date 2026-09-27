//! Deliberately invoked, reviewed reconciliation workflow. Never used by startup.
use crate::checkpoint_generation::GenerationServices;
use orbisync_application::{
    ApplicationError,
    checkpoint_admission::{LegacyInventory, ReconciledAuthority},
};
use orbisync_domain::{InstanceId, Revision, Timestamp};
use orbisync_storage_postgres::generation_execution as execution;
use orbisync_world_runtime::Checkpoint;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

fn refused() -> ApplicationError {
    ApplicationError::port_failure("operator artifact incomplete or changed; preserve sources")
}

/// Stable artifact digest. Credentials must never be part of an artifact.
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn inventory_value(
    instance: InstanceId,
    inventory: &LegacyInventory,
) -> Result<Value, ApplicationError> {
    if inventory.source_id.is_some() != inventory.source_digest.is_some()
        || inventory.source_id.is_some() != inventory.checkpoint.is_some()
        || inventory
            .rows
            .iter()
            .any(|entity| entity.instance_id() != instance)
    {
        return Err(refused());
    }
    let mut rows = inventory.rows.clone();
    rows.sort_by_key(|e| e.id());
    let rows = Checkpoint::new(
        instance,
        Revision::INITIAL,
        rows,
        Timestamp::from_unix_millis(0).map_err(|_| refused())?,
    );
    Ok(json!({
        "source_id": inventory.source_id.map(|id| id.to_string()),
        "source_digest": inventory.source_digest.map(|d| digest(&d)),
        "checkpoint": inventory.checkpoint.as_ref().map(|c| digest(&c.payload)),
        "rows": digest(&rows.to_json_bytes().map_err(|_| refused())?),
    }))
}

/// Preserve both sources and an immutable selection fingerprint for review.
pub async fn inspect(
    service: &Arc<GenerationServices>,
    instance: InstanceId,
) -> Result<Value, ApplicationError> {
    let owned = service.clone();
    service.operator_owned(async move {
    let service = &owned;
    let (inventory, observations) =
        crate::checkpoint_reconciliation::inspect_legacy(service, instance).await?;
    execution::check()?;
    tokio::task::spawn_blocking(move || {
    let rows = Checkpoint::new(
        instance,
        Revision::INITIAL,
        inventory.rows.clone(),
        Timestamp::from_unix_millis(0).map_err(|_| refused())?,
    );
    Ok(json!({
        "version": 1,
        "instance": instance.to_string(),
        "selection": inventory_value(instance, &inventory)?,
        "observations": observations.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>(),
        "legacy_checkpoint": inventory.checkpoint.as_ref().map(|c| serde_json::from_slice::<Value>(&c.payload)).transpose().map_err(|_| refused())?,
        "persistent_rows": serde_json::from_slice::<Value>(&rows.to_json_bytes().map_err(|_| refused())?).map_err(|_| refused())?,
        "decision": null,
        "evidence": null,
        "chosen_checkpoint": null,
    }))
    }).await.map_err(|_| refused())?
    }).await
}

/// Separate privileged approval port; runtime credentials are never substituted.
#[async_trait::async_trait]
pub trait Approval: Send + Sync {
    /// Join retained approval transaction and connection cleanup.
    async fn finish_cleanup(&self) {}
    /// Persist this exact, reviewed approval using the operator credential.
    async fn approve(
        &self,
        instance: InstanceId,
        authority: &ReconciledAuthority,
        decision: orbisync_storage_postgres::ReconciliationDecision,
    ) -> Result<(), ApplicationError>;
}

#[async_trait::async_trait]
impl Approval for orbisync_storage_postgres::PgReconciliationOperator {
    async fn finish_cleanup(&self) {
        self.finish_cleanup().await;
    }
    async fn approve(
        &self,
        instance: InstanceId,
        authority: &ReconciledAuthority,
        decision: orbisync_storage_postgres::ReconciliationDecision,
    ) -> Result<(), ApplicationError> {
        self.approve(instance, authority, decision).await
    }
}

/// Validate all review evidence before approval, then convert exactly the chosen
/// state. On uncertainty the caller retains this service and explicitly retries
/// `retry_conversion`; it must never invoke this function to retry an attempt.
pub async fn apply(
    service: &Arc<GenerationServices>,
    operator: Arc<dyn Approval>,
    artifact: Value,
    approved_digest: &str,
    now: i64,
) -> Result<(), ApplicationError> {
    let owned = service.clone();
    let digest = approved_digest.to_owned();
    service
        .operator_owned(async move {
            {
                let mut stage = owned
                    .operator_stage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if stage.is_some() {
                    return Err(refused());
                }
                *stage = Some("preparing");
            }
            let result = apply_inner(&owned, operator.as_ref(), artifact, &digest, now).await;
            operator.finish_cleanup().await;
            let mut stage = owned
                .operator_stage
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *stage == Some("preparing") || result.is_ok() {
                *stage = None;
            }
            result
        })
        .await
}

async fn apply_inner(
    service: &Arc<GenerationServices>,
    operator: &dyn Approval,
    artifact: Value,
    approved_digest: &str,
    now: i64,
) -> Result<(), ApplicationError> {
    if service.pending_conversion_identity().is_some() {
        return Err(refused());
    }
    let limits = service.limits;
    let approved = approved_digest.to_owned();
    let (artifact, checkpoint, chosen_digest) = tokio::task::spawn_blocking(move || {
        if artifact.get("version").and_then(Value::as_u64) != Some(1)
            || digest(&serde_json::to_vec(&artifact).map_err(|_| refused())?) != approved
        {
            return Err(refused());
        }
        let chosen = serde_json::to_vec(&artifact["chosen_checkpoint"]).map_err(|_| refused())?;
        let chosen_digest = digest(&chosen);
        let checkpoint =
            Checkpoint::from_legacy_json_with_limits(&chosen, limits).map_err(|_| refused())?;
        checkpoint.stream_manifest(limits, &std::sync::atomic::AtomicBool::new(false))?;
        Ok((artifact, checkpoint, chosen_digest))
    })
    .await
    .map_err(|_| refused())??;
    execution::check()?;
    let instance: InstanceId = artifact["instance"]
        .as_str()
        .ok_or_else(refused)?
        .parse()
        .map_err(|_| refused())?;
    let evidence = artifact["evidence"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 32_768)
        .ok_or_else(refused)?;
    if checkpoint.instance_id != instance {
        return Err(refused());
    }
    if checkpoint
        .timestamp
        .to_unix_millis()
        .map_err(|_| refused())?
        > now
    {
        return Err(refused());
    }
    let (inventory, _) =
        crate::checkpoint_reconciliation::inspect_legacy(service, instance).await?;
    let (inventory, selection) = tokio::task::spawn_blocking(move || {
        let selection = inventory_value(instance, &inventory)?;
        Ok::<_, ApplicationError>((inventory, selection))
    })
    .await
    .map_err(|_| refused())??;
    execution::check()?;
    if selection != artifact["selection"] {
        return Err(refused());
    }
    use orbisync_storage_postgres::ReconciliationDecision;
    let decision = match artifact["decision"].as_str() {
        Some("trusted_history") if artifact["history_complete"].as_bool() == Some(true) => {
            ReconciliationDecision::TrustedHistory
        }
        Some("new_empty")
            if artifact["never_admitted"].as_bool() == Some(true)
                && inventory.source_id.is_none()
                && inventory.checkpoint.is_none()
                && inventory.rows.is_empty()
                && checkpoint.entities.is_empty()
                && checkpoint.dedup.is_empty()
                && checkpoint.revision == Revision::INITIAL =>
        {
            ReconciliationDecision::NewEmpty
        }
        Some("baseline") => {
            let boundary = crate::checkpoint_reconciliation::BaselineBoundary {
                last_old_commit: artifact["last_old_commit"].as_i64().ok_or_else(refused)?,
                cutoff: artifact["cutoff"].as_i64().ok_or_else(refused)?,
                now,
                sessions_invalidated: artifact["sessions_invalidated"].as_bool() == Some(true),
                report: evidence.to_owned(),
            };
            boundary.validate()?;
            if !checkpoint.dedup.is_empty()
                || artifact["historical_loss_accepted"].as_bool() != Some(true)
            {
                return Err(refused());
            }
            ReconciliationDecision::Baseline {
                last_old_commit: boundary.last_old_commit,
                cutoff: boundary.cutoff,
                sessions_invalidated: true,
            }
        }
        _ => return Err(refused()),
    };
    let authority = ReconciledAuthority { source_id: inventory.source_id, source_digest: inventory.source_digest,
        report: json!({"artifact_digest": approved_digest, "evidence": evidence, "chosen_digest": chosen_digest}).to_string() };
    execution::check()?;
    *service
        .operator_stage
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some("approval_pending");
    operator.approve(instance, &authority, decision).await?;
    *service
        .operator_stage
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some("approved_conversion_not_started");
    execution::check()?;
    let service_for_job = Arc::clone(service);
    let selection = artifact["selection"].clone();
    service
        .convert(authority, move || async move {
            let current = service_for_job.store.inventory(instance).await?;
            let matches = tokio::task::spawn_blocking(move || {
                inventory_value(instance, &current).map(|current| current == selection)
            })
            .await
            .map_err(|_| refused())??;
            execution::check()?;
            if !matches {
                return Err(refused());
            }
            Ok(checkpoint)
        })
        .await
}
