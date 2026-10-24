// Transient, application-defined activity. This file is included in realtime_ws.
use crate::realtime_activity::{
    ACTIVITY_SNAPSHOT_BYTES, ACTIVITY_SNAPSHOT_CHUNKS, ActivityError, ActivitySender,
};

fn activity_error(error: ActivityError) -> (&'static str, String) {
    let code = match error {
        ActivityError::Invalid => "INVALID_ARGUMENT",
        ActivityError::NotBound => "NOT_AUTHORIZED",
        ActivityError::Capacity => "RATE_LIMITED",
        ActivityError::SnapshotTooLarge => "MESSAGE_TOO_LARGE",
        ActivityError::VersionExhausted | ActivityError::DeliveryUnavailable => "SERVER_BUSY",
    };
    (code, "activity could not be applied".to_owned())
}

fn activity_envelope(instance: InstanceId, payload: envelope::Payload) -> Envelope {
    Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: orbisync_protocol::SUPPORTED_MINOR_MAX,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: u64::MAX,
        sent_at_unix_ms: i64::MAX,
        instance_id: instance.to_string(),
        payload: Some(payload),
    }
}

fn broadcast_activity_change(
    state: &RealtimeState,
    instance: InstanceId,
    change: orbisync_protocol::v1::ActivityChanged,
) {
    let envelope = activity_envelope(instance, envelope::Payload::ActivityChanged(change));
    let bytes = envelope.encode_to_vec();
    if bytes.len() as u64 > state.config.max_normal_message_bytes {
        tracing::error!(
            event = "realtime.activity_frame_invariant",
            "admitted activity exceeds the configured frame limit"
        );
        return;
    }
    // FIFO delivery is ephemeral: it is never RetainReliable, checkpointed or
    // inserted into the instance's revision/replay history.
    let _outcome = state
        .delivery
        .broadcast(instance, bytes, Reliability::Reliable);
}

fn bind_activity_presence(
    state: &RealtimeState,
    sender: &ActivitySender,
) -> Result<(), (&'static str, String)> {
    if !state
        .resume_store
        .is_current(sender.presence_id, sender.epoch)
    {
        return Err(activity_error(ActivityError::NotBound));
    }
    state
        .activities
        .bind_presence(*sender, |instance, change| {
            broadcast_activity_change(state, instance, change)
        })
        .map_err(activity_error)
}

fn clear_activity_presence(state: &RealtimeState, sender: ActivitySender) {
    if let Err(error) = state
        .activities
        .remove_presence(sender, |instance, change| {
            broadcast_activity_change(state, instance, change)
        })
    {
        tracing::warn!(event="realtime.activity_cleanup_failed",error=?error,"activity cleanup failed closed");
    }
}

fn clear_superseded_activity_presence(state: &RealtimeState, sender: ActivitySender) {
    if !state
        .resume_store
        .is_current(sender.presence_id, sender.epoch)
    {
        return;
    }
    if let Err(error) = state
        .activities
        .retire_superseded_presence(sender, |instance, change| {
            broadcast_activity_change(state, instance, change)
        })
    {
        tracing::warn!(event = "realtime.activity_cleanup_failed", error = ?error,
            "superseded activity cleanup failed closed");
    }
}

fn sweep_activities(state: &RealtimeState) {
    if let Err(error) = state.activities.sweep(
        state.clock.now().to_unix_millis().unwrap_or(0),
        |instance, change| broadcast_activity_change(state, instance, change),
    ) {
        tracing::warn!(event="realtime.activity_sweep_failed",error=?error,"activity expiration could not publish");
    }
}

async fn activity_sender_allowed(
    state: &RealtimeState,
    sender: ActivitySender,
    world_id: orbisync_domain::WorldId,
    session: Option<orbisync_domain::AuthSessionId>,
) -> bool {
    if !state
        .resume_store
        .is_current(sender.presence_id, sender.epoch)
        || !is_session_still_active(
            state.identity_repository.as_deref(),
            session,
            state.clock.now(),
        )
        .await
        || !ephemeral_participation_allowed(
            state.ephemeral_scope.as_deref(),
            sender.user_id,
            world_id.as_uuid(),
            state.clock.now(),
        )
        .await
    {
        return false;
    }
    let member = state
        .registry
        .read_members(sender.instance_id)
        .await
        .is_some_and(|members| members.contains(&(sender.presence_id, sender.user_id)));
    member
        && state
            .resume_store
            .is_current(sender.presence_id, sender.epoch)
}

async fn publish_client_activity(
    state: &RealtimeState,
    sender: ActivitySender,
    world_id: orbisync_domain::WorldId,
    session: Option<orbisync_domain::AuthSessionId>,
    claimed_instance: &str,
    update: &orbisync_protocol::v1::ActivityUpdate,
) -> Result<(), (&'static str, String)> {
    if claimed_instance != sender.instance_id.to_string() {
        return Err((
            "INSTANCE_MISMATCH",
            "activity must target the joined instance".into(),
        ));
    }
    if !activity_sender_allowed(state, sender, world_id, session).await {
        clear_activity_presence(state, sender);
        return Err(activity_error(ActivityError::NotBound));
    }
    let max_frame = usize::try_from(state.config.max_normal_message_bytes).unwrap_or(usize::MAX);
    state
        .activities
        .update_checked(
            sender,
            update.clone(),
            state.clock.now().to_unix_millis().unwrap_or(0),
            |snapshot| {
                // A candidate must remain representable even under a small configured
                // normal-frame limit. This validation runs before mutation under lock.
                let candidate = snapshot
                    .activities
                    .iter()
                    .find(|entry| {
                        entry.presence_id == sender.presence_id.to_string()
                            && entry.key == update.key
                    })
                    .cloned();
                if let Some(entry) = candidate {
                    let change = orbisync_protocol::v1::ActivityChanged {
                        activity: Some(entry),
                        removed: false,
                        // Expiration/clear must remain encodable after the
                        // global version crosses another protobuf varint boundary.
                        version: u64::MAX,
                    };
                    if activity_envelope(
                        sender.instance_id,
                        envelope::Payload::ActivityChanged(change),
                    )
                    .encoded_len()
                        > max_frame
                    {
                        return Err(ActivityError::SnapshotTooLarge);
                    }
                }
                let frames = activity_snapshot_frames(
                    sender.instance_id,
                    snapshot,
                    max_frame,
                    String::new(),
                )?;
                if frames.len() > state.config.outbound_queue_capacity as usize {
                    return Err(ActivityError::SnapshotTooLarge);
                }
                Ok(())
            },
            |instance, change| broadcast_activity_change(state, instance, change),
        )
        .map_err(activity_error)
}

async fn send_activity_snapshot(
    state: &RealtimeState,
    sink: &Arc<RealtimeDeliverySink>,
    sender: ActivitySender,
    world_id: orbisync_domain::WorldId,
    session: Option<orbisync_domain::AuthSessionId>,
    request_id: String,
) -> Result<(), (&'static str, String)> {
    if orbisync_domain::CommandId::parse(&request_id).is_err() {
        return Err(activity_error(ActivityError::Invalid));
    }
    if !activity_sender_allowed(state, sender, world_id, session).await {
        clear_activity_presence(state, sender);
        return Err(activity_error(ActivityError::NotBound));
    }
    let max_frame = usize::try_from(state.config.max_normal_message_bytes).unwrap_or(usize::MAX);
    state
        .activities
        .snapshot(
            sender.instance_id,
            state.clock.now().to_unix_millis().unwrap_or(0),
            |instance, change| broadcast_activity_change(state, instance, change),
            |snapshot| {
                // Build the entire bounded set before enqueue: a failed size check may
                // not leave the recipient with an incomplete baseline on a live queue.
                let frames =
                    activity_snapshot_frames(sender.instance_id, snapshot, max_frame, request_id)?;
                if frames.len() > state.config.outbound_queue_capacity as usize {
                    return Err(ActivityError::SnapshotTooLarge);
                }
                for frame in frames {
                    if sink.is_closed() || sink.queue_payload(frame).delivered != 1 {
                        return Err(ActivityError::DeliveryUnavailable);
                    }
                }
                Ok(())
            },
        )
        .map_err(activity_error)
}

fn activity_snapshot_frames(
    instance: InstanceId,
    snapshot: orbisync_protocol::v1::ActivitySnapshot,
    max_frame: usize,
    request_id: String,
) -> Result<Vec<Vec<u8>>, ActivityError> {
    let snapshot_id = if orbisync_domain::CommandId::parse(&request_id).is_ok() {
        request_id
    } else {
        uuid::Uuid::now_v7().to_string()
    };
    let skeleton = orbisync_protocol::v1::ActivitySnapshot {
        activities: Vec::new(),
        // Keep entry grouping stable as the global version grows, including
        // mutations in other instances between this admission and a snapshot.
        version: u64::MAX,
        snapshot_id: snapshot_id.clone(),
        chunk_index: 127,
        chunk_count: 128,
    };
    // Reserve the growth of both protobuf length delimiters as entries are added.
    let overhead = activity_envelope(instance, envelope::Payload::ActivitySnapshot(skeleton))
        .encoded_len()
        .saturating_add(16);
    let available = max_frame
        .checked_sub(overhead)
        .ok_or(ActivityError::SnapshotTooLarge)?;
    let mut groups: Vec<Vec<orbisync_protocol::v1::ActivityEntry>> = vec![Vec::new()];
    let mut current_bytes = 0usize;
    for entry in snapshot.activities {
        let len = entry.encoded_len();
        let mut size = len;
        let mut delimiter = 1;
        while size >= 128 {
            delimiter += 1;
            size >>= 7;
        }
        let wire = len + delimiter + 1;
        if wire > available {
            return Err(ActivityError::SnapshotTooLarge);
        }
        if current_bytes.saturating_add(wire) > available {
            if groups.len() >= ACTIVITY_SNAPSHOT_CHUNKS {
                return Err(ActivityError::SnapshotTooLarge);
            }
            groups.push(Vec::new());
            current_bytes = 0;
        }
        groups
            .last_mut()
            .ok_or(ActivityError::SnapshotTooLarge)?
            .push(entry);
        current_bytes += wire;
    }
    let count = u32::try_from(groups.len()).map_err(|_| ActivityError::SnapshotTooLarge)?;
    let mut total = 0usize;
    let mut frames = Vec::with_capacity(groups.len());
    for (index, activities) in groups.into_iter().enumerate() {
        let chunk = orbisync_protocol::v1::ActivitySnapshot {
            activities,
            version: snapshot.version,
            snapshot_id: snapshot_id.clone(),
            chunk_index: u32::try_from(index).map_err(|_| ActivityError::SnapshotTooLarge)?,
            chunk_count: count,
        };
        let bytes =
            activity_envelope(instance, envelope::Payload::ActivitySnapshot(chunk)).encode_to_vec();
        total = total.saturating_add(bytes.len());
        if bytes.len() > max_frame || total > ACTIVITY_SNAPSHOT_BYTES {
            return Err(ActivityError::SnapshotTooLarge);
        }
        frames.push(bytes);
    }
    Ok(frames)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "realtime_ws_tests_activity.rs"]
mod activity_wire_tests;
