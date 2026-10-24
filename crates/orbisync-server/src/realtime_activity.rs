//! Bounded, ephemeral participant activity with ordered snapshot publication.
//!
//! Callbacks execute synchronously under the publication mutex. They may only
//! enqueue prepared frames; they must not await or re-enter this store.

use std::collections::HashMap;
use std::sync::Mutex;

use orbisync_domain::{InstanceId, PresenceId, UserId};
use orbisync_protocol::v1::{ActivityChanged, ActivityEntry, ActivitySnapshot, ActivityUpdate};
use prost::Message as _;

/// Maximum serialized application value size.
pub const ACTIVITY_VALUE_BYTES: usize = 4 * 1024;
/// Maximum supported snapshot chunk count.
pub const ACTIVITY_SNAPSHOT_CHUNKS: usize = 128;
/// Snapshot assembly limit, including all chunk envelopes.
pub const ACTIVITY_SNAPSHOT_BYTES: usize = 512 * 1024;
// Reserve version/id/chunk fields and complete envelope overhead at every
// chunk, even when splitting one entry per frame. IDs are canonical UUIDs.
const SNAPSHOT_OVERHEAD_RESERVE: usize = ACTIVITY_SNAPSHOT_CHUNKS * 256;
// Activity values must round-trip through every supported JSON/SDK client.
const MAX_SAFE_ACTIVITY_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Server-owned connection identity. Client payloads cannot construct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivitySender {
    /// Joined instance.
    pub instance_id: InstanceId,
    /// Presence bound to this socket.
    pub presence_id: PresenceId,
    /// Authenticated user.
    pub user_id: UserId,
    /// Connection ownership generation, fencing old socket guards.
    pub epoch: u64,
}

/// Process and instance storage budgets. Encoded bytes include entry metadata.
#[derive(Debug, Clone, Copy)]
pub struct ActivityLimits {
    /// Keys retained for one presence.
    pub keys_per_presence: usize,
    /// Entries retained for one instance.
    pub entries_per_instance: usize,
    /// Full snapshot wire budget for one instance.
    pub bytes_per_instance: usize,
    /// Entries retained process-wide.
    pub total_entries: usize,
    /// Encoded entry bytes retained process-wide.
    pub total_bytes: usize,
    /// Server connection bindings retained process-wide.
    pub total_bindings: usize,
}

impl Default for ActivityLimits {
    fn default() -> Self {
        Self {
            keys_per_presence: 16,
            entries_per_instance: 4096,
            bytes_per_instance: ACTIVITY_SNAPSHOT_BYTES,
            total_entries: 65_536,
            total_bytes: 8 * 1024 * 1024,
            total_bindings: 65_536,
        }
    }
}

/// Admission failure without application payloads or identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityError {
    /// Key, TTL or data shape is invalid.
    Invalid,
    /// The socket is no longer the registered presence owner.
    NotBound,
    /// A configured memory or entry budget is full.
    Capacity,
    /// The monotonic version space is exhausted.
    VersionExhausted,
    /// Snapshot framing cannot satisfy the negotiated wire budgets.
    SnapshotTooLarge,
    /// The recipient's bounded delivery queue is no longer usable.
    DeliveryUnavailable,
}

#[derive(Debug)]
struct StoredEntry {
    value: ActivityEntry,
    wire_bytes: usize,
}
#[derive(Debug, Default)]
struct InstanceActivities {
    bindings: HashMap<PresenceId, (UserId, u64)>,
    entries: HashMap<(PresenceId, String), StoredEntry>,
    bytes: usize,
}
#[derive(Debug, Default)]
struct State {
    instances: HashMap<InstanceId, InstanceActivities>,
    version: u64,
    entries: usize,
    bytes: usize,
    bindings: usize,
}

/// In-memory activity only. Nothing in this store is checkpointed or replayed.
#[derive(Debug)]
pub struct ActivityStore {
    limits: ActivityLimits,
    state: Mutex<State>,
}

impl Default for ActivityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityStore {
    /// Creates the production bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(ActivityLimits::default())
    }

    /// Creates independently bounded storage; zero limits fail closed.
    #[must_use]
    pub fn with_limits(limits: ActivityLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
        }
    }

    /// Registers a server-authenticated connection; a newer owner clears old
    /// transient state. Repeating the same binding is harmless.
    pub fn bind_presence(
        &self,
        sender: ActivitySender,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<(), ActivityError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state
            .instances
            .get(&sender.instance_id)
            .and_then(|i| i.bindings.get(&sender.presence_id))
            .copied();
        if let Some((user, epoch)) = previous {
            if user != sender.user_id || epoch > sender.epoch {
                return Err(ActivityError::NotBound);
            }
            if epoch == sender.epoch {
                return Ok(());
            }
            remove_entries(
                &mut state,
                sender.instance_id,
                |entry| entry.value.presence_id == sender.presence_id.to_string(),
                &mut publish,
            )?;
        } else if state.bindings >= self.limits.total_bindings {
            return Err(ActivityError::Capacity);
        }
        state
            .instances
            .entry(sender.instance_id)
            .or_default()
            .bindings
            .insert(sender.presence_id, (sender.user_id, sender.epoch));
        if previous.is_none() {
            state.bindings += 1;
        }
        Ok(())
    }

    /// Applies a temporary update or TTL-zero clear and publishes in version
    /// order. Rejected writes do not consume capacity or extend prior TTLs.
    pub fn update(
        &self,
        sender: ActivitySender,
        update: ActivityUpdate,
        now_ms: i64,
        publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<(), ActivityError> {
        self.update_checked(sender, update, now_ms, |_| Ok(()), publish)
    }

    /// Also preflights the complete candidate snapshot before admitting a
    /// write, allowing the transport to enforce its frame/chunk budget atomically.
    pub fn update_checked(
        &self,
        sender: ActivitySender,
        update: ActivityUpdate,
        now_ms: i64,
        validate_snapshot: impl FnOnce(ActivitySnapshot) -> Result<(), ActivityError>,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<(), ActivityError> {
        if !valid_activity_key(&update.key)
            || (update.ttl_ms != 0 && !(250..=30_000).contains(&update.ttl_ms))
        {
            return Err(ActivityError::Invalid);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        require_binding(&state, sender)?;
        remove_entries(
            &mut state,
            sender.instance_id,
            |entry| entry.value.expires_at_unix_ms <= now_ms,
            &mut publish,
        )?;
        let key = (sender.presence_id, update.key.clone());
        if update.ttl_ms == 0 {
            return remove_entries(
                &mut state,
                sender.instance_id,
                |entry| {
                    entry.value.presence_id == sender.presence_id.to_string()
                        && entry.value.key == update.key
                },
                &mut publish,
            )
            .map(|_| ());
        }
        let data = update.data.unwrap_or_default();
        validate_data(&data)?;
        let value = ActivityEntry {
            presence_id: sender.presence_id.to_string(),
            user_id: sender.user_id.to_string(),
            key: update.key,
            data: Some(data),
            expires_at_unix_ms: now_ms
                .checked_add(i64::from(update.ttl_ms))
                .ok_or(ActivityError::Invalid)?,
        };
        let wire_bytes = repeated_entry_bytes(&value);
        let instance = state
            .instances
            .get(&sender.instance_id)
            .ok_or(ActivityError::NotBound)?;
        let old_bytes = instance
            .entries
            .get(&key)
            .map_or(0, |entry| entry.wire_bytes);
        let is_new = !instance.entries.contains_key(&key);
        let per_presence = instance
            .entries
            .keys()
            .filter(|(id, _)| *id == sender.presence_id)
            .count();
        let instance_bytes = instance
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(wire_bytes);
        let total_bytes = state
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(wire_bytes);
        if (is_new
            && (per_presence >= self.limits.keys_per_presence
                || instance.entries.len() >= self.limits.entries_per_instance
                || state.entries >= self.limits.total_entries))
            || instance_bytes.saturating_add(SNAPSHOT_OVERHEAD_RESERVE)
                > self.limits.bytes_per_instance.min(ACTIVITY_SNAPSHOT_BYTES)
            || total_bytes > self.limits.total_bytes
        {
            return Err(ActivityError::Capacity);
        }
        let mut activities: Vec<_> = instance
            .entries
            .iter()
            .filter(|(stored_key, _)| **stored_key != key)
            .map(|(_, entry)| entry.value.clone())
            .collect();
        activities.push(value.clone());
        activities.sort_by(|a, b| (&a.presence_id, &a.key).cmp(&(&b.presence_id, &b.key)));
        validate_snapshot(ActivitySnapshot {
            activities,
            version: state
                .version
                .checked_add(1)
                .ok_or(ActivityError::VersionExhausted)?,
            ..Default::default()
        })?;
        let version = next_version(&mut state)?;
        let instance = state
            .instances
            .get_mut(&sender.instance_id)
            .ok_or(ActivityError::NotBound)?;
        instance.entries.insert(
            key,
            StoredEntry {
                value: value.clone(),
                wire_bytes,
            },
        );
        instance.bytes = instance_bytes;
        state.bytes = total_bytes;
        if is_new {
            state.entries += 1;
        }
        publish(
            sender.instance_id,
            ActivityChanged {
                activity: Some(value),
                removed: false,
                version,
            },
        );
        Ok(())
    }

    /// Clears only the matching connection generation. A superseded guard
    /// cannot clear the replacement socket's activity or remove its binding.
    pub fn remove_presence(
        &self,
        sender: ActivitySender,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<usize, ActivityError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if require_binding(&state, sender).is_err() {
            return Ok(0);
        }
        let removed = remove_entries(
            &mut state,
            sender.instance_id,
            |entry| entry.value.presence_id == sender.presence_id.to_string(),
            &mut publish,
        )?;
        if let Some(instance) = state.instances.get_mut(&sender.instance_id) {
            instance.bindings.remove(&sender.presence_id);
            release_excess_capacity(&mut instance.bindings);
        }
        state.bindings = state.bindings.saturating_sub(1);
        if state
            .instances
            .get(&sender.instance_id)
            .is_some_and(|i| i.bindings.is_empty())
        {
            state.instances.remove(&sender.instance_id);
        }
        Ok(removed)
    }

    /// Clears an earlier socket incarnation without registering a replacement.
    /// Used when the new connection does not negotiate activity support.
    pub fn retire_superseded_presence(
        &self,
        sender: ActivitySender,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<usize, ActivityError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state
            .instances
            .get(&sender.instance_id)
            .and_then(|instance| instance.bindings.get(&sender.presence_id))
            .copied();
        if !previous.is_some_and(|(user, epoch)| user == sender.user_id && epoch < sender.epoch) {
            return Ok(0);
        }
        let removed = remove_entries(
            &mut state,
            sender.instance_id,
            |entry| entry.value.presence_id == sender.presence_id.to_string(),
            &mut publish,
        )?;
        if let Some(instance) = state.instances.get_mut(&sender.instance_id) {
            instance.bindings.remove(&sender.presence_id);
            release_excess_capacity(&mut instance.bindings);
        }
        state.bindings = state.bindings.saturating_sub(1);
        if state
            .instances
            .get(&sender.instance_id)
            .is_some_and(|instance| instance.bindings.is_empty())
        {
            state.instances.remove(&sender.instance_id);
        }
        Ok(removed)
    }

    /// Removes expired entries across all bounded instances. Called by an
    /// owned maintenance loop; it never creates its own background task.
    pub fn sweep(
        &self,
        now_ms: i64,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
    ) -> Result<usize, ActivityError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let instances: Vec<_> = state.instances.keys().copied().collect();
        let mut removed = 0;
        for instance in instances {
            removed += remove_entries(
                &mut state,
                instance,
                |entry| entry.value.expires_at_unix_ms <= now_ms,
                &mut publish,
            )?;
        }
        Ok(removed)
    }

    /// Captures and enqueues a complete snapshot under the same publication
    /// lock as live changes. All chunks must be prepared before enqueueing.
    pub fn snapshot(
        &self,
        instance_id: InstanceId,
        now_ms: i64,
        mut publish: impl FnMut(InstanceId, ActivityChanged),
        send: impl FnOnce(ActivitySnapshot) -> Result<(), ActivityError>,
    ) -> Result<(), ActivityError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        remove_entries(
            &mut state,
            instance_id,
            |entry| entry.value.expires_at_unix_ms <= now_ms,
            &mut publish,
        )?;
        let mut activities: Vec<_> = state
            .instances
            .get(&instance_id)
            .map(|i| {
                i.entries
                    .values()
                    .map(|entry| entry.value.clone())
                    .collect()
            })
            .unwrap_or_default();
        activities.sort_by(|a, b| (&a.presence_id, &a.key).cmp(&(&b.presence_id, &b.key)));
        send(ActivitySnapshot {
            activities,
            version: state.version,
            ..Default::default()
        })
    }

    /// Reports count and exact encoded-entry bytes without exposing payloads.
    #[must_use]
    pub fn retained(&self) -> (usize, usize, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.entries, state.bytes, state.bindings)
    }
}

fn next_version(state: &mut State) -> Result<u64, ActivityError> {
    state.version = state
        .version
        .checked_add(1)
        .ok_or(ActivityError::VersionExhausted)?;
    Ok(state.version)
}
fn require_binding(state: &State, sender: ActivitySender) -> Result<(), ActivityError> {
    if state
        .instances
        .get(&sender.instance_id)
        .and_then(|i| i.bindings.get(&sender.presence_id))
        .copied()
        == Some((sender.user_id, sender.epoch))
    {
        Ok(())
    } else {
        Err(ActivityError::NotBound)
    }
}
fn remove_entries(
    state: &mut State,
    instance_id: InstanceId,
    mut predicate: impl FnMut(&StoredEntry) -> bool,
    publish: &mut impl FnMut(InstanceId, ActivityChanged),
) -> Result<usize, ActivityError> {
    let mut keys: Vec<_> = state
        .instances
        .get(&instance_id)
        .map(|i| {
            i.entries
                .iter()
                .filter_map(|(key, value)| predicate(value).then_some(key.clone()))
                .collect()
        })
        .unwrap_or_default();
    keys.sort_by(|a, b| (a.0.to_string(), &a.1).cmp(&(b.0.to_string(), &b.1)));
    state
        .version
        .checked_add(keys.len() as u64)
        .ok_or(ActivityError::VersionExhausted)?;
    let removed = keys.len();
    for key in keys {
        let instance = state
            .instances
            .get_mut(&instance_id)
            .ok_or(ActivityError::NotBound)?;
        if let Some(entry) = instance.entries.remove(&key) {
            instance.bytes = instance.bytes.saturating_sub(entry.wire_bytes);
            state.bytes = state.bytes.saturating_sub(entry.wire_bytes);
            state.entries = state.entries.saturating_sub(1);
            let version = next_version(state)?;
            let mut value = entry.value;
            value.data = None;
            publish(
                instance_id,
                ActivityChanged {
                    activity: Some(value),
                    removed: true,
                    version,
                },
            );
        }
    }
    if let Some(instance) = state.instances.get_mut(&instance_id) {
        release_excess_capacity(&mut instance.entries);
    }
    Ok(removed)
}

// Repeated fill/clear cycles across otherwise idle instances must not retain
// one maximum-sized allocation per instance after releasing wire capacity.
fn release_excess_capacity<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    if map.is_empty() {
        *map = HashMap::new();
    } else if map.capacity() > map.len().saturating_mul(2) {
        map.shrink_to(map.len());
    }
}

/// App namespaces are dotted ASCII names; reserved engine roots are forbidden.
#[must_use]
pub fn valid_activity_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key.contains('.')
        && !matches!(
            key.split('.').next(),
            Some("core" | "orbisync" | "system" | "admin")
        )
        && key.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')
                })
        })
}
fn repeated_entry_bytes(entry: &ActivityEntry) -> usize {
    let len = entry.encoded_len();
    let mut n = len;
    let mut delimiter = 1;
    while n >= 128 {
        delimiter += 1;
        n >>= 7;
    }
    1 + delimiter + len
}
fn validate_data(data: &prost_types::Struct) -> Result<(), ActivityError> {
    let mut pending: Vec<_> = data.fields.values().map(|value| (value, 1usize)).collect();
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > 16 || nodes > 1024 {
            return Err(ActivityError::Invalid);
        }
        match value.kind.as_ref() {
            Some(prost_types::value::Kind::StructValue(value)) => {
                pending.extend(value.fields.values().map(|value| (value, depth + 1)))
            }
            Some(prost_types::value::Kind::ListValue(value)) => {
                pending.extend(value.values.iter().map(|value| (value, depth + 1)))
            }
            Some(prost_types::value::Kind::NumberValue(value))
                if !value.is_finite()
                    || (value.fract() == 0.0 && value.abs() > MAX_SAFE_ACTIVITY_INTEGER) =>
            {
                return Err(ActivityError::Invalid);
            }
            Some(prost_types::value::Kind::NullValue(value)) if *value != 0 => {
                return Err(ActivityError::Invalid);
            }
            None => return Err(ActivityError::Invalid),
            _ => {}
        }
    }
    if data.encoded_len() > ACTIVITY_VALUE_BYTES {
        return Err(ActivityError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    fn sender() -> ActivitySender {
        ActivitySender {
            instance_id: InstanceId::generate(),
            presence_id: PresenceId::generate(),
            user_id: UserId::generate(),
            epoch: 0,
        }
    }
    fn update(key: &str, ttl_ms: u32) -> ActivityUpdate {
        ActivityUpdate {
            key: key.into(),
            data: Some(prost_types::Struct::default()),
            ttl_ms,
        }
    }
    fn snapshot(store: &ActivityStore, instance: InstanceId, now: i64) -> ActivitySnapshot {
        let mut result = None;
        store
            .snapshot(
                instance,
                now,
                |_, _| {},
                |value| {
                    result = Some(value);
                    Ok(())
                },
            )
            .unwrap();
        result.unwrap()
    }
    #[test]
    fn activity_ttl_clear_and_expiry_are_ordered_and_never_replayed_in_snapshot() {
        let store = ActivityStore::new();
        let owner = sender();
        let mut changes = Vec::new();
        store.bind_presence(owner, |_, _| {}).unwrap();
        store
            .update(owner, update("example.typing", 500), 1000, |_, v| {
                changes.push(v)
            })
            .unwrap();
        store
            .update(owner, update("example.typing", 1000), 1200, |_, v| {
                changes.push(v)
            })
            .unwrap();
        assert_eq!(
            store.sweep(1500, |_, v| changes.push(v)).unwrap(),
            0,
            "renewal survives the original deadline"
        );
        assert_eq!(
            snapshot(&store, owner.instance_id, 2199).activities.len(),
            1
        );
        assert_eq!(store.sweep(2200, |_, v| changes.push(v)).unwrap(), 1);
        assert!(changes.last().unwrap().removed);
        assert!(
            changes
                .last()
                .unwrap()
                .activity
                .as_ref()
                .unwrap()
                .data
                .is_none()
        );
        assert!(
            snapshot(&store, owner.instance_id, 2200)
                .activities
                .is_empty()
        );
        store
            .update(owner, update("example.typing", 500), 2300, |_, v| {
                changes.push(v)
            })
            .unwrap();
        store
            .update(owner, update("example.typing", 0), 2301, |_, v| {
                changes.push(v)
            })
            .unwrap();
        assert!(
            snapshot(&store, owner.instance_id, 2301)
                .activities
                .is_empty()
        );
        assert!(changes.windows(2).all(|v| v[0].version < v[1].version));
        assert_eq!(store.retained().0, 0);
    }
    #[test]
    fn activity_old_owner_cannot_reinsert_or_clear_new_connections_activity() {
        let store = ActivityStore::new();
        let old = sender();
        let new = ActivitySender { epoch: 1, ..old };
        store.bind_presence(old, |_, _| {}).unwrap();
        store
            .update(old, update("example.editing", 5000), 1000, |_, _| {})
            .unwrap();
        let mut removed = 0;
        store
            .bind_presence(new, |_, change| removed += usize::from(change.removed))
            .unwrap();
        assert_eq!(
            removed, 1,
            "resume clears transient state belonging to the previous socket"
        );
        assert_eq!(
            store.bind_presence(old, |_, _| {}),
            Err(ActivityError::NotBound)
        );
        assert_eq!(
            store.update(old, update("example.editing", 5000), 1001, |_, _| {}),
            Err(ActivityError::NotBound)
        );
        store
            .update(new, update("example.editing", 5000), 1001, |_, _| {})
            .unwrap();
        assert_eq!(store.remove_presence(old, |_, _| {}).unwrap(), 0);
        assert_eq!(snapshot(&store, old.instance_id, 1001).activities.len(), 1);
        assert_eq!(store.remove_presence(new, |_, _| {}).unwrap(), 1);
        assert_eq!(
            store.update(new, update("example.editing", 5000), 1002, |_, _| {}),
            Err(ActivityError::NotBound)
        );
        assert_eq!(store.retained(), (0, 0, 0));
    }
    #[test]
    fn activity_caps_do_not_block_clear_or_affect_other_instances_and_account_for_metadata() {
        let store = ActivityStore::with_limits(ActivityLimits {
            keys_per_presence: 1,
            entries_per_instance: 1,
            total_entries: 2,
            ..Default::default()
        });
        let a = sender();
        let b = sender();
        store.bind_presence(a, |_, _| {}).unwrap();
        store.bind_presence(b, |_, _| {}).unwrap();
        store
            .update(a, update("example.a", 5000), 1000, |_, _| {})
            .unwrap();
        assert_eq!(
            store.update(a, update("example.b", 5000), 1000, |_, _| {}),
            Err(ActivityError::Capacity)
        );
        store
            .update(b, update("example.a", 5000), 1000, |_, _| {})
            .unwrap();
        let entries = snapshot(&store, a.instance_id, 1000).activities;
        assert!(
            store.retained().1 > entries[0].data.as_ref().unwrap().encoded_len(),
            "ID/key/deadline overhead counts against the wire budget"
        );
        store
            .update(a, update("example.a", 0), 1001, |_, _| {})
            .unwrap();
        store
            .update(a, update("example.b", 5000), 1001, |_, _| {})
            .unwrap();
        assert_eq!(
            snapshot(&store, b.instance_id, 1001).activities[0].key,
            "example.a"
        );
    }
    #[test]
    fn activity_validation_and_transport_preflight_cannot_partially_replace_state() {
        let store = ActivityStore::new();
        let owner = sender();
        store.bind_presence(owner, |_, _| {}).unwrap();
        for key in [
            "typing",
            "core.typing",
            "orbisync.typing",
            "admin.typing",
            "system.typing",
            "example..typing",
            "Example.typing",
        ] {
            assert_eq!(
                store.update(owner, update(key, 5000), 1000, |_, _| {}),
                Err(ActivityError::Invalid)
            );
        }
        for ttl in [1, 249, 30_001, u32::MAX] {
            assert_eq!(
                store.update(owner, update("example.typing", ttl), 1000, |_, _| {}),
                Err(ActivityError::Invalid)
            );
        }
        store
            .update(owner, update("example.typing", 5000), 1000, |_, _| {})
            .unwrap();
        let before = snapshot(&store, owner.instance_id, 1000);
        assert_eq!(
            store.update_checked(
                owner,
                update("example.typing", 30_000),
                1001,
                |_| Err(ActivityError::SnapshotTooLarge),
                |_, _| panic!("rejected candidate cannot publish")
            ),
            Err(ActivityError::SnapshotTooLarge)
        );
        assert_eq!(snapshot(&store, owner.instance_id, 1001), before);
        let mut invalid = update("example.typing", 5000);
        invalid.data.as_mut().unwrap().fields.insert(
            "number".into(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(f64::NAN)),
            },
        );
        assert_eq!(
            store.update(owner, invalid, 1001, |_, _| {}),
            Err(ActivityError::Invalid)
        );
        assert_eq!(snapshot(&store, owner.instance_id, 1001), before);
    }
    #[test]
    fn activity_byte_budgets_include_metadata_and_replacement_reclaims_exact_capacity() {
        let owner = sender();
        let small = ActivityEntry {
            presence_id: owner.presence_id.to_string(),
            user_id: owner.user_id.to_string(),
            key: "example.typing".into(),
            data: Some(prost_types::Struct::default()),
            expires_at_unix_ms: 6000,
        };
        let entry_bytes = repeated_entry_bytes(&small);
        for limits in [
            ActivityLimits {
                total_bytes: entry_bytes,
                ..Default::default()
            },
            ActivityLimits {
                bytes_per_instance: SNAPSHOT_OVERHEAD_RESERVE + entry_bytes,
                ..Default::default()
            },
        ] {
            let store = ActivityStore::with_limits(limits);
            store.bind_presence(owner, |_, _| {}).unwrap();
            store
                .update(owner, update("example.typing", 5000), 1000, |_, _| {})
                .unwrap();
            assert_eq!(store.retained().1, entry_bytes);
            let before = snapshot(&store, owner.instance_id, 1000);
            let mut large = update("example.typing", 5000);
            large.data.as_mut().unwrap().fields.insert(
                "value".into(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("larger".into())),
                },
            );
            assert_eq!(
                store.update(owner, large, 1000, |_, _| panic!(
                    "rejected replacement cannot publish"
                )),
                Err(ActivityError::Capacity)
            );
            assert_eq!(snapshot(&store, owner.instance_id, 1000), before);
            store
                .update(owner, update("example.typing", 5000), 1000, |_, _| {})
                .unwrap();
            assert_eq!(
                store.retained(),
                (1, entry_bytes, 1),
                "replacement must subtract its previous encoded size"
            );
            store.remove_presence(owner, |_, _| {}).unwrap();
            assert_eq!(store.retained(), (0, 0, 0));
        }
    }

    #[test]
    fn activity_global_entry_and_binding_limits_are_reusable_after_disconnect() {
        let store = ActivityStore::with_limits(ActivityLimits {
            total_entries: 1,
            total_bindings: 2,
            ..Default::default()
        });
        let a = sender();
        let b = sender();
        let c = sender();
        store.bind_presence(a, |_, _| {}).unwrap();
        store.bind_presence(b, |_, _| {}).unwrap();
        assert_eq!(
            store.bind_presence(c, |_, _| {}),
            Err(ActivityError::Capacity)
        );
        store
            .update(a, update("example.typing", 5000), 1000, |_, _| {})
            .unwrap();
        assert_eq!(
            store.update(b, update("example.typing", 5000), 1000, |_, _| {}),
            Err(ActivityError::Capacity)
        );
        store.remove_presence(a, |_, _| {}).unwrap();
        store.bind_presence(c, |_, _| {}).unwrap();
        store
            .update(b, update("example.typing", 5000), 1000, |_, _| {})
            .unwrap();
        assert_eq!(store.retained().0, 1);
        assert_eq!(store.retained().2, 2);
    }

    #[test]
    fn activity_featureless_resume_retires_only_earlier_owner_without_admission() {
        let store = ActivityStore::with_limits(ActivityLimits {
            total_bindings: 1,
            ..Default::default()
        });
        let old = sender();
        let resumed = ActivitySender { epoch: 1, ..old };
        store.bind_presence(old, |_, _| {}).unwrap();
        store
            .update(old, update("example.typing", 5000), 1000, |_, _| {})
            .unwrap();
        assert_eq!(
            store
                .retire_superseded_presence(old, |_, _| panic!("same epoch cannot retire itself"))
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .retire_superseded_presence(
                    ActivitySender {
                        user_id: UserId::generate(),
                        ..resumed
                    },
                    |_, _| panic!("another user cannot retire the owner")
                )
                .unwrap(),
            0
        );
        let mut changes = Vec::new();
        assert_eq!(
            store
                .retire_superseded_presence(resumed, |_, change| changes.push(change))
                .unwrap(),
            1
        );
        assert_eq!(changes.len(), 1);
        assert!(changes[0].removed);
        assert_eq!(store.retained(), (0, 0, 0));
        let other = sender();
        store.bind_presence(other, |_, _| {}).unwrap();
        assert_eq!(
            store.update(resumed, update("example.typing", 5000), 1001, |_, _| {}),
            Err(ActivityError::NotBound),
            "featureless retirement never admits a new activity binding"
        );
        assert_eq!(
            store
                .retire_superseded_presence(ActivitySender { epoch: 0, ..other }, |_, _| panic!(
                    "old guard cannot retire current owner"
                ))
                .unwrap(),
            0
        );
        assert_eq!(store.retained().2, 1);
    }

    #[test]
    fn activity_rejects_oversized_deep_and_malformed_values_without_retention() {
        let store = ActivityStore::new();
        let owner = sender();
        store.bind_presence(owner, |_, _| {}).unwrap();
        let mut deep = prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        };
        for _ in 0..17 {
            deep = prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue { values: vec![deep] },
                )),
            };
        }
        for value in [
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "x".repeat(ACTIVITY_VALUE_BYTES),
                )),
            },
            prost_types::Value { kind: None },
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NullValue(1)),
            },
            deep,
        ] {
            let mut request = update("example.typing", 5000);
            request
                .data
                .as_mut()
                .unwrap()
                .fields
                .insert("value".into(), value);
            assert_eq!(
                store.update(owner, request, 1000, |_, _| panic!(
                    "invalid data cannot publish"
                )),
                Err(ActivityError::Invalid)
            );
        }
        assert_eq!(store.retained(), (0, 0, 1));
        for number in [
            MAX_SAFE_ACTIVITY_INTEGER,
            -MAX_SAFE_ACTIVITY_INTEGER,
            1.5,
            -1.5,
        ] {
            let value = prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(number)),
            };
            let mut request = update("example.number", 5000);
            request
                .data
                .as_mut()
                .unwrap()
                .fields
                .insert("value".into(), value.clone());
            store.update(owner, request, 1000, |_, _| {}).unwrap();
            assert_eq!(
                snapshot(&store, owner.instance_id, 1000).activities[0]
                    .data
                    .as_ref()
                    .unwrap()
                    .fields["value"],
                value
            );
        }
        let before = snapshot(&store, owner.instance_id, 1000);
        for number in [
            9_007_199_254_740_992.0,
            -9_007_199_254_740_992.0,
            1e20,
            -1e20,
        ] {
            let mut request = update("example.number", 5000);
            request.data.as_mut().unwrap().fields.insert(
                "value".into(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::NumberValue(number)),
                },
            );
            assert_eq!(
                store.update(owner, request, 1000, |_, _| panic!(
                    "unsafe integer cannot publish"
                )),
                Err(ActivityError::Invalid)
            );
            assert_eq!(
                snapshot(&store, owner.instance_id, 1000),
                before,
                "unsafe numeric input cannot poison an existing baseline"
            );
        }
    }

    #[test]
    fn activity_clear_releases_high_water_allocations_while_an_instance_stays_connected() {
        let store = ActivityStore::new();
        let owner = sender();
        store.bind_presence(owner, |_, _| {}).unwrap();
        for index in 0..16 {
            store
                .update(
                    owner,
                    update(&format!("example.key_{index}"), 5000),
                    1000,
                    |_, _| {},
                )
                .unwrap();
        }
        for index in 0..16 {
            store
                .update(
                    owner,
                    update(&format!("example.key_{index}"), 0),
                    1000,
                    |_, _| {},
                )
                .unwrap();
        }
        let others: Vec<_> = (0..32)
            .map(|_| ActivitySender {
                presence_id: PresenceId::generate(),
                ..owner
            })
            .collect();
        for other in &others {
            store.bind_presence(*other, |_, _| {}).unwrap();
        }
        for other in others {
            store.remove_presence(other, |_, _| {}).unwrap();
        }
        let state = store.state.lock().unwrap();
        let instance = state.instances.get(&owner.instance_id).unwrap();
        assert_eq!(
            instance.entries.capacity(),
            0,
            "empty entry maps cannot retain past full-instance allocations"
        );
        assert_eq!(instance.bindings.len(), 1);
        assert!(
            instance.bindings.capacity() <= 8,
            "idle binding maps must release their previous connection high water mark"
        );
        assert_eq!((state.entries, state.bytes, state.bindings), (0, 0, 1));
    }

    #[test]
    fn activity_snapshots_and_concurrent_publications_share_one_monotonic_order() {
        use std::sync::Arc;
        let store = Arc::new(ActivityStore::new());
        let owner = sender();
        store.bind_presence(owner, |_, _| {}).unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let observed = Arc::clone(&observed);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        store
                            .update(owner, update("example.typing", 5000), 1000, |_, change| {
                                observed.lock().unwrap().push(change.version)
                            })
                            .unwrap();
                        store
                            .snapshot(
                                owner.instance_id,
                                1000,
                                |_, _| {},
                                |snapshot| {
                                    observed.lock().unwrap().push(snapshot.version);
                                    Ok(())
                                },
                            )
                            .unwrap();
                    }
                })
            })
            .collect();
        for job in jobs {
            job.join().unwrap();
        }
        let versions = observed.lock().unwrap();
        assert!(
            versions.windows(2).all(|v| v[0] <= v[1]),
            "snapshot enqueue cannot overtake a newer publication"
        );
        assert_eq!(store.retained().0, 1);
    }
}
