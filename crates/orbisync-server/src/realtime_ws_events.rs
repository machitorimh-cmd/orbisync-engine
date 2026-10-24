// Custom client messages are room-scoped unless sent as a DirectedEvent.
// Core facts (ownership, moderation,
// membership, etc.) remain server-owned and cannot be published by clients.
fn prepare_client_event(
    event: &mut orbisync_protocol::v1::DomainEvent,
    user: UserId,
    presence: PresenceId,
) -> Result<(), &'static str> {
    if orbisync_domain::CommandId::parse(&event.event_id).is_err() {
        return Err("event_id must be a canonical UUIDv7");
    }
    let Some(name) = event.event_type.strip_prefix("custom.") else {
        return Err("client event_type must use the custom. namespace");
    };
    if event.event_type.len() > 128
        || name.is_empty()
        || name.split('.').any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    {
        return Err("event_type must contain non-empty ASCII name segments (maximum 128 bytes)");
    }
    let data = event.data.get_or_insert_with(prost_types::Struct::default);
    for (key, value) in [
        ("sender_user_id", user.to_string()),
        ("sender_presence_id", presence.to_string()),
    ] {
        data.fields.insert(
            key.to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(value)),
            },
        );
    }
    // Never forward a client-selected instance revision.
    event.instance_revision = 0;
    Ok(())
}

/// Validates membership through the actor before publishing to its room.
#[allow(clippy::too_many_arguments)]
async fn publish_client_event(
    state: &RealtimeState,
    instance_id: InstanceId,
    claimed_instance: &str,
    user: UserId,
    presence: PresenceId,
    mut event: orbisync_protocol::v1::DomainEvent,
    negotiated_minor: u32,
) -> Result<Option<Vec<u8>>, (&'static str, String)> {
    if claimed_instance != instance_id.to_string() {
        return Err((
            "INSTANCE_MISMATCH",
            "event must target the joined instance".to_owned(),
        ));
    }
    prepare_client_event(&mut event, user, presence)
        .map_err(|detail| ("INVALID_ARGUMENT", detail.to_owned()))?;
    if let Some(bytes) = state
        .registry
        .reliable_event(instance_id, presence, user, event.event_id.clone())
        .await
        .map_err(|code| (code, "event acknowledgement lookup failed".to_owned()))?
    {
        let mut cached = decode_envelope(&bytes)
            .map_err(|_| ("INTERNAL_ERROR", "invalid retained event".to_owned()))?;
        if let Some(envelope::Payload::DomainEvent(previous)) = cached.payload.as_mut() {
            previous.instance_revision = 0;
            if *previous == event {
                return Ok(Some(bytes.to_vec()));
            }
        }
        return Err((
            "EVENT_ID_CONFLICT",
            "event_id already identifies a different event".to_owned(),
        ));
    }
    let mut envelope = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: negotiated_minor,
        message_id: event.event_id.clone(),
        sequence: u64::MAX,
        sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
        instance_id: instance_id.to_string(),
        payload: Some(envelope::Payload::DomainEvent(event.clone())),
    };
    // Include server-added identity metadata, and reserve the largest revision
    // and sequence encodings before admitting the event to the actor.
    if let Some(envelope::Payload::DomainEvent(value)) = envelope.payload.as_mut() {
        value.instance_revision = u64::MAX;
    }
    if envelope.encoded_len() as u64 > state.config.max_message_bytes {
        return Err((
            "MESSAGE_TOO_LARGE",
            "event with server metadata exceeds the message limit".to_owned(),
        ));
    }
    let outcome = state
        .registry
        .submit(
            instance_id,
            InstanceCommand::PublishEvent {
                presence_id: presence,
                user_id: user,
            },
        )
        .await
        .map_err(|_| {
            (
                "SERVER_BUSY",
                "instance cannot accept the event; retry later".to_owned(),
            )
        })?;
    match outcome {
        CommandOutcome::Applied { revision, .. } => event.instance_revision = revision.as_u64(),
        CommandOutcome::Rejected { code, detail } => return Err((code, detail)),
    }
    envelope.payload = Some(envelope::Payload::DomainEvent(event));
    envelope.sequence = 0;
    let bytes = envelope.encode_to_vec();
    let revision = match &envelope.payload {
        Some(envelope::Payload::DomainEvent(value)) => value.instance_revision,
        _ => 0,
    };
    retain_reliable_event(state, instance_id, revision, bytes.clone())
        .await
        .map_err(|()| ("SERVER_BUSY", "event history unavailable".to_owned()))?;
    let outcome = state
        .delivery
        .broadcast(instance_id, bytes, Reliability::Reliable);
    if outcome.disconnected_slow_consumers > 0 {
        tracing::warn!(event = "realtime.domain_event_slow_consumer", instance_id = %instance_id,
            disconnected = outcome.disconnected_slow_consumers,
            "disconnected slow consumers on reliable event queue overflow");
    }
    Ok(None)
}

const MAX_DIRECTED_RECIPIENTS: usize = 64;

/// The retained audience is a set of presence incarnations, never user IDs
/// resolved again during delivery. A later join by the same user gains no
/// access to an earlier event.
fn directed_event_visible_to(envelope: &Envelope, presence: Option<PresenceId>) -> bool {
    match envelope.payload.as_ref() {
        Some(envelope::Payload::DirectedEvent(event)) => presence.is_some_and(|presence| {
            let id = presence.to_string();
            event
                .resolved_presence_ids
                .iter()
                .any(|recipient| recipient == &id)
        }),
        _ => true,
    }
}

/// Match the SDK's JSON value contract without coercing malformed protobuf
/// values to null. Root Struct is depth zero and counts as one value.
fn validate_directed_event_data(data: &prost_types::Struct) -> Result<(), &'static str> {
    let mut pending: Vec<_> = data.fields.values().map(|value| (value, 1usize)).collect();
    let mut values = 1usize;
    while let Some((value, depth)) = pending.pop() {
        values += 1;
        if depth > 64 || values > 1_000_000 {
            return Err("event data exceeds JSON depth or value limits");
        }
        match value.kind.as_ref() {
            Some(prost_types::value::Kind::StructValue(value)) => {
                pending.extend(value.fields.values().map(|value| (value, depth + 1)));
            }
            Some(prost_types::value::Kind::ListValue(value)) => {
                pending.extend(value.values.iter().map(|value| (value, depth + 1)));
            }
            Some(prost_types::value::Kind::NumberValue(value))
                if !value.is_finite()
                    || value.fract() == 0.0 && value.abs() > 9_007_199_254_740_991.0 =>
            {
                return Err("event data requires finite numbers and safe integers");
            }
            Some(prost_types::value::Kind::NullValue(value)) if *value != 0 => {
                return Err("event data contains an invalid null value");
            }
            None => return Err("event data value kind is required"),
            _ => {}
        }
    }
    if data.encoded_len() > 65_536 {
        return Err("event data exceeds 65536 encoded bytes");
    }
    Ok(())
}

fn prepare_directed_event(
    directed: &mut orbisync_protocol::v1::DirectedEvent,
    user: UserId,
    presence: PresenceId,
) -> Result<(), &'static str> {
    if !directed.resolved_presence_ids.is_empty() {
        return Err("resolved_presence_ids is server-owned");
    }
    let event = directed.event.as_mut().ok_or("event is required")?;
    prepare_client_event(event, user, presence)?;
    if let Some(data) = event.data.as_ref() {
        validate_directed_event_data(data)?;
    }
    let users = directed.recipient_user_ids.len();
    let presences = directed.recipient_presence_ids.len();
    if (users == 0) == (presences == 0) || users.max(presences) > MAX_DIRECTED_RECIPIENTS {
        return Err("specify 1..64 users or 1..64 presences, exclusively");
    }
    if directed
        .recipient_user_ids
        .iter()
        .any(|id| UserId::parse(id).is_err())
        || directed
            .recipient_presence_ids
            .iter()
            .any(|id| PresenceId::parse(id).is_err())
    {
        return Err("recipients must be canonical UUIDv7 identifiers");
    }
    // Recipient order is immaterial to an idempotent retry. Duplicate entries
    // are rejected so the public bound always describes distinct recipients.
    directed.recipient_user_ids.sort_unstable();
    directed.recipient_presence_ids.sort_unstable();
    if directed
        .recipient_user_ids
        .windows(2)
        .any(|pair| pair[0] == pair[1])
        || directed
            .recipient_presence_ids
            .windows(2)
            .any(|pair| pair[0] == pair[1])
    {
        return Err("recipients must be distinct");
    }
    Ok(())
}

fn resolve_directed_recipients(
    directed: &orbisync_protocol::v1::DirectedEvent,
    members: &[(PresenceId, UserId)],
    sender: (PresenceId, UserId),
) -> Result<Vec<String>, (&'static str, String)> {
    if !members.contains(&sender) {
        return Err((
            "NOT_JOINED",
            "event sender is not a current member".to_owned(),
        ));
    }
    let users: std::collections::HashSet<_> = directed
        .recipient_user_ids
        .iter()
        .map(|id| UserId::parse(id))
        .collect::<Result<_, _>>()
        .map_err(|_| ("INVALID_ARGUMENT", "invalid recipient user".to_owned()))?;
    let presences: std::collections::HashSet<_> = directed
        .recipient_presence_ids
        .iter()
        .map(|id| PresenceId::parse(id))
        .collect::<Result<_, _>>()
        .map_err(|_| ("INVALID_ARGUMENT", "invalid recipient presence".to_owned()))?;
    let mut found_users = std::collections::HashSet::new();
    let mut found_presences = std::collections::HashSet::new();
    let mut resolved = std::collections::BTreeSet::new();
    // Scan membership once, without formatting every UUID for every target.
    for (presence, user) in members {
        if users.contains(user) {
            found_users.insert(*user);
            resolved.insert(*presence);
        }
        if presences.contains(presence) {
            found_presences.insert(*presence);
            resolved.insert(*presence);
        }
    }
    if users.len() != found_users.len() || presences.len() != found_presences.len() {
        return Err((
            "RECIPIENT_NOT_JOINED",
            "every recipient must belong to the joined instance".to_owned(),
        ));
    }
    resolved.insert(sender.0);
    Ok(resolved.into_iter().map(|id| id.to_string()).collect())
}

/// The caller holds the instance publication fence, just as for room events.
/// Resolve the whole audience before committing; never partially publish.
#[allow(clippy::too_many_arguments)]
async fn publish_directed_event(
    state: &RealtimeState,
    instance_id: InstanceId,
    claimed_instance: &str,
    user: UserId,
    presence: PresenceId,
    mut directed: orbisync_protocol::v1::DirectedEvent,
    negotiated_minor: u32,
) -> Result<Option<Vec<u8>>, (&'static str, String)> {
    if claimed_instance != instance_id.to_string() {
        return Err((
            "INSTANCE_MISMATCH",
            "event must target the joined instance".to_owned(),
        ));
    }
    prepare_directed_event(&mut directed, user, presence)
        .map_err(|detail| ("INVALID_ARGUMENT", detail.to_owned()))?;
    let event_id = directed
        .event
        .as_ref()
        .map(|event| event.event_id.clone())
        .ok_or_else(|| ("INVALID_ARGUMENT", "event is required".to_owned()))?;
    if let Some(bytes) = state
        .registry
        .reliable_event(instance_id, presence, user, event_id.clone())
        .await
        .map_err(|code| (code, "event acknowledgement lookup failed".to_owned()))?
    {
        let mut cached = decode_envelope(&bytes)
            .map_err(|_| ("INTERNAL_ERROR", "invalid retained event".to_owned()))?;
        // Apply the same audience boundary to acknowledgement lookups. Current
        // room membership alone must never expose someone else's private event.
        if directed_event_visible_to(&cached, Some(presence)) {
            if let Some(envelope::Payload::DirectedEvent(previous)) = cached.payload.as_mut() {
                if let Some(event) = previous.event.as_mut() {
                    event.instance_revision = 0;
                }
                previous.resolved_presence_ids.clear();
                if *previous == directed {
                    // Reuse the original bytes and frozen audience. In
                    // particular do not expand user targets to newly joined
                    // presences when a sender retries after acknowledgement loss.
                    return Ok(Some(bytes.to_vec()));
                }
            }
        }
        return Err((
            "EVENT_ID_CONFLICT",
            "event_id already identifies a different event".to_owned(),
        ));
    }
    let members = state
        .registry
        .read_members(instance_id)
        .await
        .ok_or_else(|| ("SERVER_BUSY", "recipient membership unavailable".to_owned()))?;
    directed.resolved_presence_ids =
        resolve_directed_recipients(&directed, &members, (presence, user))?;
    let mut envelope = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: negotiated_minor,
        message_id: event_id,
        sequence: u64::MAX,
        sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
        instance_id: instance_id.to_string(),
        payload: Some(envelope::Payload::DirectedEvent(directed)),
    };
    if let Some(envelope::Payload::DirectedEvent(value)) = envelope.payload.as_mut() {
        if let Some(event) = value.event.as_mut() {
            event.instance_revision = u64::MAX;
        }
    }
    if envelope.encoded_len() as u64 > state.config.max_message_bytes {
        return Err((
            "MESSAGE_TOO_LARGE",
            "event with resolved recipients exceeds the message limit".to_owned(),
        ));
    }
    let outcome = state
        .registry
        .submit(
            instance_id,
            InstanceCommand::PublishEvent {
                presence_id: presence,
                user_id: user,
            },
        )
        .await
        .map_err(|_| {
            (
                "SERVER_BUSY",
                "instance cannot accept the event; retry later".to_owned(),
            )
        })?;
    let revision = match outcome {
        CommandOutcome::Applied { revision, .. } => revision.as_u64(),
        CommandOutcome::Rejected { code, detail } => return Err((code, detail)),
    };
    if let Some(envelope::Payload::DirectedEvent(value)) = envelope.payload.as_mut() {
        if let Some(event) = value.event.as_mut() {
            event.instance_revision = revision;
        }
    }
    envelope.sequence = 0;
    let bytes = envelope.encode_to_vec();
    retain_reliable_event(state, instance_id, revision, bytes.clone())
        .await
        .map_err(|()| ("SERVER_BUSY", "event history unavailable".to_owned()))?;
    let outcome = state
        .delivery
        .broadcast(instance_id, bytes, Reliability::Reliable);
    if outcome.disconnected_slow_consumers > 0 {
        tracing::warn!(event = "realtime.directed_event_slow_consumer", instance_id = %instance_id,
            disconnected = outcome.disconnected_slow_consumers,
            "disconnected slow consumers on directed event queue overflow");
    }
    Ok(None)
}

async fn retain_reliable_event(
    state: &RealtimeState,
    instance: InstanceId,
    revision: u64,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let message_id = decode_envelope(&payload).map_err(|_| ())?.message_id;
    match state
        .registry
        .submit(
            instance,
            InstanceCommand::RetainReliable {
                message_id,
                revision: orbisync_domain::Revision::from_u64(revision),
                payload: payload.into(),
            },
        )
        .await
    {
        Ok(CommandOutcome::Applied { .. }) => Ok(()),
        _ => Err(()),
    }
}

#[cfg(test)]
#[path = "realtime_ws_tests_directed.rs"]
mod directed_event_tests;
