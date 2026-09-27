//! Instance-scoped realtime command idempotency admission.
//!
//! This store is intentionally before the actor mailbox. A leader owns the
//! only admission for a `(instance, command_id)` pair; concurrent followers
//! wait for its result and therefore cannot execute the command twice. The
//! bounded records are included in the instance checkpoint, so idle reap and
//! process restart retain the contractual 24-hour replay window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use orbisync_domain::{CommandId, EntityKind, InstanceId, UserId};
use orbisync_protocol::v1::EntityCommand;
use orbisync_world_runtime::{CheckpointDedupEntry, CheckpointDedupResult};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

/// Command result retained for idempotent replay.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandDedupResult {
    /// The command applied and this is the reliable event/ack to return.
    Applied {
        /// The authoritative command/event payload.
        command: EntityCommand,
        /// Stable envelope identifier reused for every replay.
        message_id: String,
    },
    /// The command was rejected deterministically.
    Rejected {
        /// Stable protocol error code.
        code: String,
        /// Stable protocol error detail.
        detail: String,
        /// Stable envelope identifier reused for every replay.
        message_id: String,
    },
}

impl CommandDedupResult {
    fn message_id(&self) -> &str {
        match self {
            Self::Applied { message_id, .. } | Self::Rejected { message_id, .. } => message_id,
        }
    }
}

#[derive(Debug)]
struct Entry {
    fingerprint: [u8; 32],
    created_at: i64,
    expires_at: i64,
    state: EntryState,
    notify: Arc<Notify>,
}

#[derive(Debug, Clone)]
enum EntryState {
    Pending(Option<CommandDedupResult>),
    Complete(CommandDedupResult),
    /// The command was applied locally but the checkpoint port remained
    /// unavailable after the bounded recovery window. Keep it fenced in this
    /// process rather than allowing a retry to execute the mutation twice.
    Dirty(CommandDedupResult),
}

#[derive(Debug, Default)]
struct StoreInner {
    entries: HashMap<InstanceId, HashMap<CommandId, Entry>>,
}

/// Admission result for a command received from realtime transport.
#[derive(Debug)]
pub enum CommandDedupAdmission {
    /// This request is the only request allowed to dispatch to the actor.
    Leader(CommandDedupReservation),
    /// A prior request completed; return this result without dispatching.
    Duplicate(CommandDedupResult),
    /// A prior request is still running; await its result without dispatching.
    InFlight(CommandDedupWaiter),
    /// The local actor applied the command but checkpoint recovery is still
    /// unavailable; retry after the client receives a bounded failure.
    PersistenceUnavailable,
    /// The ID was already used for another payload.
    Conflict,
    /// The bounded store has no room for a new in-flight command.
    Capacity,
}

/// Bounded instance-scoped command result store.
#[derive(Clone, Debug)]
pub struct CommandDedupStore {
    inner: Arc<Mutex<StoreInner>>,
    durability_locks: Arc<Mutex<HashMap<InstanceId, Weak<tokio::sync::Mutex<()>>>>>,
    capacity_per_instance: usize,
    ttl_millis: i64,
}

impl Default for CommandDedupStore {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY_PER_INSTANCE)
    }
}

impl CommandDedupStore {
    /// The default maximum number of retained IDs for one instance.
    pub const DEFAULT_CAPACITY_PER_INSTANCE: usize = 4_096;
    /// The contractual retention period for completed command results.
    pub const DEFAULT_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;
    /// Production must never be configured above the checkpoint record bound.
    pub const MAX_CAPACITY_PER_INSTANCE: usize = orbisync_world_runtime::MAX_DEDUP_RECORDS;

    /// Creates a store with a bounded per-instance capacity.
    #[must_use]
    pub fn new(capacity_per_instance: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreInner::default())),
            durability_locks: Arc::new(Mutex::new(HashMap::new())),
            capacity_per_instance: capacity_per_instance.clamp(1, Self::MAX_CAPACITY_PER_INSTANCE),
            ttl_millis: Self::DEFAULT_TTL_MILLIS,
        }
    }

    /// Serializes checkpoint creation for command outcomes so concurrent
    /// commands cannot save same-revision snapshots that omit each other.
    pub async fn durability_guard(
        &self,
        instance_id: InstanceId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .durability_locks
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locks.retain(|_, lock| lock.strong_count() != 0);
            if let Some(lock) = locks.get(&instance_id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(instance_id, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }

    /// Returns the deterministic fingerprint of the command payload.
    ///
    /// `command_id` is excluded so a caller can compare the same logical
    /// payload after rebuilding its protobuf envelope.
    #[must_use]
    pub fn fingerprint(command: &EntityCommand) -> [u8; 32] {
        let mut payload = command.clone();
        payload.command_id.clear();
        payload.instance_revision = None;
        let mut bytes = Vec::new();
        payload.encode(&mut bytes).ok();
        Sha256::digest(bytes).into()
    }

    /// Admits a command before rate limiting, backpressure, or actor dispatch.
    pub fn begin(
        &self,
        instance_id: InstanceId,
        command_id: CommandId,
        fingerprint: [u8; 32],
        now_millis: i64,
    ) -> CommandDedupAdmission {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entries = inner.entries.entry(instance_id).or_default();
        entries.retain(|_, entry| {
            entry.expires_at > now_millis
                || matches!(entry.state, EntryState::Pending(_) | EntryState::Dirty(_))
        });
        if let Some(entry) = entries.get(&command_id) {
            if entry.fingerprint != fingerprint {
                return CommandDedupAdmission::Conflict;
            }
            return match &entry.state {
                EntryState::Complete(result) => CommandDedupAdmission::Duplicate(result.clone()),
                EntryState::Pending(_) => CommandDedupAdmission::InFlight(CommandDedupWaiter {
                    notify: Arc::clone(&entry.notify),
                    store: self.clone(),
                    instance_id,
                    command_id,
                    fingerprint,
                }),
                EntryState::Dirty(_) => CommandDedupAdmission::PersistenceUnavailable,
            };
        }
        if entries.len() >= self.capacity_per_instance {
            // A completed result remains replayable until its completion-based
            // TTL expires. Never evict it to admit a newer command.
            return CommandDedupAdmission::Capacity;
        }
        entries.insert(
            command_id,
            Entry {
                fingerprint,
                created_at: now_millis,
                expires_at: now_millis.saturating_add(self.ttl_millis),
                state: EntryState::Pending(None),
                notify: Arc::new(Notify::new()),
            },
        );
        CommandDedupAdmission::Leader(CommandDedupReservation {
            store: self.clone(),
            instance_id,
            command_id,
            fingerprint,
            completed: false,
        })
    }

    fn complete(
        &self,
        instance_id: InstanceId,
        command_id: CommandId,
        fingerprint: [u8; 32],
        result: CommandDedupResult,
        now_millis: i64,
    ) {
        let notify = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let Some(entries) = inner.entries.get_mut(&instance_id) else {
                return;
            };
            let Some(entry) = entries.get_mut(&command_id) else {
                return;
            };
            if entry.fingerprint != fingerprint {
                return;
            }
            entry.created_at = now_millis;
            entry.expires_at = now_millis.saturating_add(self.ttl_millis);
            entry.state = EntryState::Complete(result);
            Arc::clone(&entry.notify)
        };
        notify.notify_waiters();
    }

    fn remove_pending(
        &self,
        instance_id: InstanceId,
        command_id: CommandId,
        fingerprint: [u8; 32],
    ) {
        let notify = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let notify = if let Some(entries) = inner.entries.get_mut(&instance_id) {
                let Some(entry) = entries.get_mut(&command_id) else {
                    return;
                };
                if entry.fingerprint != fingerprint {
                    return;
                }
                match &entry.state {
                    EntryState::Pending(Some(result)) => {
                        // A cancelled/panicked persistence worker must not
                        // release an already-applied command for re-execution.
                        let result = result.clone();
                        entry.state = EntryState::Dirty(result);
                        Some(Arc::clone(&entry.notify))
                    }
                    EntryState::Pending(None) => {
                        entries.remove(&command_id).map(|entry| entry.notify)
                    }
                    EntryState::Complete(_) | EntryState::Dirty(_) => None,
                }
            } else {
                None
            };
            if inner
                .entries
                .get(&instance_id)
                .is_some_and(HashMap::is_empty)
            {
                inner.entries.remove(&instance_id);
            }
            notify
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    fn result(
        &self,
        instance_id: InstanceId,
        command_id: CommandId,
        fingerprint: [u8; 32],
    ) -> Option<Option<CommandDedupResult>> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entry = inner.entries.get(&instance_id)?.get(&command_id)?;
        if entry.fingerprint != fingerprint {
            return None;
        }
        match &entry.state {
            EntryState::Pending(_) => Some(None),
            EntryState::Complete(result) => Some(Some(result.clone())),
            EntryState::Dirty(_) => Some(None),
        }
    }

    /// Removes expired completed records and returns the number evicted.
    pub fn prune(&self, now_millis: i64) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut removed = 0;
        for entries in inner.entries.values_mut() {
            let before = entries.len();
            entries.retain(|_, entry| {
                entry.expires_at > now_millis
                    || matches!(entry.state, EntryState::Pending(_) | EntryState::Dirty(_))
            });
            removed += before.saturating_sub(entries.len());
        }
        inner.entries.retain(|_, entries| !entries.is_empty());
        removed
    }

    /// Returns the number of entries retained for an instance.
    #[must_use]
    pub fn len(&self, instance_id: InstanceId) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .get(&instance_id)
            .map_or(0, HashMap::len)
    }

    /// Exports completed outcomes for the instance checkpoint.
    pub fn snapshot(&self, instance_id: InstanceId, now_millis: i64) -> Vec<CheckpointDedupEntry> {
        self.prune(now_millis);
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .entries
            .get(&instance_id)
            .into_iter()
            .flat_map(HashMap::iter)
            .filter_map(|(command_id, entry)| {
                let result = match &entry.state {
                    EntryState::Complete(result)
                    | EntryState::Pending(Some(result))
                    | EntryState::Dirty(result) => result,
                    EntryState::Pending(None) => return None,
                };
                Some(Self::checkpoint_entry(
                    command_id,
                    entry.fingerprint,
                    entry.created_at,
                    entry.expires_at,
                    result,
                ))
            })
            .collect()
    }

    /// Exports the current completed outcomes plus a result that is still
    /// pending durable publication. This closes the success-before-checkpoint
    /// window without making the request visible as a replay until the write
    /// has committed.
    pub fn snapshot_with_outcome(
        &self,
        instance_id: InstanceId,
        now_millis: i64,
        command_id: CommandId,
        fingerprint: [u8; 32],
        result: &CommandDedupResult,
    ) -> Vec<CheckpointDedupEntry> {
        let mut entries = self.snapshot(instance_id, now_millis);
        let durable_entry = Self::checkpoint_entry(
            &command_id,
            fingerprint,
            now_millis,
            now_millis.saturating_add(Self::DEFAULT_TTL_MILLIS),
            result,
        );
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.command_id == command_id.to_string())
        {
            // Pending entries initially carry their admission time. Replace
            // it for every persistence attempt so the accepted checkpoint's
            // replay window starts at durable completion, not admission.
            *entry = durable_entry;
        } else {
            debug_assert!(entries.len() < Self::MAX_CAPACITY_PER_INSTANCE);
            entries.push(durable_entry);
        }
        entries
    }

    fn checkpoint_entry(
        command_id: &CommandId,
        fingerprint: [u8; 32],
        created_at: i64,
        expires_at: i64,
        result: &CommandDedupResult,
    ) -> CheckpointDedupEntry {
        let message_id = result.message_id().to_owned();
        let result = match result {
            CommandDedupResult::Applied {
                command: payload, ..
            } => {
                // EntityCommand encoding is infallible for prost messages.
                let response_payload = payload.encode_to_vec();
                CheckpointDedupResult::Applied { response_payload }
            }
            CommandDedupResult::Rejected { code, detail, .. } => CheckpointDedupResult::Rejected {
                code: code.clone(),
                detail: detail.clone(),
            },
        };
        CheckpointDedupEntry {
            command_id: command_id.to_string(),
            fingerprint: fingerprint.to_vec(),
            created_at_millis: created_at,
            expires_at_millis: expires_at,
            message_id,
            result,
        }
    }

    fn is_stable_error_code(code: &str) -> bool {
        matches!(
            code,
            "CONFLICT"
                | "INVALID_ARGUMENT"
                | "INSTANCE_NOT_RUNNING"
                | "INSTANCE_MISMATCH"
                | "INSTANCE_FULL"
                | "REVISION_OVERFLOW"
                | "NOT_OWNER"
                | "INVALID_TRANSFORM"
                | "PERMISSION_DENIED"
                | "INVALID_TIMESTAMP"
                | "ENTITY_EXISTS"
                | "ENTITY_NOT_FOUND"
                | "REVISION_MISMATCH"
                | "COMPONENT_RATE_LIMITED"
                | "NOT_FOUND"
                | "MAILBOX_SATURATED"
                | "MAILBOX_CLOSED"
                | "INVALID_COMPONENT"
                | "INVALID_VALUE"
                | "PERSISTENCE_UNAVAILABLE"
                | "CHECKPOINT_CAPACITY"
                | "EXPLICIT_SPAWN_REQUIRED"
                | "PERSISTENCE_BACKPRESSURE"
        )
    }

    /// Restores checkpoint outcomes, rejecting malformed or tampered records.
    /// Returns the number of records accepted. Expired records are ignored.
    pub fn restore(
        &self,
        instance_id: InstanceId,
        records: &[CheckpointDedupEntry],
        now_millis: i64,
    ) -> Result<usize, String> {
        if records.len() > Self::MAX_CAPACITY_PER_INSTANCE {
            return Err(String::from("invalid dedup checkpoint"));
        }
        let mut seen_ids = std::collections::HashSet::with_capacity(records.len());
        let mut aggregate_bytes = 0_usize;
        let mut restored = Vec::with_capacity(records.len());
        for record in records {
            if record.created_at_millis > now_millis
                || record.expires_at_millis
                    != record
                        .created_at_millis
                        .checked_add(Self::DEFAULT_TTL_MILLIS)
                        .unwrap_or(i64::MAX)
                || record.fingerprint.len() != 32
                || CommandId::parse(&record.message_id).is_err()
            {
                return Err(String::from("invalid dedup checkpoint"));
            }
            let command_id = match CommandId::parse(&record.command_id) {
                Ok(value) => value,
                Err(_) => return Err(String::from("invalid dedup checkpoint")),
            };
            if !seen_ids.insert(command_id) {
                return Err(String::from("invalid dedup checkpoint"));
            }
            let fingerprint: [u8; 32] = match record.fingerprint.as_slice().try_into() {
                Ok(value) => value,
                Err(_) => return Err(String::from("invalid dedup checkpoint")),
            };
            let result = match &record.result {
                CheckpointDedupResult::Applied { response_payload } => {
                    if response_payload.is_empty()
                        || response_payload.len() > orbisync_world_runtime::MAX_DEDUP_RESPONSE_BYTES
                    {
                        return Err(String::from("invalid dedup checkpoint"));
                    }
                    let payload = match EntityCommand::decode(response_payload.as_slice()) {
                        Ok(value) => value,
                        Err(_) => return Err(String::from("invalid dedup checkpoint")),
                    };
                    if payload.command_id != command_id.to_string() {
                        return Err(String::from("invalid dedup checkpoint"));
                    }
                    if !Self::valid_replay_command(&payload) {
                        return Err(String::from("invalid dedup checkpoint"));
                    }
                    CommandDedupResult::Applied {
                        command: payload,
                        message_id: record.message_id.clone(),
                    }
                }
                CheckpointDedupResult::Rejected { code, detail } => {
                    if code.is_empty()
                        || code.len() > orbisync_world_runtime::MAX_DEDUP_CODE_BYTES
                        || detail.len() > orbisync_world_runtime::MAX_DEDUP_DETAIL_BYTES
                        || !Self::is_stable_error_code(code)
                    {
                        return Err(String::from("invalid dedup checkpoint"));
                    }
                    CommandDedupResult::Rejected {
                        code: code.clone(),
                        detail: detail.clone(),
                        message_id: record.message_id.clone(),
                    }
                }
            };
            if record.expires_at_millis <= now_millis {
                // Expiry is a retention decision, not a validation bypass:
                // malformed records must still fail the whole restore.
                continue;
            }
            let result_bytes = match &result {
                CommandDedupResult::Applied { command, .. } => command.encoded_len(),
                CommandDedupResult::Rejected { code, detail, .. } => {
                    code.len().saturating_add(detail.len())
                }
            };
            let record_bytes = record
                .command_id
                .len()
                .saturating_add(record.fingerprint.len())
                .saturating_add(record.message_id.len())
                .saturating_add(result_bytes);
            if record_bytes > orbisync_world_runtime::MAX_DEDUP_RECORD_BYTES
                || (aggregate_bytes.saturating_add(record_bytes)
                    > orbisync_world_runtime::MAX_DEDUP_AGGREGATE_BYTES)
            {
                return Err(String::from("invalid dedup checkpoint"));
            }
            aggregate_bytes = aggregate_bytes.saturating_add(record_bytes);
            restored.push((
                command_id,
                Entry {
                    fingerprint,
                    created_at: record.created_at_millis,
                    expires_at: record.expires_at_millis,
                    state: EntryState::Complete(result),
                    notify: Arc::new(Notify::new()),
                },
            ));
        }
        restored.sort_by_key(|(_, entry)| entry.created_at);
        if restored.len() > self.capacity_per_instance {
            return Err(String::from("dedup checkpoint exceeds configured capacity"));
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let entries = inner.entries.entry(instance_id).or_default();
        // A checkpoint is the authoritative instance boundary; discard any
        // stale process-local records before importing its bounded snapshot.
        entries.clear();
        for (command_id, entry) in restored {
            entries.insert(command_id, entry);
        }
        Ok(entries.len())
    }

    fn valid_replay_command(command: &EntityCommand) -> bool {
        if CommandId::parse(&command.command_id).is_err()
            || orbisync_domain::EntityId::parse(&command.entity_id).is_err()
        {
            return false;
        }
        match command.operation.as_str() {
            "spawn" => {
                let Some(arguments) = &command.arguments else {
                    return false;
                };
                let Some(prost_types::value::Kind::StringValue(kind)) = arguments
                    .fields
                    .get("kind")
                    .and_then(|value| value.kind.as_ref())
                else {
                    return false;
                };
                if EntityKind::parse(&kind.to_ascii_lowercase()).is_err() {
                    return false;
                }
                let Some(prost_types::value::Kind::StringValue(visibility)) = arguments
                    .fields
                    .get("visibility")
                    .and_then(|value| value.kind.as_ref())
                else {
                    return false;
                };
                match visibility.as_str() {
                    "global" | "owner_only" => true,
                    "spatial" => matches!(
                        arguments
                            .fields
                            .get("visibility_radius")
                            .or_else(|| arguments.fields.get("radius"))
                            .or_else(|| arguments.fields.get("spatial_radius"))
                            .and_then(|value| value.kind.as_ref()),
                        Some(prost_types::value::Kind::NumberValue(radius))
                            if radius.is_finite() && *radius > 0.0
                    ),
                    _ => false,
                }
            }
            "update" => command.arguments.is_some(),
            "delete" => true,
            "transfer_ownership" => {
                let Some(arguments) = &command.arguments else {
                    return false;
                };
                if arguments.fields.len() > 2 {
                    return false;
                }
                let valid_user_id = |key: &str| {
                    arguments
                        .fields
                        .get(key)
                        .and_then(|value| value.kind.as_ref())
                        .and_then(|kind| match kind {
                            prost_types::value::Kind::StringValue(value) => Some(value),
                            _ => None,
                        })
                        .is_some_and(|value| UserId::parse(value).is_ok())
                };
                valid_user_id("new_owner_id")
                    && (!arguments.fields.contains_key("previous_owner_id")
                        || valid_user_id("previous_owner_id"))
            }
            _ => false,
        }
    }
}

/// Completion handle owned by the request that won command admission.
#[derive(Debug)]
pub struct CommandDedupReservation {
    store: CommandDedupStore,
    instance_id: InstanceId,
    command_id: CommandId,
    fingerprint: [u8; 32],
    completed: bool,
}

impl CommandDedupReservation {
    /// Returns the admitted command identifier for durable checkpointing.
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Returns the request fingerprint for durable checkpointing.
    #[must_use]
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// Publishes the deterministic command result to followers and replayers.
    pub fn complete(mut self, result: CommandDedupResult, now_millis: i64) {
        self.store.complete(
            self.instance_id,
            self.command_id,
            self.fingerprint,
            result,
            now_millis,
        );
        self.completed = true;
    }

    /// Associates the deterministic outcome with a still-pending reservation.
    /// Periodic checkpointing can then retain the dirty outcome even while the
    /// foreground save is timing out.
    pub fn set_outcome(&mut self, result: CommandDedupResult) {
        let mut inner = self
            .store
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = inner
            .entries
            .get_mut(&self.instance_id)
            .and_then(|entries| entries.get_mut(&self.command_id))
            && entry.fingerprint == self.fingerprint
        {
            entry.state = EntryState::Pending(Some(result));
        }
    }

    /// Leaves an unrecovered outcome fenced locally after background recovery
    /// has reached its own bounded deadline.
    pub fn mark_dirty(mut self, result: CommandDedupResult) {
        let notify = {
            let mut inner = self
                .store
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(entry) = inner
                .entries
                .get_mut(&self.instance_id)
                .and_then(|entries| entries.get_mut(&self.command_id))
            {
                if entry.fingerprint == self.fingerprint {
                    entry.state = EntryState::Dirty(result);
                    Some(Arc::clone(&entry.notify))
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        self.completed = true;
    }
}

impl Drop for CommandDedupReservation {
    fn drop(&mut self) {
        if !self.completed {
            self.store
                .remove_pending(self.instance_id, self.command_id, self.fingerprint);
        }
    }
}

/// Handle for a command currently being processed by another request.
#[derive(Debug)]
pub struct CommandDedupWaiter {
    notify: Arc<Notify>,
    store: CommandDedupStore,
    instance_id: InstanceId,
    command_id: CommandId,
    fingerprint: [u8; 32],
}

impl CommandDedupWaiter {
    /// Waits until the leader publishes its result.
    pub async fn wait(&self) -> Option<CommandDedupResult> {
        loop {
            match self
                .store
                .result(self.instance_id, self.command_id, self.fingerprint)
            {
                Some(Some(result)) => return Some(result),
                None => return None,
                Some(None) => {}
            }
            self.notify.notified().await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_boundary_is_excluded_from_request_fingerprint() {
        let mut request = command(&CommandId::generate(), "spawn");
        let expected = CommandDedupStore::fingerprint(&request);
        request.instance_revision = Some(u64::MAX);
        assert_eq!(CommandDedupStore::fingerprint(&request), expected);
    }

    #[test]
    fn restored_response_preserves_absent_legacy_or_exact_new_boundary() {
        for boundary in [None, Some(u64::MAX)] {
            let instance = InstanceId::generate();
            let id = CommandId::generate();
            let mut response = command(&id, "spawn");
            let fingerprint = CommandDedupStore::fingerprint(&response);
            response.instance_revision = boundary;
            let expected = applied(response);
            let source = CommandDedupStore::new(2);
            let CommandDedupAdmission::Leader(reservation) =
                source.begin(instance, id, fingerprint, 100)
            else {
                panic!("leader");
            };
            reservation.complete(expected.clone(), 100);
            let records = source.snapshot(instance, 101);
            let mut checkpoint = orbisync_world_runtime::Checkpoint::new(
                instance,
                orbisync_domain::Revision::from_u64(1),
                Vec::new(),
                orbisync_domain::Timestamp::from_unix_millis(101).unwrap(),
            );
            checkpoint.dedup = records;
            let serialized = checkpoint.to_json_bytes().unwrap();
            let checkpoint =
                orbisync_world_runtime::Checkpoint::from_json_bytes(&serialized).unwrap();
            let restored = CommandDedupStore::new(2);
            restored.restore(instance, &checkpoint.dedup, 102).unwrap();
            assert!(
                matches!(restored.begin(instance, id, fingerprint, 103), CommandDedupAdmission::Duplicate(actual) if actual == expected)
            );
        }
    }

    fn command(id: &CommandId, operation: &str) -> EntityCommand {
        let arguments = match operation {
            "spawn" => Some(prost_types::Struct {
                fields: [
                    (
                        "kind".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue("object".to_owned())),
                        },
                    ),
                    (
                        "visibility".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue("global".to_owned())),
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            }),
            "update" => Some(prost_types::Struct::default()),
            "transfer_ownership" => Some(prost_types::Struct {
                fields: [(
                    "new_owner_id".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            "01900000-0000-7000-8000-000000000002".to_owned(),
                        )),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            _ => None,
        };
        EntityCommand {
            command_id: id.to_string(),
            entity_id: String::from("01900000-0000-7000-8000-000000000001"),
            expected_revision: 1,
            operation: operation.to_owned(),
            arguments,
            instance_revision: None,
        }
    }

    fn applied(command: EntityCommand) -> CommandDedupResult {
        CommandDedupResult::Applied {
            message_id: command.command_id.clone(),
            command,
        }
    }

    #[tokio::test]
    async fn same_id_is_single_flight_and_replays_result() {
        let store = CommandDedupStore::new(2);
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let cmd = command(&id, "delete");
        let fp = CommandDedupStore::fingerprint(&cmd);
        let leader = match store.begin(instance, id, fp, 0) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        let follower = match store.begin(instance, id, fp, 0) {
            CommandDedupAdmission::InFlight(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        leader.complete(applied(cmd.clone()), 0);
        assert_eq!(follower.wait().await, Some(applied(cmd.clone())));
        assert!(matches!(
            store.begin(instance, id, fp, 1),
            CommandDedupAdmission::Duplicate(CommandDedupResult::Applied { command: value, .. }) if value == cmd
        ));
    }

    #[tokio::test]
    async fn durability_fence_serializes_one_instance_without_blocking_another() {
        let store = CommandDedupStore::new(2);
        let instance_a = InstanceId::generate();
        let instance_b = InstanceId::generate();
        let guard_a = store.durability_guard(instance_a).await;
        let other_store = store.clone();
        let other = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            other_store.durability_guard(instance_b),
        )
        .await
        .expect("different instances must not share a fence");
        drop(other);
        let same_store = store.clone();
        let same = tokio::spawn(async move { same_store.durability_guard(instance_a).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), same)
                .await
                .is_err()
        );
        drop(guard_a);
    }

    #[test]
    fn same_id_replays_all_entity_operations_without_new_admission() {
        let store = CommandDedupStore::new(8);
        let instance = InstanceId::generate();
        for operation in ["spawn", "update", "delete", "transfer_ownership"] {
            let id = CommandId::generate();
            let payload = command(&id, operation);
            let fingerprint = CommandDedupStore::fingerprint(&payload);
            let reservation = match store.begin(instance, id, fingerprint, 0) {
                CommandDedupAdmission::Leader(value) => value,
                other => panic!("unexpected admission: {other:?}"),
            };
            reservation.complete(applied(payload.clone()), 0);
            assert!(matches!(
                store.begin(instance, id, fingerprint, 1),
                CommandDedupAdmission::Duplicate(CommandDedupResult::Applied { command: value, .. })
                    if value == payload
            ));
        }
    }

    #[test]
    fn checkpoint_restore_preserves_success_and_rejected_outcomes() {
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let applied_command = command(&id, "spawn");
        let store = CommandDedupStore::new(4);
        let reservation = match store.begin(
            instance,
            id,
            CommandDedupStore::fingerprint(&applied_command),
            1_000,
        ) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        reservation.complete(applied(applied_command.clone()), 1_000);

        let rejected_id = CommandId::generate();
        let rejected = command(&rejected_id, "update");
        let rejected_reservation = match store.begin(
            instance,
            rejected_id,
            CommandDedupStore::fingerprint(&rejected),
            1_000,
        ) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        rejected_reservation.complete(
            CommandDedupResult::Rejected {
                code: String::from("CONFLICT"),
                detail: String::from("revision conflict"),
                message_id: rejected_id.to_string(),
            },
            1_000,
        );

        let mut checkpoint = orbisync_world_runtime::Checkpoint::new(
            instance,
            orbisync_domain::Revision::from_u64(1),
            Vec::new(),
            orbisync_domain::Timestamp::from_unix_millis(1_000).expect("timestamp"),
        );
        checkpoint.dedup = store.snapshot(instance, 1_000);
        let bytes = checkpoint.to_json_bytes().expect("checkpoint encode");
        let restored_checkpoint =
            orbisync_world_runtime::Checkpoint::from_json_bytes(&bytes).expect("checkpoint decode");

        let restarted = CommandDedupStore::new(4);
        assert_eq!(
            restarted
                .restore(instance, &restored_checkpoint.dedup, 1_001)
                .expect("valid dedup restore"),
            2
        );
        assert!(matches!(
            restarted.begin(
                instance,
                id,
                CommandDedupStore::fingerprint(&applied_command),
                1_001,
            ),
            CommandDedupAdmission::Duplicate(CommandDedupResult::Applied { command: value, .. }) if value == applied_command
        ));
        assert!(matches!(
            restarted.begin(
                instance,
                rejected_id,
                CommandDedupStore::fingerprint(&rejected),
                1_001,
            ),
            CommandDedupAdmission::Duplicate(CommandDedupResult::Rejected { code, detail, .. })
                if code == "CONFLICT" && detail == "revision conflict"
        ));

        let mut tampered = restored_checkpoint
            .dedup
            .iter()
            .find(|record| matches!(record.result, CheckpointDedupResult::Applied { .. }))
            .cloned()
            .expect("applied record");
        if let CheckpointDedupResult::Applied { response_payload } = &mut tampered.result {
            let mut response =
                EntityCommand::decode(response_payload.as_slice()).expect("response");
            response.command_id = CommandId::generate().to_string();
            response_payload.clear();
            response.encode(response_payload).expect("response encode");
        }
        let clean = CommandDedupStore::new(4);
        assert!(clean.restore(instance, &[tampered], 1_001).is_err());
    }

    #[test]
    fn checkpoint_restore_replays_all_operations_with_response_revision() {
        let instance = InstanceId::generate();
        let store = CommandDedupStore::new(8);
        for operation in ["spawn", "update", "delete", "transfer_ownership"] {
            let id = CommandId::generate();
            let request = command(&id, operation);
            let fingerprint = CommandDedupStore::fingerprint(&request);
            let reservation = match store.begin(instance, id, fingerprint, 1_000) {
                CommandDedupAdmission::Leader(value) => value,
                other => panic!("unexpected admission: {other:?}"),
            };
            let mut response = request.clone();
            response.expected_revision = 99;
            if operation == "transfer_ownership" {
                response
                    .arguments
                    .as_mut()
                    .expect("transfer arguments")
                    .fields
                    .insert(
                        "previous_owner_id".to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "01900000-0000-7000-8000-000000000003".to_owned(),
                            )),
                        },
                    );
            }
            reservation.complete(applied(response.clone()), 1_000);
            let mut checkpoint = orbisync_world_runtime::Checkpoint::new(
                instance,
                orbisync_domain::Revision::from_u64(99),
                Vec::new(),
                orbisync_domain::Timestamp::from_unix_millis(1_000).expect("timestamp"),
            );
            checkpoint.dedup = store.snapshot(instance, 1_000);
            let bytes = checkpoint.to_json_bytes().expect("checkpoint encode");
            let restored_checkpoint = orbisync_world_runtime::Checkpoint::from_json_bytes(&bytes)
                .expect("checkpoint decode");
            let restarted = CommandDedupStore::new(8);
            assert!(
                restarted
                    .restore(instance, &restored_checkpoint.dedup, 1_001)
                    .expect("valid dedup restore")
                    >= 1
            );
            assert!(matches!(
                restarted.begin(instance, id, fingerprint, 1_001),
                CommandDedupAdmission::Duplicate(CommandDedupResult::Applied { command: value, .. })
                    if value == response
            ));
        }
    }

    #[test]
    fn mismatch_and_capacity_fail_closed() {
        let store = CommandDedupStore::new(1);
        let instance = InstanceId::generate();
        let first = CommandId::generate();
        let first_cmd = command(&first, "delete");
        let first_fp = CommandDedupStore::fingerprint(&first_cmd);
        let reservation = match store.begin(instance, first, first_fp, 0) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        let mut mismatch = first_cmd.clone();
        mismatch.operation = String::from("spawn");
        assert!(matches!(
            store.begin(
                instance,
                first,
                CommandDedupStore::fingerprint(&mismatch),
                0
            ),
            CommandDedupAdmission::Conflict
        ));
        let second = CommandId::generate();
        let second_cmd = command(&second, "delete");
        assert!(matches!(
            store.begin(
                instance,
                second,
                CommandDedupStore::fingerprint(&second_cmd),
                0
            ),
            CommandDedupAdmission::Capacity
        ));
        reservation.complete(applied(first_cmd), 0);
        assert!(matches!(
            store.begin(
                instance,
                CommandId::generate(),
                CommandDedupStore::fingerprint(&command(&CommandId::generate(), "delete")),
                1,
            ),
            CommandDedupAdmission::Capacity
        ));
        assert!(matches!(
            store.begin(instance, first, first_fp, 1,),
            CommandDedupAdmission::Duplicate(_)
        ));
    }

    #[test]
    fn completed_records_are_not_evicted_and_expire_from_completion() {
        let store = CommandDedupStore::new(1);
        let instance = InstanceId::generate();
        let first = CommandId::generate();
        let first_command = command(&first, "delete");
        let reservation = match store.begin(
            instance,
            first,
            CommandDedupStore::fingerprint(&first_command),
            100,
        ) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        reservation.complete(applied(first_command), 1_000);

        let second = CommandId::generate();
        let second_command = command(&second, "delete");
        assert!(matches!(
            store.begin(
                instance,
                second,
                CommandDedupStore::fingerprint(&second_command),
                1_000 + CommandDedupStore::DEFAULT_TTL_MILLIS - 1,
            ),
            CommandDedupAdmission::Capacity
        ));
        assert!(matches!(
            store.begin(
                instance,
                first,
                CommandDedupStore::fingerprint(&command(&first, "delete")),
                1_000 + CommandDedupStore::DEFAULT_TTL_MILLIS - 1,
            ),
            CommandDedupAdmission::Duplicate(_)
        ));
        assert!(matches!(
            store.begin(
                instance,
                second,
                CommandDedupStore::fingerprint(&second_command),
                1_000 + CommandDedupStore::DEFAULT_TTL_MILLIS + 1,
            ),
            CommandDedupAdmission::Leader(_)
        ));
    }

    #[test]
    fn replay_message_id_is_persisted_and_reused() {
        let store = CommandDedupStore::new(2);
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let payload = command(&id, "delete");
        let replay = applied(payload.clone());
        let reservation =
            match store.begin(instance, id, CommandDedupStore::fingerprint(&payload), 0) {
                CommandDedupAdmission::Leader(value) => value,
                other => panic!("unexpected admission: {other:?}"),
            };
        reservation.complete(replay, 500);
        let records = store.snapshot(instance, 501);
        assert_eq!(records[0].created_at_millis, 500);
        assert_eq!(
            records[0].expires_at_millis,
            500 + CommandDedupStore::DEFAULT_TTL_MILLIS
        );
        assert_eq!(records[0].message_id, payload.command_id);
        let restarted = CommandDedupStore::new(2);
        restarted.restore(instance, &records, 501).expect("restore");
        let CommandDedupAdmission::Duplicate(CommandDedupResult::Applied { message_id, .. }) =
            restarted.begin(instance, id, CommandDedupStore::fingerprint(&payload), 501)
        else {
            panic!("expected replay");
        };
        assert_eq!(message_id, payload.command_id);
    }

    #[test]
    fn retry_snapshot_uses_success_attempt_time_for_local_replay() {
        let store = CommandDedupStore::new(2);
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let payload = command(&id, "delete");
        let fingerprint = CommandDedupStore::fingerprint(&payload);
        let reservation = match store.begin(instance, id, fingerprint, 10_000) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        let result = applied(payload.clone());
        reservation.complete(result.clone(), 10_500);

        // Model a failed first save followed by a successful retry. The
        // snapshot is generated at the successful attempt, not at admission
        // or at the earlier failed attempt.
        let successful_attempt = 12_345;
        let records =
            store.snapshot_with_outcome(instance, successful_attempt, id, fingerprint, &result);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].created_at_millis, successful_attempt);
        assert_eq!(
            records[0].expires_at_millis,
            successful_attempt + CommandDedupStore::DEFAULT_TTL_MILLIS
        );

        let mut checkpoint = orbisync_world_runtime::Checkpoint::new(
            instance,
            orbisync_domain::Revision::INITIAL,
            Vec::new(),
            orbisync_domain::Timestamp::from_unix_millis(successful_attempt).expect("timestamp"),
        );
        checkpoint.dedup = records;
        let bytes = checkpoint.to_json_bytes().expect("checkpoint encode");
        let restored =
            orbisync_world_runtime::Checkpoint::from_json_bytes(&bytes).expect("checkpoint decode");
        let replay_store = CommandDedupStore::new(2);
        replay_store
            .restore(instance, &restored.dedup, successful_attempt + 1)
            .expect("local replay restore");
        assert!(matches!(
            replay_store.begin(instance, id, fingerprint, successful_attempt + 1),
            CommandDedupAdmission::Duplicate(value) if value == result
        ));
    }

    #[test]
    fn malformed_restore_fails_closed_for_command_semantics() {
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let payload = command(&id, "delete");
        let source = CommandDedupStore::new(2);
        let reservation =
            match source.begin(instance, id, CommandDedupStore::fingerprint(&payload), 0) {
                CommandDedupAdmission::Leader(value) => value,
                other => panic!("unexpected admission: {other:?}"),
            };
        reservation.complete(applied(payload), 0);
        let mut record = source.snapshot(instance, 1);
        let CheckpointDedupResult::Applied { response_payload } = &mut record[0].result else {
            panic!("expected applied record");
        };
        let mut decoded = EntityCommand::decode(response_payload.as_slice()).expect("decode");
        decoded.operation = String::from("teleport");
        *response_payload = decoded.encode_to_vec();
        let restarted = CommandDedupStore::new(2);
        assert!(restarted.restore(instance, &record, 1).is_err());
        assert_eq!(restarted.len(instance), 0);
    }

    #[test]
    fn dirty_outcome_remains_fenced_and_is_checkpointed() {
        let store = CommandDedupStore::new(2);
        let instance = InstanceId::generate();
        let id = CommandId::generate();
        let payload = command(&id, "delete");
        let fingerprint = CommandDedupStore::fingerprint(&payload);
        let mut reservation = match store.begin(instance, id, fingerprint, 0) {
            CommandDedupAdmission::Leader(value) => value,
            other => panic!("unexpected admission: {other:?}"),
        };
        let result = applied(payload);
        reservation.set_outcome(result.clone());
        reservation.mark_dirty(result);

        assert!(matches!(
            store.begin(instance, id, fingerprint, 1),
            CommandDedupAdmission::PersistenceUnavailable
        ));
        assert_eq!(store.snapshot(instance, 1).len(), 1);
    }

    #[test]
    fn every_runtime_stable_rejection_code_is_restoreable() {
        for code in [
            "CONFLICT",
            "INVALID_ARGUMENT",
            "INSTANCE_NOT_RUNNING",
            "INSTANCE_MISMATCH",
            "INSTANCE_FULL",
            "REVISION_OVERFLOW",
            "NOT_OWNER",
            "INVALID_TRANSFORM",
            "PERMISSION_DENIED",
            "INVALID_TIMESTAMP",
            "ENTITY_EXISTS",
            "ENTITY_NOT_FOUND",
            "REVISION_MISMATCH",
            "COMPONENT_RATE_LIMITED",
            "INVALID_COMPONENT",
            "INVALID_VALUE",
            "NOT_FOUND",
            "MAILBOX_SATURATED",
            "MAILBOX_CLOSED",
            "PERSISTENCE_UNAVAILABLE",
            "CHECKPOINT_CAPACITY",
            "EXPLICIT_SPAWN_REQUIRED",
            "PERSISTENCE_BACKPRESSURE",
        ] {
            assert!(
                CommandDedupStore::is_stable_error_code(code),
                "missing {code}"
            );
        }
    }
}
