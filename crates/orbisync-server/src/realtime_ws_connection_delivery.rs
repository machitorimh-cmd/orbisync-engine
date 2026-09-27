use crate::delivery::{DeliverySink, Reliability, SinkOutcome};
use orbisync_realtime::outbound_queue::{QueueError, RevisionedEnqueueResult};
use tokio::sync::Notify;

const STATE_SYNC_FEATURE: &str = "orbisync.state-sync.v1";
const SNAPSHOT_PENDING_BYTES: usize = 16 * 1024 * 1024;

fn negotiate_state_sync(
    request: &orbisync_protocol::v1::ClientHello,
    response: &mut orbisync_protocol::v1::ServerHello,
) {
    if request.supported_features.iter().any(|feature| feature == STATE_SYNC_FEATURE) {
        response.enabled_features.push(STATE_SYNC_FEATURE.to_owned());
    }
}

/// Split repeated entities and optional fields before latest coalescing. A
/// single maximum revision for a whole batched Delta would discard other keys.
fn enqueue_committed_delta(
    queue: &mut OutboundQueue,
    mut envelope: Envelope,
    max_bytes: usize,
) -> Result<usize, QueueError> {
    let Some(envelope::Payload::StateDelta(delta)) = envelope.payload.take() else { return Ok(0); };
    if delta.from_revision > delta.to_revision { return Err(QueueError::RevisionConflict); }
    let mut delivered = 0;
    for mut entity in delta.entities {
        let base = orbisync_protocol::v1::EntityState {
            entity_id: entity.entity_id.clone(), revision: entity.revision, ..Default::default()
        };
        let mut fields = Vec::new();
        macro_rules! field {
            ($name:ident) => {
                if let Some(value) = entity.$name.take() {
                    let mut state = base.clone(); state.$name = Some(value);
                    fields.push((stringify!($name), state));
                }
            };
        }
        field!(transform); field!(velocity); field!(animation); field!(presence); field!(properties);
        if fields.is_empty() { fields.push(("revision", base)); }
        for (field, state) in fields {
            let key = format!("{}:{field}", state.entity_id);
            let canonical = state.encode_to_vec();
            let mut fragment = envelope.clone();
            fragment.message_id = uuid::Uuid::now_v7().to_string();
            fragment.payload = Some(envelope::Payload::StateDelta(orbisync_protocol::v1::StateDelta {
                from_revision: delta.from_revision, to_revision: delta.to_revision, entities: vec![state],
            }));
            match queue.push_latest_revisioned(key, delta.to_revision, canonical, fragment.encode_to_vec(), max_bytes)? {
                RevisionedEnqueueResult::Inserted | RevisionedEnqueueResult::Replaced => delivered += 1,
                RevisionedEnqueueResult::Stale | RevisionedEnqueueResult::Duplicate => {},
            }
        }
    }
    Ok(delivered)
}

struct DeliveryFilterState {
    viewer_pos: Vec3,
    user: UserId,
    viewer_roles: Option<Arc<std::collections::BTreeSet<orbisync_domain::RoleId>>>,
    subscribed: std::collections::HashMap<EntityId, bool>,
    snapshot_pending: bool,
    snapshot_revision: Option<u64>,
    raw_snapshot_messages: std::collections::HashMap<String, Vec<orbisync_world_runtime::actor::EntityInterestView>>,
    membership: std::sync::Weak<()>,
}

/// Connection-owned delivery handoff.
///
/// The registry calls this sink directly. Live updates are interest-filtered
/// before enqueue. Snapshot handoff retains raw frames in the same bounded
/// queue until its subscription baseline is available, then filters before
/// writing. There is no additional per-connection mpsc queue.
struct RealtimeDeliverySink {
    state: Arc<RealtimeState>,
    instance_id: InstanceId,
    queue: Arc<Mutex<OutboundQueue>>,
    filter: Mutex<DeliveryFilterState>,
    notify: Arc<Notify>,
    closed: AtomicBool,
}

impl RealtimeDeliverySink {
    fn new(
        state: Arc<RealtimeState>,
        instance_id: InstanceId,
        viewer_pos: Vec3,
        user: UserId,
        viewer_roles: Option<Arc<std::collections::BTreeSet<orbisync_domain::RoleId>>>,
        subscribed: std::collections::HashMap<EntityId, bool>,
    ) -> Self {
        Self {
            queue: Arc::new(Mutex::new(OutboundQueue::from_config_with_metrics(
                &state.config,
                Arc::clone(&state.metrics_recorder),
            ))),
            state,
            instance_id,
            filter: Mutex::new(DeliveryFilterState {
                viewer_pos,
                user,
                viewer_roles,
                subscribed,
                snapshot_pending: false,
                snapshot_revision: None,
                raw_snapshot_messages: std::collections::HashMap::new(),
                membership: std::sync::Weak::new(),
            }),
            notify: Arc::new(Notify::new()),
            closed: AtomicBool::new(false),
        }
    }

    fn queue(&self) -> Arc<Mutex<OutboundQueue>> {
        Arc::clone(&self.queue)
    }

    /// Register this sink before reading a snapshot. During handoff the single
    /// bounded queue retains raw messages reliably; interest is evaluated after
    /// installing the snapshot's subscription baseline, never with an empty one.
    fn begin_snapshot(&self) {
        self.filter.lock().unwrap_or_else(|e| e.into_inner()).snapshot_pending = true;
    }

    fn install_snapshot(&self, revision: u64, subscribed: std::collections::HashMap<EntityId, bool>) {
        let mut filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        filter.snapshot_revision = Some(revision);
        filter.subscribed = subscribed;
    }

    fn finish_snapshot(&self) {
        let mut filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        // Keep handoff messages FIFO until all raw entries are filtered. New
        // broadcasts cannot race this switch because enqueue takes filter first.
        if self.queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            filter.snapshot_pending = false;
        }
    }

    /// Called before writing queued data. The boundary also discards late
    /// broadcasts committed before the snapshot but published after it.
    fn prepare_delivery(&self, bytes: &[u8]) -> Option<Envelope> {
        if self.closed.load(Ordering::Acquire) { return None; }
        let env = decode_envelope(bytes).ok()?;
        let mut filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        let raw = filter.raw_snapshot_messages.remove(&env.message_id);
        let revision = match env.payload.as_ref() {
            Some(envelope::Payload::StateDelta(delta)) => Some(delta.to_revision),
            Some(envelope::Payload::EntityCommand(command)) => command.instance_revision,
            _ => None,
        };
        if revision.zip(filter.snapshot_revision).is_some_and(|(revision, boundary)| revision <= boundary) {
            return None;
        }
        let Some(views) = raw else { return Some(env); };
        let viewer_pos = filter.viewer_pos;
        let user = filter.user;
        let roles = filter.viewer_roles.clone();
        let filtered = filter_payload_for_viewer_with_roles(
            bytes, Some(viewer_pos), Some(user), &self.state.interest_grid,
            &views, roles.as_deref(), &mut filter.subscribed,
        )?;
        decode_envelope(&filtered).ok()
    }

    /// Freeze publication authorization; do not consult a later incarnation.
    /// Resolve static policies now to bound metadata independently of role/user set sizes.
    fn publication_views(&self, env: &Envelope, views: &orbisync_world_runtime::InterestSnapshot, filter: &DeliveryFilterState)
        -> Option<Vec<orbisync_world_runtime::actor::EntityInterestView>> {
        let ids: Vec<&str> = match env.payload.as_ref() {
            Some(envelope::Payload::StateDelta(delta)) => delta.entities.iter().map(|e| e.entity_id.as_str()).collect(),
            Some(envelope::Payload::EntityCommand(command)) => vec![command.entity_id.as_str()],
            _ => return Some(Vec::new()),
        };
        let mut retained = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.parse::<EntityId>().ok()?;
            let view = views.get(id)?;
            let mut view = view.clone();
            if !matches!(view.visibility, VisibilityPolicy::Spatial { .. }) {
                let allowed = !crate::interest_filter::filter_visible_views_with_roles(
                    Some(filter.viewer_pos), std::slice::from_ref(&view), &self.state.interest_grid,
                    Some(filter.user), filter.viewer_roles.as_deref(), None,
                ).is_empty();
                view.visibility = if allowed { VisibilityPolicy::Global } else { VisibilityPolicy::OwnerOnly };
                if !allowed { view.owner = None; }
            }
            retained.push(view);
        }
        Some(retained)
    }

    fn close_for_missing_visibility(&self) {
        self.closed.store(true, Ordering::Release);
        tracing::warn!(event = "realtime.sync_visibility_unavailable", instance_id = %self.instance_id,
            "publication visibility unavailable; disconnecting for snapshot recovery");
        self.notify.notify_one();
    }

    fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    fn set_viewer_position(&self, viewer_pos: Vec3) {
        let mut filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        filter.viewer_pos = viewer_pos;
    }

    fn close_for_overflow(&self, error: QueueError) {
        self.closed.store(true, Ordering::Release);
        tracing::warn!(
            event = "realtime.reliable_queue_overflow",
            instance_id = %self.instance_id,
            error = %error,
            "reliable queue overflow — disconnecting per D-21 (will be revisited after W-18 Resume)"
        );
        self.notify.notify_one();
    }

    fn queue_payload(&self, filtered: Vec<u8>) -> SinkOutcome {
        let decoded = match decode_envelope(&filtered) {
            Ok(envelope) => envelope,
            Err(_) => {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                return match queue.push_control(filtered) {
                    Ok(()) => {
                        self.notify.notify_one();
                        SinkOutcome { delivered: 1, ..SinkOutcome::default() }
                    }
                    Err(error) => {
                        drop(queue);
                        self.close_for_overflow(error);
                        SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() }
                    }
                };
            }
        };
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(decoded.payload, Some(envelope::Payload::StateDelta(_)))
            && filtered.len() > SNAPSHOT_PENDING_BYTES.saturating_sub(queue.depth().bytes) {
            drop(queue);
            self.close_for_overflow(QueueError::ReliableOverflow);
            return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
        }
        let result = match &decoded.payload {
            Some(envelope::Payload::StateDelta(_)) => {
                match enqueue_committed_delta(&mut queue, decoded, SNAPSHOT_PENDING_BYTES) {
                    Ok(delivered) => SinkOutcome { delivered, ..SinkOutcome::default() },
                    Err(error) => {
                        drop(queue);
                        self.close_for_overflow(error);
                        return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
                    }
                }
            }
            Some(envelope::Payload::EntityCommand(_))
            | Some(envelope::Payload::DomainEvent(_)) => match queue.push_reliable(filtered) {
                Ok(()) => SinkOutcome { delivered: 1, ..SinkOutcome::default() },
                Err(error) => {
                    drop(queue);
                    self.close_for_overflow(error);
                    return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
                }
            },
            Some(envelope::Payload::Heartbeat(_))
            | Some(envelope::Payload::HeartbeatAck(_))
            | Some(envelope::Payload::Error(_)) => match queue.push_control(filtered) {
                Ok(()) => SinkOutcome { delivered: 1, ..SinkOutcome::default() },
                Err(error) => {
                    drop(queue);
                    self.close_for_overflow(error);
                    return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
                }
            },
            _ => match queue.push_reliable(filtered) {
                Ok(()) => SinkOutcome { delivered: 1, ..SinkOutcome::default() },
                Err(error) => {
                    drop(queue);
                    self.close_for_overflow(error);
                    return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
                }
            },
        };
        drop(queue);
        if result.delivered > 0 {
            self.notify.notify_one();
        }
        result
    }
}

impl DeliverySink for RealtimeDeliverySink {
    fn viewer_cell(&self) -> Option<orbisync_interest::CellCoord> {
        let filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        Some(self.state.interest_grid.cell_for(filter.viewer_pos))
    }

    fn enqueue(
        &self,
        payload: &[u8],
        _reliability: Reliability,
        views: Option<&orbisync_world_runtime::InterestSnapshot>,
    ) -> SinkOutcome {
        if self.closed.load(Ordering::Acquire) {
            return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
        }
        let mut filter = self.filter.lock().unwrap_or_else(|e| e.into_inner());
        let owned_views;
        let views = if let Some(views) = views {
            views
        } else {
            owned_views = self
                .state
                .registry
                .interest_views(self.instance_id)
                .unwrap_or_default();
            &owned_views
        };
        if filter.snapshot_pending {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            if payload.len() > SNAPSHOT_PENDING_BYTES.saturating_sub(queue.depth().bytes) {
                drop(queue);
                drop(filter);
                self.close_for_overflow(QueueError::ReliableOverflow);
                return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
            }
            let Ok(env) = decode_envelope(payload) else {
                drop(queue);
                drop(filter);
                self.close_for_overflow(QueueError::ReliableOverflow);
                return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
            };
            let Some(retained) = self.publication_views(&env, views, &filter) else {
                drop(queue); drop(filter);
                self.close_for_missing_visibility();
                return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
            };
            let metadata_bytes = filter.raw_snapshot_messages.iter().map(|(key, value)|
                key.len() + value.len() * size_of::<orbisync_world_runtime::actor::EntityInterestView>()).sum::<usize>();
            let additional = env.message_id.len() + retained.len() * size_of::<orbisync_world_runtime::actor::EntityInterestView>();
            if filter.raw_snapshot_messages.contains_key(&env.message_id)
                || payload.len().saturating_add(additional).saturating_add(metadata_bytes) > SNAPSHOT_PENDING_BYTES.saturating_sub(queue.depth().bytes) {
                drop(queue); drop(filter);
                self.close_for_overflow(QueueError::ReliableOverflow);
                return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
            }
            match queue.push_reliable(payload.to_vec()) {
                Ok(()) => {
                    filter.raw_snapshot_messages.insert(env.message_id, retained);
                    self.notify.notify_one();
                    return SinkOutcome { delivered: 1, ..SinkOutcome::default() };
                }
                Err(error) => {
                    drop(queue);
                    drop(filter);
                    self.close_for_overflow(error);
                    return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
                }
            }
        }
        let decoded = decode_envelope(payload).ok();
        if let (Some(floor), Some(env)) = (filter.snapshot_revision, decoded.as_ref()) {
            let boundary = match env.payload.as_ref() {
                Some(envelope::Payload::StateDelta(delta)) => Some(delta.to_revision),
                Some(envelope::Payload::EntityCommand(command)) => command.instance_revision,
                _ => None,
            };
            if boundary.is_some_and(|revision| revision <= floor) {
                return SinkOutcome { filtered: 1, ..SinkOutcome::default() };
            }
        }
        let valid = decoded.as_ref().and_then(|env| self.publication_views(env, views, &filter));
        if valid.is_none() {
            drop(filter);
            self.close_for_missing_visibility();
            return SinkOutcome { disconnected_slow_consumer: 1, ..SinkOutcome::default() };
        }
        let viewer_pos = filter.viewer_pos;
        let user = filter.user;
        let viewer_roles = filter.viewer_roles.clone();
        let filtered = filter_payload_with_index(
            payload,
            Some(viewer_pos),
            Some(user),
            &self.state.interest_grid,
            views,
            viewer_roles.as_deref(),
            &mut filter.subscribed,
        );
        let membership = views.membership_token();
        if !std::sync::Weak::ptr_eq(&filter.membership, &membership) {
            filter.subscribed.retain(|id, _| views.contains(*id));
            filter.membership = membership;
        }
        let Some(filtered) = filtered else {
            return SinkOutcome { filtered: 1, ..SinkOutcome::default() };
        };
        drop(filter);
        self.queue_payload(filtered)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}
