//! Read-only legacy inventory classification and explicit reconciliation boundary.
//! Observations never authorize a baseline or discard either source.
use orbisync_domain::{Entity, EntityId};
use orbisync_world_runtime::Checkpoint;
use std::collections::BTreeMap;

/// Operator read-only entry point using the exact production adapter and policy.
/// Inventory is returned intact alongside observations; no approval is invented.
pub async fn inspect_legacy(
    service: &std::sync::Arc<crate::checkpoint_generation::GenerationServices>,
    instance: orbisync_domain::InstanceId,
) -> Result<
    (
        orbisync_application::checkpoint_admission::LegacyInventory,
        Vec<LegacyObservation>,
    ),
    orbisync_application::ApplicationError,
> {
    let service_for_job = std::sync::Arc::clone(service);
    service
        .inspect_legacy(move || async move {
            let inventory = service_for_job.store.inventory(instance).await?;
            let limits = service_for_job.limits;
            tokio::task::spawn_blocking(move || {
                let checkpoint = inventory
                    .checkpoint
                    .as_ref()
                    .map(|stored| {
                        Checkpoint::from_legacy_json_with_limits(&stored.payload, limits).map_err(
                            |e| orbisync_application::ApplicationError::port_failure(e.to_string()),
                        )
                    })
                    .transpose()?;
                if let (Some(stored), Some(decoded)) = (&inventory.checkpoint, &checkpoint)
                    && (stored.instance_id != instance
                        || decoded.instance_id != instance
                        || stored.revision != decoded.revision)
                {
                    return Err(orbisync_application::ApplicationError::port_failure(
                        "selected legacy metadata mismatch; preserve sources",
                    ));
                }
                let observations =
                    classify_legacy(checkpoint.as_ref(), &inventory.rows, &[], false);
                Ok((inventory, observations))
            })
            .await
            .map_err(|_| {
                orbisync_application::ApplicationError::port_failure("inventory worker unavailable")
            })?
        })
        .await
}

/// An inventory observation; several may apply to the same selected snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyObservation {
    /// Semantic values agree; receipt completeness still needs history evidence.
    Matching,
    /// Selected checkpoint revision is newer than a corresponding row.
    CheckpointAhead(EntityId),
    /// A row revision is newer than the checkpoint.
    RowsAhead(EntityId),
    /// Row entity has no checkpoint counterpart.
    RowOnly(EntityId),
    /// Checkpoint entity has no row counterpart; absence is not a tombstone.
    CheckpointOnly(EntityId),
    /// Trusted inventory includes an explicit deletion record.
    Delete(EntityId),
    /// Both sources are empty; existing instances still require history proof.
    Empty,
    /// Equal revisions disagree or historical outcomes are not proven complete.
    UnknownHistory,
}

/// Classify one fixed, consistently fetched snapshot without modifying sources.
/// Deletion IDs must come from operator-supplied evidence, never inferred absence.
pub fn classify_legacy(
    checkpoint: Option<&Checkpoint>,
    rows: &[Entity],
    deletions: &[EntityId],
    history_complete: bool,
) -> Vec<LegacyObservation> {
    let entities = checkpoint.map_or(&[][..], |checkpoint| checkpoint.entities.as_slice());
    let stored: BTreeMap<_, _> = entities
        .iter()
        .map(|entity| (entity.id(), entity))
        .collect();
    let projected: BTreeMap<_, _> = rows.iter().map(|entity| (entity.id(), entity)).collect();
    let mut observations = Vec::new();
    for (id, entity) in &stored {
        match projected.get(id) {
            None => observations.push(LegacyObservation::CheckpointOnly(*id)),
            Some(row) if entity.revision() > row.revision() => {
                observations.push(LegacyObservation::CheckpointAhead(*id))
            }
            Some(row) if entity.revision() < row.revision() => {
                observations.push(LegacyObservation::RowsAhead(*id))
            }
            Some(row) if *entity != *row => observations.push(LegacyObservation::UnknownHistory),
            Some(_) => {}
        }
    }
    for id in projected.keys().filter(|id| !stored.contains_key(id)) {
        observations.push(LegacyObservation::RowOnly(*id));
    }
    observations.extend(deletions.iter().copied().map(LegacyObservation::Delete));
    if stored.is_empty() && projected.is_empty() {
        observations.push(LegacyObservation::Empty);
    }
    if observations.is_empty() {
        observations.push(LegacyObservation::Matching);
    }
    if !history_complete && !observations.contains(&LegacyObservation::UnknownHistory) {
        observations.push(LegacyObservation::UnknownHistory);
    }
    observations
}

/// Evidence required for an explicitly chosen new guarantee boundary (route B).
/// This validates timing/session prerequisites, not the truth of operator evidence.
pub struct BaselineBoundary {
    /// Proven final old-writer admission/commit in milliseconds.
    pub last_old_commit: i64,
    /// Operator-selected issuance cutoff.
    pub cutoff: i64,
    /// Current operator time; conversion before the cutoff is forbidden.
    pub now: i64,
    /// Attestation that clients stopped and sessions/resume tokens were invalidated.
    pub sessions_invalidated: bool,
    /// Explicit report acknowledging chosen values/deletions and historical loss.
    pub report: String,
}
impl BaselineBoundary {
    /// Fail closed on missing evidence or an incomplete 24-hour retry horizon.
    pub fn validate(&self) -> Result<(), orbisync_application::ApplicationError> {
        if !self.sessions_invalidated
            || self.report.trim().is_empty()
            || self
                .last_old_commit
                .checked_add(86_400_000)
                .is_none_or(|end| self.cutoff < end)
            || self.now < self.cutoff
        {
            return Err(orbisync_application::ApplicationError::port_failure(
                "reconciliation boundary incomplete; preserve both sources",
            ));
        }
        Ok(())
    }
}
