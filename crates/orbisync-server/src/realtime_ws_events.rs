// Custom client messages are room-scoped. Core facts (ownership, moderation,
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
    if let Some(bytes) = state.registry.reliable_event(instance_id, presence, user, event.event_id.clone())
        .await.map_err(|code| (code, "event acknowledgement lookup failed".to_owned()))? {
        let mut cached = decode_envelope(&bytes)
            .map_err(|_| ("INTERNAL_ERROR", "invalid retained event".to_owned()))?;
        if let Some(envelope::Payload::DomainEvent(previous)) = cached.payload.as_mut() {
            previous.instance_revision = 0;
            if *previous == event {
                return Ok(Some(bytes.to_vec()));
            }
        }
        return Err(("EVENT_ID_CONFLICT", "event_id already identifies a different event".to_owned()));
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
