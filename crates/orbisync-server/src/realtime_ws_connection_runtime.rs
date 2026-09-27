// Reason: the runtime receives the established connection state by value so
// the handshake and join paths remain behaviorally unchanged.
const PERSISTENCE_FOREGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
const PERSISTENCE_BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const PERSISTENCE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

async fn acquire_command_durability_guard(
    state: &RealtimeState,
    instance_id: InstanceId,
    deadline: tokio::time::Instant,
) -> Option<OwnedMutexGuard<()>> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return None;
    }
    tokio::time::timeout(remaining, state.command_dedup.durability_guard(instance_id))
        .await
        .ok()
}

/// Persists a command outcome before publishing its success or rejection.
/// When the port is unavailable, the socket gets a bounded failure while a
/// detached worker keeps the local command fenced and attempts recovery.
async fn persist_command_outcome(
    state: Arc<RealtimeState>,
    instance_id: InstanceId,
    reservation: Option<crate::command_dedup::CommandDedupReservation>,
    result: crate::command_dedup::CommandDedupResult,
    now: orbisync_domain::Timestamp,
    guard: Option<OwnedMutexGuard<()>>,
    foreground_deadline: tokio::time::Instant,
) -> bool {
    persist_command_outcome_with_publication(state, instance_id, reservation, result,
        now, guard, foreground_deadline, None).await
}

/// Owned only by the original leader, never reconstructed from duplicate receipts.
struct CommandPublication {
    negotiated_minor: u32,
    views: Arc<orbisync_world_runtime::InterestSnapshot>,
    revision: u64,
}

impl CommandPublication {
    async fn publish(self, state: &RealtimeState, instance_id: InstanceId, result: &crate::command_dedup::CommandDedupResult) {
        if let crate::command_dedup::CommandDedupResult::Applied { command, message_id } = result {
            // Existing registered sinks own their lifecycle/floor/authorization checks.
            // No registry read: an actor may stop or mutate while storage is pending.
            broadcast_entity_command(state, instance_id, command.clone(), message_id.clone(),
                self.negotiated_minor, self.views, self.revision).await;
        }
    }
}

async fn persist_command_outcome_with_publication(
    state: Arc<RealtimeState>,
    instance_id: InstanceId,
    reservation: Option<crate::command_dedup::CommandDedupReservation>,
    result: crate::command_dedup::CommandDedupResult,
    now: orbisync_domain::Timestamp,
    guard: Option<OwnedMutexGuard<()>>,
    foreground_deadline: tokio::time::Instant,
    publication: Option<CommandPublication>,
) -> bool {
    let Some(mut reservation) = reservation else {
        if let Some(publication) = publication { publication.publish(&state, instance_id, &result).await; }
        return true;
    };
    reservation.set_outcome(result.clone());
    if state.checkpoint_store.is_none() {
        if let Some(publication) = publication { publication.publish(&state, instance_id, &result).await; }
        reservation.complete(result, now.to_unix_millis().unwrap_or(0));
        return true;
    }
    let command_id = reservation.command_id();
    let fingerprint = reservation.fingerprint();

    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    let recovery_state = Arc::clone(&state);
    state.spawn_recovery(async move {
        let foreground = persist_command_outcome_inner(
            Arc::clone(&recovery_state),
            instance_id,
            &result,
            command_id,
            fingerprint,
            guard,
            foreground_deadline,
        )
        .await;
        if let Some(completed_at_millis) = foreground {
            if let Some(publication) = publication { publication.publish(&recovery_state, instance_id, &result).await; }
            reservation.complete(result, completed_at_millis);
            if status_tx.send(true).is_err() {
                tracing::debug!(event = "realtime.command_status_receiver_closed");
            }
            return;
        }

        // The socket is released at this point with PERSISTENCE_UNAVAILABLE,
        // while this instance-local worker gets a separate bounded recovery
        // window. A cancelled socket cannot drop the reservation or execute a
        // second mutation.
        if status_tx.send(false).is_err() {
            tracing::debug!(event = "realtime.command_status_receiver_closed");
        }
        let recovered = persist_command_outcome_inner(
            Arc::clone(&recovery_state),
            instance_id,
            &result,
            command_id,
            fingerprint,
            None,
            tokio::time::Instant::now() + PERSISTENCE_BACKGROUND_DEADLINE,
        )
        .await;
        if let Some(completed_at_millis) = recovered {
            if let Some(publication) = publication { publication.publish(&recovery_state, instance_id, &result).await; }
            reservation.complete(result, completed_at_millis);
        } else {
            tracing::error!(
                event = "realtime.command_durability_dirty",
                instance_id = %instance_id,
                "bounded background recovery exhausted; command remains fenced locally"
            );
            reservation.mark_dirty(result);
        }
    });
    status_rx.await.unwrap_or(false)
}

async fn persist_command_outcome_inner(
    state: Arc<RealtimeState>,
    instance_id: InstanceId,
    result: &crate::command_dedup::CommandDedupResult,
    command_id: orbisync_domain::CommandId,
    fingerprint: [u8; 32],
    guard: Option<OwnedMutexGuard<()>>,
    deadline: tokio::time::Instant,
) -> Option<i64> {
    let Some(store) = state.checkpoint_store.as_ref() else {
        return Some(state.clock.now().to_unix_millis().unwrap_or(0));
    };
    let _durability_guard = match guard {
        Some(guard) => guard,
        None => match tokio::time::timeout_at(
            deadline,
            state.command_dedup.durability_guard(instance_id),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => return None,
        },
    };
    loop {
        let now = state.clock.now();
        let now_millis = now.to_unix_millis().unwrap_or(0);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let Some(mut checkpoint) =
            tokio::time::timeout(remaining, state.registry.build_checkpoint(instance_id, now))
                .await
                .ok()
                .flatten()
        else {
            tracing::error!(
                event = "realtime.command_durability_failed",
                instance_id = %instance_id,
                detail = "actor checkpoint unavailable before command acknowledgement"
            );
            return None;
        };
        // The reservation's pending outcome is part of the normal snapshot,
        // which also protects it from a concurrent periodic checkpoint.
        checkpoint.dedup = state.command_dedup.snapshot_with_outcome(
            instance_id,
            now_millis,
            command_id,
            fingerprint,
            result,
        );
        let payload = match checkpoint.to_json_bytes() {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(
                    event = "realtime.command_durability_failed",
                    instance_id = %instance_id,
                    error = %error,
                    detail = "command outcome checkpoint serialization failed"
                );
                return None;
            }
        };
        let app_checkpoint = orbisync_application::AppCheckpoint::new(
            checkpoint.instance_id,
            checkpoint.revision,
            payload,
            now,
        );
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, store.save_checkpoint(app_checkpoint)).await {
            Ok(Ok(receipts)) => {
                let Some(receipt) = receipts
                    .into_iter()
                    .find(|receipt| receipt.command_id == command_id.to_string())
                else {
                    tracing::error!(
                        event = "realtime.command_durability_failed",
                        instance_id = %instance_id,
                        detail = "checkpoint store did not return the command durability receipt"
                    );
                    return None;
                };
                let Some(expected_expires_at) = receipt
                    .created_at_millis
                    .checked_add(crate::command_dedup::CommandDedupStore::DEFAULT_TTL_MILLIS)
                else {
                    tracing::error!(
                        event = "realtime.command_durability_failed",
                        instance_id = %instance_id,
                        detail = "checkpoint store returned an overflowing command expiry"
                    );
                    return None;
                };
                if receipt.expires_at_millis != expected_expires_at {
                    tracing::error!(
                        event = "realtime.command_durability_failed",
                        instance_id = %instance_id,
                        detail = "checkpoint store returned an invalid command expiry"
                    );
                    return None;
                }
                return Some(receipt.created_at_millis);
            }
            Ok(Err(error)) => {
                tracing::error!(
                    event = "realtime.command_durability_failed",
                    instance_id = %instance_id,
                    error = %error,
                    detail = "command result remains fenced while bounded recovery runs"
                );
            }
            Err(_) => return None,
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(PERSISTENCE_RETRY_DELAY).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connected_socket(
    mut socket: RealtimeSocket,
    state: Arc<RealtimeState>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<ShutdownNotice>,
    mut conn_state: ConnectionState,
    handshake: orbisync_realtime::gateway::HandshakeSuccess,
    heartbeat_policy: HeartbeatPolicy,
    user: UserId,
    session_id: Option<orbisync_domain::AuthSessionId>,
    permissions: WorldPermissions,
    mut viewer_entity: Option<EntityId>,
    delivery_sink: Arc<RealtimeDeliverySink>,
    delivery_registration: crate::delivery::DeliveryRegistration,
    final_instance_id: InstanceId,
    final_world_id: orbisync_domain::WorldId,
    final_presence: PresenceId,
    next_sequence_after_join: u64,
    inbound_sequence_after_join: u64,
    mut shutdown_notified: bool,
    presence_guard_inner: Option<PresenceGuard>,
) {
    // Join and resume both pass their initialized state into this runtime loop.
    let instance_id = final_instance_id;
    let world_id = final_world_id;
    // Presence and delivery state are initialized in both join/resume paths.
    let mut _presence_guard = presence_guard_inner;
    // The registration and bounded queue were created before snapshot read.
    let publication_lock = state.registry.lifecycle_lock(instance_id);
    let outbound_queue = delivery_sink.queue();
    let mut next_sequence: u64 = next_sequence_after_join;
    // Inbound sequence is connection-local and deliberately starts at one on
    // every reconnect (ADR-006A). Validate it before rate limiting or actor
    // admission so replays have no observable side effects.
    let mut inbound_sequence =
        orbisync_realtime::InboundSequenceTracker::with_expected(inbound_sequence_after_join);
    let mut heartbeat = HeartbeatManager::new(heartbeat_policy, state.clock.now());
    let mut rate_limiter = RateLimiter::new_with_threshold(
        state.rate_limit_normal_per_sec,
        state.rate_limit_custom_per_sec,
        state.rate_limit_persistent_threshold,
        state.clock.now(),
    );
    let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_millis(
        heartbeat_policy.interval_millis(),
    ));
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick; the first heartbeat is due after `interval`, not instantly.
    heartbeat_interval.tick().await;
    loop {
        if delivery_sink.is_closed() {
            break;
        }
        tokio::select! {
            notice = shutdown_rx.recv() => {
                match notice {
                    Ok(ShutdownNotice::Begin) if !shutdown_notified => {
                        send_error(
                            &mut socket,
                            "SERVER_SHUTTING_DOWN",
                            "server is shutting down; please reconnect".to_owned(),
                            String::new(),
                            handshake.negotiated_minor,
                            next_sequence,
                            state.clock.as_ref(),
                            &instance_id.to_string(),
                        )
                        .await;
                        next_sequence += 1;
                        shutdown_notified = true;
                    }
                    Ok(ShutdownNotice::Force) => {
                        let abandoned = outbound_queue
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .len();
                        if abandoned > 0 {
                            tracing::warn!(
                                event = "realtime.queue_drain_abandoned",
                                instance_id = %instance_id,
                                abandoned,
                                "abandoning queued realtime messages at forced shutdown"
                            );
                        }
                        break;
                    }
                    _ => {}
                }
            }
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break; };
                let bytes = match msg {
                    Message::Binary(b) => b,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let now = state.clock.now();
                let foreground_deadline =
                    tokio::time::Instant::now() + PERSISTENCE_FOREGROUND_DEADLINE;
                if heartbeat.is_timed_out(now) {
                    tracing::warn!(
                        event = "realtime.heartbeat_timeout",
                        instance_id = %instance_id,
                        "heartbeat timeout on inbound – closing connection"
                    );
                    // Reason: heartbeat timeout always closes the connection (break); the state transition is validated but the resulting Option is not used because the task terminates (A-3 allow with reason).
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = heartbeat.next_state_on_timeout(conn_state, now);
                    break;
                }
                let mut env = match decode_envelope(&bytes) {
                    Ok(envelope) => envelope,
                    Err(_) => {
                        send_error(&mut socket, "INVALID_MESSAGE", "invalid protobuf envelope".to_owned(),
                            String::new(), handshake.negotiated_minor, next_sequence,
                            state.clock.as_ref(), &instance_id.to_string()).await;
                        break;
                    }
                };
                if let Err(error) = orbisync_realtime::check_message_size_for_envelope(
                    &bytes,
                    &env,
                    state.config.max_normal_message_bytes,
                    state.config.max_message_bytes,
                ) {
                    send_error(
                        &mut socket,
                        "MESSAGE_TOO_LARGE",
                        error.to_string(),
                        env.message_id.clone(),
                        handshake.negotiated_minor,
                        next_sequence,
                        state.clock.as_ref(),
                        &instance_id.to_string(),
                    )
                    .await;
                    break;
                }
                match inbound_sequence.accept(env.sequence) {
                    orbisync_realtime::InboundSequence::Exact => {}
                    orbisync_realtime::InboundSequence::Duplicate => {
                        tracing::debug!(
                            event = "realtime.inbound_sequence_duplicate",
                            instance_id = %instance_id,
                            sequence = env.sequence,
                            expected = inbound_sequence.expected(),
                            "dropping duplicate inbound envelope before admission"
                        );
                        continue;
                    }
                    orbisync_realtime::InboundSequence::Gap { expected, received } => {
                        send_error(
                            &mut socket,
                            "SEQUENCE_GAP",
                            format!("expected inbound sequence {expected}, received {received}"),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            next_sequence,
                            state.clock.as_ref(),
                            &instance_id.to_string(),
                        )
                        .await;
                        break;
                    }
                    orbisync_realtime::InboundSequence::Overflow => {
                        send_error(
                            &mut socket,
                            "SEQUENCE_OVERFLOW",
                            "inbound sequence exhausted; reconnect required".to_owned(),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            next_sequence,
                            state.clock.as_ref(),
                            &instance_id.to_string(),
                        )
                        .await;
                        break;
                    }
                }
                // Heartbeats remain observable while draining; new application
                // work must consult the process gate, not a delayed notice.
                let _admission = if matches!(&env.payload,
                    Some(envelope::Payload::Heartbeat(_) | envelope::Payload::HeartbeatAck(_))) {
                    None
                } else {
                    match state.shutdown.admit() {
                        Some(guard) => Some(guard),
                        None => {
                            send_error(&mut socket, "SERVER_SHUTTING_DOWN",
                                "server is shutting down; please reconnect".into(),
                                env.message_id.clone(), handshake.negotiated_minor,
                                next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                            next_sequence += 1;
                            continue;
                        }
                    }
                };
                // Command IDs are validated and admitted before rate limiting.
                // A duplicate therefore cannot consume a rate token, mailbox
                // slot, or actor state transition.  The reservation is held
                // until the leader records the final deterministic result.
                let mut generation_command_guard = None;
                let mut command_id_for_dispatch = None;
                let mut command_dedup_reservation = None;
                if let Some(envelope::Payload::EntityCommand(command)) = &env.payload {
                    let command_id = match orbisync_domain::CommandId::parse(&command.command_id) {
                        Ok(value) => value,
                        Err(_) => {
                            send_error(
                                &mut socket,
                                "INVALID_ARGUMENT",
                                "command_id is not a valid UUIDv7".to_owned(),
                                env.message_id.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                    };
                    let fingerprint =
                        crate::command_dedup::CommandDedupStore::fingerprint(command);
                    if let Some(service) = &state.generation {
                        if state.registry.is_present(instance_id, final_presence) != Some(true)
                            || !is_session_still_active(state.identity_repository.as_deref(), session_id, state.clock.now()).await
                            || !ephemeral_participation_allowed(state.ephemeral_scope.as_deref(), user, world_id.as_uuid(), state.clock.now()).await {
                            send_error(&mut socket, "NOT_AUTHORIZED", "current participation required".into(), env.message_id.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                            next_sequence += 1;
                            continue;
                        }
                        generation_command_guard = match tokio::time::timeout_at(foreground_deadline, service.command_guard(instance_id)).await {
                            Ok(guard) => Some(guard),
                            Err(_) => {
                                send_error_with_retryable(&mut socket, "PERSISTENCE_UNAVAILABLE", "command preparation busy".into(), env.message_id.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string(), true).await;
                                next_sequence += 1;
                                continue;
                            }
                        };
                        use crate::checkpoint_admission::GenerationDispatch;
                        match crate::checkpoint_admission::dispatch_generation_receipt(&state, instance_id, command, foreground_deadline).await {
                            Ok(GenerationDispatch::Fresh) => {},
                            Ok(GenerationDispatch::Reply(result)) => {
                                send_command_result(&mut socket, &result, env.message_id.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                next_sequence += 1;
                                continue;
                            }
                            other => {
                                let (code, detail, retryable) = match other {
                                    Ok(GenerationDispatch::Refused { code, detail, retryable }) => (code, detail, retryable),
                                    _ => ("PERSISTENCE_UNAVAILABLE".into(), "generation receipt unavailable".into(), true),
                                };
                                send_error_with_retryable(&mut socket, &code, detail, env.message_id.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string(), retryable).await;
                                next_sequence += 1;
                                continue;
                            }
                        }
                        command_id_for_dispatch = Some(command_id);
                    } else { match state.command_dedup.begin(
                        instance_id,
                        command_id,
                        fingerprint,
                        now.to_unix_millis().unwrap_or(0),
                    ) {
                        crate::command_dedup::CommandDedupAdmission::Leader(reservation) => {
                            command_id_for_dispatch = Some(command_id);
                            command_dedup_reservation = Some(reservation);
                        }
                        crate::command_dedup::CommandDedupAdmission::Duplicate(result) => {
                            send_command_result(
                                &mut socket,
                                &result,
                                env.message_id.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                        crate::command_dedup::CommandDedupAdmission::InFlight(waiter) => {
                            tokio::select! {
                                result = waiter.wait() => {
                                    if let Some(result) = result {
                                        send_command_result(
                                            &mut socket,
                                            &result,
                                            env.message_id.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        ).await;
                                        next_sequence += 1;
                                    } else {
                                        send_error_with_retryable(
                                            &mut socket,
                                            "COMMAND_RETRY",
                                            "the command leader disconnected before completion".to_owned(),
                                            env.message_id.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                            true,
                                        ).await;
                                        next_sequence += 1;
                                    }
                                }
                                _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                                    send_error_with_retryable(
                                        &mut socket,
                                        "COMMAND_TIMEOUT",
                                        "timed out waiting for the command leader".to_owned(),
                                        env.message_id.clone(),
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                        true,
                                    ).await;
                                    next_sequence += 1;
                                }
                            }
                            continue;
                        }
                        crate::command_dedup::CommandDedupAdmission::Conflict => {
                            send_error(
                                &mut socket,
                                "COMMAND_ID_CONFLICT",
                                "command_id was previously used for a different payload".to_owned(),
                                env.message_id.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                        crate::command_dedup::CommandDedupAdmission::Capacity => {
                            send_error(
                                &mut socket,
                                "COMMAND_ID_CAPACITY",
                                "command idempotency store is at capacity".to_owned(),
                                env.message_id.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                        crate::command_dedup::CommandDedupAdmission::PersistenceUnavailable => {
                            send_error_with_retryable(
                                &mut socket,
                                "PERSISTENCE_UNAVAILABLE",
                                "the previous command is still recovering durability".to_owned(),
                                env.message_id.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                                true,
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                    } }
                }
                let _generation_command_guard = generation_command_guard;
                let category = match &env.payload {
                    Some(envelope::Payload::EntityCommand(_))
                    | Some(envelope::Payload::DomainEvent(_)) => MessageCategory::Custom,
                    _ => MessageCategory::Normal,
                };
                if let Err(err) = rate_limiter.check(now, category) {
                    let is_persistent = err.is_persistent();
                    if let Some(event) = err.to_event() {
                        if let Some(next) = transition(conn_state, event) {
                            conn_state = next;
                        }
                    }
                    let code = if is_persistent {
                        "PERSISTENT_RATE_LIMIT"
                    } else {
                        "RATE_LIMITED"
                    };
                    let retryable = !is_persistent;
                    send_error_with_retryable(
                        &mut socket,
                        code,
                        err.to_string(),
                        env.message_id.clone(),
                        handshake.negotiated_minor,
                        next_sequence,
                        state.clock.as_ref(),
                        &instance_id.to_string(),
                        retryable,
                    )
                    .await;
                    next_sequence += 1;
                    if is_persistent {
                        tracing::warn!(
                            event = "realtime.persistent_rate_limit",
                            instance_id = %instance_id,
                            category = %category,
                            "persistent rate limit – closing connection"
                        );
                        break;
                    }
                    continue;
                }
                if let Some(envelope::Payload::Heartbeat(hb)) = &env.payload {
                    heartbeat.record_heartbeat(now);
                    if let Some(ack_env) = heartbeat_ack_for_heartbeat(
                        handshake.negotiated_minor,
                        uuid::Uuid::now_v7().to_string(),
                        next_sequence,
                        instance_id.to_string(),
                        hb,
                        now,
                    ) {
                        next_sequence += 1;
                        let mut buf = Vec::new();
                        if ack_env.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                    }
                    continue;
                }
                if let Some(envelope::Payload::HeartbeatAck(_)) = &env.payload {
                    heartbeat.record_activity(now);
                    continue;
                }
                heartbeat.record_activity(now);
                let message_id_for_payload = env.message_id.clone();
                // Serialize publication and snapshot handoff within this room.
                let mut _publication_guard = Some(publication_lock.lock().await);
                match env.payload.take() {
                    Some(envelope::Payload::DomainEvent(event)) => {
                        match publish_client_event(
                            &state, instance_id, &env.instance_id, user, final_presence,
                            event, handshake.negotiated_minor,
                        ).await {
                          Ok(Some(bytes)) => {
                            // Use the same reliable queue as first delivery; do not rebroadcast.
                            let outcome = delivery_sink.queue_payload(bytes);
                            if outcome.disconnected_slow_consumer > 0 { return; }
                          }
                          Ok(None) => {}
                          Err((code, detail)) => {
                            send_error_with_retryable(&mut socket, code, detail, message_id_for_payload,
                                handshake.negotiated_minor, next_sequence, state.clock.as_ref(),
                                &instance_id.to_string(), code == "SERVER_BUSY").await;
                            next_sequence += 1;
                          }
                        }
                    }
                    Some(envelope::Payload::TransformInput(input)) => {
                        let application_started = Instant::now();
                        state.metrics_recorder.incr(Counter::InstanceCommandsTotal);
                    let parsed = (|| -> Result<Transform, String> {
                        let t = input.transform.as_ref().ok_or("transform is required")?;
                        let pos = Vec3::new(t.position_x, t.position_y, t.position_z).map_err(|error| error.to_string())?;
                        let rot = orbisync_domain::Quaternion::new(t.rotation_x, t.rotation_y, t.rotation_z, t.rotation_w).map_err(|error| error.to_string())?;
                        let scale = Vec3::new(1.0, 1.0, 1.0).map_err(|error| error.to_string())?;
                        Transform::new(pos, rot, scale).map_err(|error| error.to_string())
                    })();
                    let transform = match parsed {
                        Ok(transform) => transform,
                        Err(detail) => {
                            send_error(&mut socket, "INVALID_ARGUMENT", detail, env.message_id.clone(),
                                handshake.negotiated_minor, next_sequence, state.clock.as_ref(),
                                &instance_id.to_string()).await;
                            next_sequence += 1;
                            continue;
                        }
                    };
                    let Ok(entity_id) = input.entity_id.parse::<EntityId>() else {
                        send_error(
                            &mut socket,
                            "INVALID_ARGUMENT",
                            "entity_id is not a valid id".to_owned(),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            next_sequence,
                            state.clock.as_ref(),
                            &instance_id.to_string(),
                        )
                        .await;
                        next_sequence += 1;
                        continue;
                    };
                    let expected = orbisync_domain::Revision::from_u64(input.expected_revision);
                    let now_ts = state.clock.now();
                    // ADR-026: transform updates reach this branch and never
                    // the pre-commit arm below, so the participation boundary
                    // has to be consulted here on its own. Without it a
                    // temporary subject whose deadline passed, or whose
                    // permitted worlds changed, would keep streaming
                    // transforms for the rest of the connection.
                    if !ephemeral_participation_allowed(
                        state.ephemeral_scope.as_deref(),
                        user,
                        world_id.as_uuid(),
                        now_ts,
                    )
                    .await
                    {
                        send_error(
                            &mut socket,
                            "NOT_AUTHORIZED",
                            "not permitted to act in this world".to_owned(),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            next_sequence,
                            state.clock.as_ref(),
                            &instance_id.to_string(),
                        )
                        .await;
                        next_sequence += 1;
                        continue;
                    }
                    // ADR-025 "既知の迂回経路": a Transform targeting an
                    // entity that does not exist yet is an auto-create
                    // candidate. `spawn_hook_active` (checked inside the
                    // actor below) is a periodically refreshed cache and,
                    // on its own, leaves a window between a registration
                    // change and the next refresh, or fails open if a
                    // refresh's lookup errors. Close both by checking
                    // existence cheaply (`interest_views` is an `RwLock`
                    // read already used for interest filtering on this same
                    // hot path — no actor round trip, no I/O) and, only for
                    // the rare "does not exist yet" case, doing one live
                    // (uncached) registration lookup — the same lookup
                    // `SpawnEntity` already performs on every command. This
                    // adds no cost to ordinary movement of entities that
                    // already exist.
                    if let Some(registrations) = state.extension_registrations.as_ref() {
                        let exists = state
                            .registry
                            .interest_views(instance_id)
                            .is_some_and(|views| views.iter().any(|view| view.id == entity_id));
                        if !exists {
                            match registrations
                                .find_active_registration_by_capability(
                                    PreCommitOperation::Spawn.capability(),
                                )
                                .await
                            {
                                Ok(Some(_)) => {
                                    send_error(
                                        &mut socket,
                                        "EXPLICIT_SPAWN_REQUIRED",
                                        "an active spawn pre-commit hook is registered; \
                                         send SpawnEntity instead of relying on Transform \
                                         auto-create"
                                            .to_owned(),
                                        env.message_id.clone(),
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                    )
                                    .await;
                                    next_sequence += 1;
                                    continue;
                                }
                                Ok(None) => {}
                                Err(_) => {
                                    // Fail closed: a lookup failure must not
                                    // silently permit an unapproved
                                    // auto-create (ADR-025, symmetric with
                                    // SpawnEntity's own PRE_COMMIT_UNAVAILABLE
                                    // on the same failure).
                                    send_error(
                                        &mut socket,
                                        "PRE_COMMIT_UNAVAILABLE",
                                        "spawn pre-commit registration lookup failed"
                                            .to_owned(),
                                        env.message_id.clone(),
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                    )
                                    .await;
                                    next_sequence += 1;
                                    continue;
                                }
                            }
                        }
                    }
                    let outcome = if state.registry.contains(instance_id) {
                        state
                            .registry
                            .submit(
                                instance_id,
                                InstanceCommand::UpdateTransform {
                                    entity_id,
                                    transform,
                                    expected_revision: expected,
                                    user_id: user,
                                    now: now_ts,
                                    permissions,
                                },
                            )
                            .await
                    } else {
                        Ok(CommandOutcome::Rejected {
                            code: "NOT_FOUND",
                            detail: String::from("instance not found"),
                        })
                    };
                    let outcome = outcome.unwrap_or_else(mailbox_rejection);
                    match outcome {
                        CommandOutcome::Applied { revision, entity_revision, committed_entity } => {
                            // N-4: pin viewer position to the first entity moved by this
                            // connection. A future presence→avatar binding will make this
                            // permanent; this is the bridge.
                            if should_follow_viewer(viewer_entity, entity_id) {
                                viewer_entity = Some(entity_id);
                                let viewer_pos = transform.position();
                                delivery_sink.set_viewer_position(viewer_pos);
                                delivery_registration
                                    .update_viewer_cell(state.interest_grid.cell_for(viewer_pos));
                            }
                            let Some(entity) = committed_entity.filter(|entity| {
                                entity.id() == entity_id && Some(entity.revision()) == entity_revision
                            }) else {
                                send_error(&mut socket, "SYNC_CONTRACT_VIOLATION",
                                    "committed transform state is unavailable".to_owned(),
                                    message_id_for_payload.clone(), handshake.negotiated_minor,
                                    next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                break;
                            };
                            let delta = orbisync_protocol::v1::StateDelta {
                                from_revision: revision.as_u64().saturating_sub(1),
                                to_revision: revision.as_u64(),
                                entities: vec![committed_delta_entity(&entity)],
                            };
                            // M-7: broadcast without per-socket sequence; each receiver will set its own.
                            let env = Envelope {
                                protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                                protocol_minor: handshake.negotiated_minor,
                                message_id: uuid::Uuid::now_v7().to_string(),
                                sequence: 0,
                                sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                                instance_id: instance_id.to_string(),
                                payload: Some(envelope::Payload::StateDelta(delta)),
                            };
                            let mut buf = Vec::new();
                            if env.encode(&mut buf).is_ok() {
                                state.metrics_recorder.add(
                                    Counter::DeltaBytesTotal,
                                    u64::try_from(buf.len()).unwrap_or(u64::MAX),
                                );
                                // StateDelta is latest-wins: dropping on a full
                                // consumer channel is the documented behaviour.
                                let _outcome = state.delivery.broadcast_with_views(
                                    instance_id,
                                    buf,
                                    Reliability::LatestWins,
                                    Arc::new(std::iter::once(orbisync_world_runtime::actor::EntityInterestView::from_entity(&entity)).collect()),
                                    state.interest_grid,
                                );
                            }
                            state.metrics_recorder.observe(
                                Histogram::RealtimeApplicationDuration,
                                application_started.elapsed().as_secs_f64(),
                            );
                        }
                        CommandOutcome::Rejected { code, detail } => {
                            send_error(
                                &mut socket,
                                code,
                                detail,
                                message_id_for_payload.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            state.metrics_recorder.observe(
                                Histogram::RealtimeApplicationDuration,
                                application_started.elapsed().as_secs_f64(),
                            );
                            next_sequence += 1;
                        }
                    }
                    },
                    Some(envelope::Payload::EntityCommand(mut cmd)) => {
                        state.metrics_recorder.incr(Counter::InstanceCommandsTotal);
                        let original_generation_command = cmd.clone();
                        macro_rules! record_invalid_command {
                            ($detail:expr) => {{
                                let detail = $detail;
                                let durable = if state.generation.is_some() {
                                    crate::checkpoint_admission::reject_generation_command(&state, instance_id, &original_generation_command, detail.clone(), foreground_deadline).await.is_ok()
                                } else { persist_command_outcome(
                                    Arc::clone(&state),
                                    instance_id,
                                    command_dedup_reservation.take(),
                                    crate::command_dedup::CommandDedupResult::Rejected {
                                        code: "INVALID_ARGUMENT".to_owned(),
                                        detail: detail.clone(),
                                        message_id: uuid::Uuid::now_v7().to_string(),
                                    },
                                    state.clock.now(),
                                    None,
                                    foreground_deadline,
                                )
                                .await };
                                (detail, durable)
                            }};
                        }
                        let command_id_for_result = cmd.command_id.clone();
                        let mut operation = cmd.operation.to_lowercase();
                        let entity_id = match cmd.entity_id.parse::<EntityId>() {
                            Ok(id) => id,
                            Err(_) => {
                                let (detail, durable) = record_invalid_command!(
                                    "entity_id is not a valid id".to_owned()
                                );
                                send_error(
                                    &mut socket,
                                    if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                    if durable { detail } else { "command durability is unavailable".to_owned() },
                                    message_id_for_payload.clone(),
                                    handshake.negotiated_minor,
                                    next_sequence,
                                    state.clock.as_ref(),
                                    &instance_id.to_string(),
                                )
                                .await;
                                next_sequence += 1;
                                continue;
                            }
                        };
                        // ADR-026: consult the participation boundary before
                        // building the command, so it applies to spawn, update,
                        // delete and ownership transfer alike. Deliberately
                        // outside the pre-commit arm further down: that arm runs
                        // only when an active extension holds the matching
                        // capability, so a deployment with no hook registered
                        // would otherwise never reach this check.
                        if !ephemeral_participation_allowed(
                            state.ephemeral_scope.as_deref(),
                            user,
                            world_id.as_uuid(),
                            state.clock.now(),
                        )
                        .await
                        {
                            send_error(
                                &mut socket,
                                "NOT_AUTHORIZED",
                                "not permitted to act in this world".to_owned(),
                                message_id_for_payload.clone(),
                                handshake.negotiated_minor,
                                next_sequence,
                                state.clock.as_ref(),
                                &instance_id.to_string(),
                            )
                            .await;
                            next_sequence += 1;
                            continue;
                        }
                        // Intent shares the original command identity and dedup reservation.
                        // Only trusted server code can replace it with an authoritative update.
                        let mut input_observed = None;
                        let input_permissions = if operation == "input" {
                            Some(resolve_world_permissions(state.world_authorizer.as_ref(), user).await)
                        } else { None };
                        if let Some(live_permissions) = input_permissions {
                            let observed = tokio::time::timeout_at(foreground_deadline,
                                state.registry.read_entity(instance_id, entity_id)).await.ok().flatten();
                            let Some(observed) = observed else {
                                send_error_with_retryable(&mut socket, "PERSISTENCE_UNAVAILABLE",
                                    "input state is unavailable".into(), message_id_for_payload.clone(),
                                    handshake.negotiated_minor, next_sequence, state.clock.as_ref(),
                                    &instance_id.to_string(), true).await;
                                next_sequence += 1;
                                continue;
                            };
                            let participation_active = state.registry.is_present(instance_id, final_presence) == Some(true)
                                && is_session_still_active(state.identity_repository.as_deref(), session_id, state.clock.now()).await;
                            let computed = async {
                                if !participation_active { return Err("input participation is no longer active".into()); }
                                let entity = observed.as_ref().ok_or("input entity unavailable")?;
                                if !(live_permissions.entity_update_any || live_permissions.entity_update_own && entity.owner() == Some(user)) {
                                    return Err("entity update permission required".into());
                                }
                                if entity.revision().as_u64() != cmd.expected_revision {
                                    return Err("input entity revision mismatch".into());
                                }
                                let args = cmd.arguments.as_ref().ok_or("input arguments missing")?;
                                let name = extract_string_field(args, "rule").ok_or("input rule missing")?;
                                let rule = state.input_rules.get(&(world_id, name)).ok_or("input rule is not registered in this world")?;
                                let intent = match args.fields.get("intent").and_then(|v| v.kind.as_ref()) {
                                    Some(prost_types::value::Kind::StructValue(intent)) => intent,
                                    _ => return Err("input intent must be an object".into()),
                                };
                                let mut computed = rule.compute(crate::input::InputContext {
                                    world_id, instance_id, user_id: user, entity, now,
                                }, intent, command_id_for_dispatch.ok_or("input command id missing")?).await?;
                                computed.fields.insert("component_key".into(), prost_types::Value {
                                    kind: Some(prost_types::value::Kind::StringValue(rule.component_key().into())),
                                });
                                Ok::<prost_types::Struct, String>(computed)
                            };
                            let computed = tokio::time::timeout_at(foreground_deadline, computed).await
                                .unwrap_or_else(|_| Err("input computation timed out".into()));
                            match computed {
                                Ok(arguments) => {
                                    cmd.operation = "update".into();
                                    cmd.arguments = Some(arguments);
                                    operation = "update".into();
                                    input_observed = observed;
                                }
                                Err(detail) => {
                                    let (detail, durable) = record_invalid_command!(detail);
                                    send_error(&mut socket, if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                        detail, message_id_for_payload.clone(), handshake.negotiated_minor,
                                        next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                    next_sequence += 1;
                                    continue;
                                }
                            }
                        }
                        let expected_revision =
                            orbisync_domain::Revision::from_u64(cmd.expected_revision);
                        let mut transfer_new_owner = None;
                        let mut instance_command = match operation.as_str() {
                            "spawn" => {
                                let args = match cmd.arguments {
                                    Some(ref s) => s,
                                    None => {
                                        let (detail, durable) = record_invalid_command!(
                                            "arguments missing for spawn".to_owned()
                                        );
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                let kind = match parse_entity_kind(args) {
                                    Ok(k) => k,
                                    Err(e) => {
                                        let (detail, durable) = record_invalid_command!(e);
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                let visibility = match parse_visibility(args) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let (detail, durable) = record_invalid_command!(e);
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                let transform = match parse_transform_from_struct(args) {
                                    Ok(t) => t,
                                    Err(e) => {
                                        let (detail, durable) = record_invalid_command!(e);
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                InstanceCommand::SpawnEntity {
                                    command_id: command_id_for_dispatch,
                                    entity_id,
                                    kind,
                                    owner: Some(user),
                                    transform,
                                    visibility,
                                    requester: user,
                                    permissions,
                                }
                            }
                            "update" => {
                                let args = match cmd.arguments {
                                    Some(ref s) => s,
                                    None => {
                                        let (detail, durable) = record_invalid_command!(
                                            "arguments missing for update".to_owned()
                                        );
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                let component_key = extract_string_field(args, "component_key")
                                    .or_else(|| extract_string_field(args, "key"))
                                    .unwrap_or_else(|| "entity.component".to_owned());
                                if input_observed.is_none() && state.input_rules.iter().any(|((world, _), rule)|
                                    *world == world_id && rule.component_key() == component_key) {
                                    let (detail, durable) = record_invalid_command!("component is owned by a server input rule".to_owned());
                                    send_error(&mut socket, if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                        detail, message_id_for_payload.clone(), handshake.negotiated_minor,
                                        next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                    next_sequence += 1;
                                    continue;
                                }
                                let payload_bytes = struct_to_sorted_json_bytes(args);
                                InstanceCommand::UpdateEntityComponent {
                                    command_id: command_id_for_dispatch,
                                    entity_id,
                                    component_key,
                                    payload_bytes,
                                    expected_revision,
                                    now,
                                    requester: user,
                                    permissions,
                                }
                            }
                            "delete" => InstanceCommand::DeleteEntity {
                                command_id: command_id_for_dispatch,
                                entity_id,
                                expected_revision,
                                requester: user,
                                permissions,
                            },
                            "transfer_ownership" => {
                                let args = match cmd.arguments.as_ref() {
                                    Some(args) => args,
                                    None => {
                                        let (detail, durable) = record_invalid_command!(
                                            "arguments missing for transfer_ownership".to_owned()
                                        );
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                let new_owner = match parse_transfer_owner(args) {
                                    Ok(owner) => owner,
                                    Err(detail) => {
                                        let (detail, durable) = record_invalid_command!(detail);
                                        send_error(
                                            &mut socket,
                                            if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                            if durable { detail } else { "command durability is unavailable".to_owned() },
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                };
                                transfer_new_owner = Some(new_owner);
                                InstanceCommand::TransferOwnership {
                                    command_id: command_id_for_dispatch,
                                    entity_id,
                                    new_owner: Some(new_owner),
                                    expected_revision,
                                    now,
                                    requester: user,
                                    permissions,
                                }
                            }
                            _ => {
                                let (detail, durable) = record_invalid_command!(format!(
                                    "unknown operation: {}",
                                    cmd.operation
                                ));
                                send_error(
                                    &mut socket,
                                    if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                    if durable { detail } else { "command durability is unavailable".to_owned() },
                                    message_id_for_payload.clone(),
                                    handshake.negotiated_minor,
                                    next_sequence,
                                    state.clock.as_ref(),
                                    &instance_id.to_string(),
                                )
                                .await;
                                next_sequence += 1;
                                continue;
                            }
                        };
                        // ADR-025: opt-in, synchronous pre-commit validation
                        // hook. Runs before the durability guard below is
                        // acquired (an instance-scoped tokio::sync::Mutex) so
                        // the extension's HTTP round trip never holds a lock
                        // that would stall unrelated commands on the same
                        // instance. `UpdateTransform` never reaches this
                        // match arm and is out of scope by design (§2.3).
                        if let Some(pre_commit_operation) = match operation.as_str() {
                            "spawn" => Some(PreCommitOperation::Spawn),
                            "update" => Some(PreCommitOperation::Update),
                            "delete" => Some(PreCommitOperation::Delete),
                            _ => None,
                        } {
                            if let (Some(gate), Some(registrations)) = (
                                state.pre_commit_gate.as_ref(),
                                state.extension_registrations.as_ref(),
                            ) {
                                match registrations
                                    .find_active_registration_by_capability(
                                        pre_commit_operation.capability(),
                                    )
                                    .await
                                {
                                    Ok(Some(registration)) => {
                                        // Re-check the requester's live
                                        // permission through the real
                                        // authorizer immediately before
                                        // consulting the hook, rather than
                                        // reusing the connection-lifetime
                                        // `permissions` snapshot resolved at
                                        // handshake (ADR-025 "権限・所有・
                                        // 参加状態のwindow"). This also
                                        // avoids paying for an HTTP round
                                        // trip for a command that is no
                                        // longer authorized regardless of
                                        // the hook's answer.
                                        let pre_hook_permissions = resolve_world_permissions(
                                            state.world_authorizer.as_ref(),
                                            user,
                                        )
                                        .await;
                                        instance_command.set_permissions(pre_hook_permissions);
                                        // Re-check membership the same way:
                                        // a cheap cached-lock read (no actor
                                        // round trip), not the connection's
                                        // own handshake-time presence —
                                        // catches the case where this user
                                        // already left the instance before
                                        // this command was even admitted.
                                        if state
                                            .registry
                                            .is_present(instance_id, final_presence)
                                            != Some(true)
                                        {
                                            send_error(
                                                &mut socket,
                                                "PRESENCE_LOST",
                                                "no longer a member of this instance".to_owned(),
                                                message_id_for_payload.clone(),
                                                handshake.negotiated_minor,
                                                next_sequence,
                                                state.clock.as_ref(),
                                                &instance_id.to_string(),
                                            )
                                            .await;
                                            next_sequence += 1;
                                            continue;
                                        }
                                        // ADR-025 §9: re-check the ticket's
                                        // originating session the same way,
                                        // immediately before consulting the
                                        // hook. A `RealtimeTicketVerifier`
                                        // that does not track sessions
                                        // leaves this a no-op (see
                                        // `is_session_still_active`).
                                        if !is_session_still_active(
                                            state.identity_repository.as_deref(),
                                            session_id,
                                            state.clock.now(),
                                        )
                                        .await
                                        {
                                            send_error(
                                                &mut socket,
                                                "SESSION_REVOKED",
                                                "authentication session is no longer active"
                                                    .to_owned(),
                                                message_id_for_payload.clone(),
                                                handshake.negotiated_minor,
                                                next_sequence,
                                                state.clock.as_ref(),
                                                &instance_id.to_string(),
                                            )
                                            .await;
                                            next_sequence += 1;
                                            continue;
                                        }
                                        // Read on the actor task, without holding the durability
                                        // mutex across either this read or the external request.
                                        let observed = tokio::time::timeout_at(
                                            foreground_deadline,
                                            state.registry.read_entity(instance_id, entity_id),
                                        ).await.ok().flatten();
                                        let validation_error = match &observed {
                                            None => Some(("PRE_COMMIT_UNAVAILABLE", "entity state is unavailable")),
                                            Some(None) if operation != "spawn" => Some(("ENTITY_NOT_FOUND", "entity not found")),
                                            Some(Some(_)) if operation == "spawn" => Some(("ENTITY_EXISTS", "entity already exists")),
                                            Some(Some(entity)) if !(pre_hook_permissions.entity_update_any
                                                || pre_hook_permissions.entity_update_own && entity.owner() == Some(user)) =>
                                                Some(("NOT_OWNER", "entity update permission required")),
                                            Some(Some(entity)) if entity.revision() != expected_revision =>
                                                Some(("REVISION_MISMATCH", "entity changed before pre-commit validation")),
                                            Some(None) if !pre_hook_permissions.entity_spawn =>
                                                Some(("PERMISSION_DENIED", "entity spawn permission required")),
                                            _ => None,
                                        };
                                        if let Some((code, detail)) = validation_error {
                                            send_error(
                                                &mut socket, code, detail.to_owned(),
                                                message_id_for_payload.clone(), handshake.negotiated_minor,
                                                next_sequence, state.clock.as_ref(), &instance_id.to_string(),
                                            ).await;
                                            next_sequence += 1;
                                            continue;
                                        }
                                        let observed_entity = observed.flatten();
                                        let current_entity = observed_entity.as_ref().map(|entity| {
                                            let components = entity.components().iter()
                                                .filter(|(key, _)| !key.starts_with("core."))
                                                .map(|(key, bytes)| {
                                                    let value = match serde_json::from_slice::<serde_json::Value>(bytes) {
                                                        Ok(value) => serde_json::json!({"encoding": "json", "value": value}),
                                                        Err(_) => serde_json::json!({"encoding": "base64", "value": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)}),
                                                    };
                                                    (key.clone(), value)
                                                }).collect::<serde_json::Map<_, _>>();
                                            serde_json::json!({
                                                "entity_id": entity.id().to_string(),
                                                "revision": entity.revision().as_u64(),
                                                "owner_id": entity.owner().map(|id| id.to_string()),
                                                "components": components,
                                            })
                                        });
                                        let payload = cmd
                                            .arguments
                                            .as_ref()
                                            .map(struct_to_sorted_json_value)
                                            .unwrap_or(serde_json::Value::Null);
                                        let component_key = if operation == "update" {
                                            cmd.arguments.as_ref().map(|args| {
                                                extract_string_field(args, "component_key")
                                                    .or_else(|| extract_string_field(args, "key"))
                                                    .unwrap_or_else(|| {
                                                        "entity.component".to_owned()
                                                    })
                                            })
                                        } else {
                                            None
                                        };
                                        // The client's own asserted revision, forwarded
                                        // unchanged — not a value Core observed by reading
                                        // current state. `None` for spawn (no revision
                                        // field exists on `SpawnEntity`). See ADR-025
                                        // "revisionの束縛と限界".
                                        let client_expected_revision = (operation != "spawn")
                                            .then_some(expected_revision.as_u64());
                                        let request = PreCommitValidationRequest {
                                            // Guaranteed `Some`: this branch
                                            // only runs for EntityCommand
                                            // payloads, and every non-Leader
                                            // dedup admission above already
                                            // continued the loop before
                                            // reaching here.
                                            request_id: command_id_for_dispatch.unwrap_or_else(
                                                orbisync_domain::CommandId::generate,
                                            ),
                                            instance_id,
                                            entity_id,
                                            operation: pre_commit_operation,
                                            requester: user,
                                            client_expected_revision,
                                            current_entity,
                                            component_key,
                                            payload,
                                        };
                                        // The external hook must not serialize unrelated
                                        // publications or snapshot reads for this instance.
                                        // Reacquire before authorization and observed-state
                                        // checks, retaining the commit/publication barrier.
                                        drop(_publication_guard.take());
                                        let decision = gate.validate(&registration, &request).await;
                                        _publication_guard = Some(publication_lock.lock().await);
                                        match decision {
                                            PreCommitDecision::Deny { reason } => {
                                                send_error(
                                                    &mut socket,
                                                    "PRE_COMMIT_DENIED",
                                                    reason,
                                                    message_id_for_payload.clone(),
                                                    handshake.negotiated_minor,
                                                    next_sequence,
                                                    state.clock.as_ref(),
                                                    &instance_id.to_string(),
                                                )
                                                .await;
                                                next_sequence += 1;
                                                continue;
                                            }
                                            PreCommitDecision::Allow => {
                                                // Re-check again after the
                                                // hook's wait: a grant that
                                                // was still valid immediately
                                                // before the HTTP call may no
                                                // longer be valid by the time
                                                // the response comes back.
                                                // The actor's own ownership
                                                // check still applies on top
                                                // of this — this only refreshes
                                                // the snapshot it checks
                                                // against.
                                                let post_hook_permissions =
                                                    resolve_world_permissions(
                                                        state.world_authorizer.as_ref(),
                                                        user,
                                                    )
                                                    .await;
                                                instance_command
                                                    .set_permissions(post_hook_permissions);
                                                if state
                                                    .registry
                                                    .is_present(instance_id, final_presence)
                                                    != Some(true)
                                                {
                                                    send_error(
                                                        &mut socket,
                                                        "PRESENCE_LOST",
                                                        "no longer a member of this instance"
                                                            .to_owned(),
                                                        message_id_for_payload.clone(),
                                                        handshake.negotiated_minor,
                                                        next_sequence,
                                                        state.clock.as_ref(),
                                                        &instance_id.to_string(),
                                                    )
                                                    .await;
                                                    next_sequence += 1;
                                                    continue;
                                                }
                                                if !is_session_still_active(
                                                    state.identity_repository.as_deref(),
                                                    session_id,
                                                    state.clock.now(),
                                                )
                                                .await
                                                {
                                                    send_error(
                                                        &mut socket,
                                                        "SESSION_REVOKED",
                                                        "authentication session is no longer active"
                                                            .to_owned(),
                                                        message_id_for_payload.clone(),
                                                        handshake.negotiated_minor,
                                                        next_sequence,
                                                        state.clock.as_ref(),
                                                        &instance_id.to_string(),
                                                    )
                                                    .await;
                                                    next_sequence += 1;
                                                    continue;
                                                }
                                            }
                                        }
                                        instance_command = InstanceCommand::WithExpectedEntityState {
                                            entity_id,
                                            expected: observed_entity.map(Box::new),
                                            command: Box::new(instance_command),
                                        };
                                    }
                                    Ok(None) => {}
                                    Err(_) => {
                                        // Fail closed: a registration lookup
                                        // failure must not silently bypass an
                                        // opted-in hook (ADR-025).
                                        send_error(
                                            &mut socket,
                                            "PRE_COMMIT_UNAVAILABLE",
                                            "pre-commit validation registration lookup failed"
                                                .to_owned(),
                                            message_id_for_payload.clone(),
                                            handshake.negotiated_minor,
                                            next_sequence,
                                            state.clock.as_ref(),
                                            &instance_id.to_string(),
                                        )
                                        .await;
                                        next_sequence += 1;
                                        continue;
                                    }
                                }
                            }
                        }
                        if let Some(observed) = input_observed {
                            // External computation may outlive participation or authentication.
                            if state.registry.is_present(instance_id, final_presence) != Some(true)
                                || !is_session_still_active(state.identity_repository.as_deref(), session_id, state.clock.now()).await
                                || !ephemeral_participation_allowed(state.ephemeral_scope.as_deref(), user, world_id.as_uuid(), state.clock.now()).await {
                                let (detail, durable) = record_invalid_command!("input participation is no longer active".to_owned());
                                send_error(&mut socket, if durable { "INVALID_ARGUMENT" } else { "PERSISTENCE_UNAVAILABLE" },
                                    detail, message_id_for_payload.clone(), handshake.negotiated_minor,
                                    next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                next_sequence += 1;
                                continue;
                            }
                            // Preserve the computation's original read guard across precommit.
                            // Nested guards are not supported by the actor.
                            if let InstanceCommand::WithExpectedEntityState { command, .. } = instance_command {
                                instance_command = *command;
                            }
                            instance_command.set_permissions(resolve_world_permissions(state.world_authorizer.as_ref(), user).await);
                            instance_command = InstanceCommand::WithExpectedEntityState {
                                entity_id, expected: Some(Box::new(observed)), command: Box::new(instance_command),
                            };
                        }
                        if let Some(service) = &state.generation {
                            let result = crate::checkpoint_admission::submit_generation_command(
                                &state.registry, instance_id, instance_command, cmd.clone(),
                                crate::command_dedup::CommandDedupStore::fingerprint(&original_generation_command),
                                uuid::Uuid::now_v7(), state.clock.now().to_unix_millis().unwrap_or(0),
                                Arc::new(AtomicBool::new(false)),
                            ).await;
                            let result = match result {
                                Ok(result) => result,
                                Err(_) => {
                                    send_error_with_retryable(&mut socket, "PERSISTENCE_UNAVAILABLE", "generation admission unavailable".into(), message_id_for_payload.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string(), true).await;
                                    next_sequence += 1;
                                    continue;
                                }
                            };
                            let completed = result.phase == orbisync_application::checkpoint_admission::AdmissionPhase::Completed;
                            if completed {
                                if let (CommandOutcome::Applied { committed_entity, revision, .. }, Some(receipt)) = (&result.outcome, &result.receipt) {
                                    if let orbisync_world_runtime::CheckpointDedupResult::Applied { response_payload } = &receipt.result {
                                        let Ok(command) = orbisync_protocol::v1::EntityCommand::decode(response_payload.as_slice()) else { break; };
                                        let response = crate::command_dedup::CommandDedupResult::Applied { command, message_id: receipt.message_id.clone() };
                                        let views = Arc::new(committed_entity.as_deref().map(orbisync_world_runtime::actor::EntityInterestView::from_entity).into_iter().collect());
                                        let publication = CommandPublication { negotiated_minor: handshake.negotiated_minor, views, revision: revision.as_u64() };
                                        let state = Arc::downgrade(&state);
                                        service.retain_publication(instance_id, receipt.command_id.clone(), move || {
                                            if let Some(state) = state.upgrade() {
                                                tokio::spawn(async move { publication.publish(&state, instance_id, &response).await; });
                                            }
                                        });
                                    }
                                }
                            }
                            let dirty_replay = matches!(&result.outcome, CommandOutcome::Rejected { code: "PERSISTENCE_UNAVAILABLE", .. }) && result.receipt.is_some();
                            if completed || dirty_replay {
                                let durable = match state.registry.handle(instance_id) {
                                    Some(handle) => matches!(tokio::time::timeout_at(foreground_deadline, service.persist(handle, state.clock.now())).await, Ok(Ok(()))),
                                    None => false,
                                };
                                if !durable {
                                    send_error_with_retryable(&mut socket, "PERSISTENCE_UNAVAILABLE", "generation outcome awaiting durability".into(), message_id_for_payload.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string(), true).await;
                                    next_sequence += 1;
                                    continue;
                                }
                            }
                            if let Some(receipt) = result.receipt {
                                let response = match receipt.result {
                                    orbisync_world_runtime::CheckpointDedupResult::Applied { response_payload } => {
                                        let Ok(command) = orbisync_protocol::v1::EntityCommand::decode(response_payload.as_slice()) else { break; };
                                        crate::command_dedup::CommandDedupResult::Applied { command, message_id: receipt.message_id }
                                    }
                                    orbisync_world_runtime::CheckpointDedupResult::Rejected { code, detail } => crate::command_dedup::CommandDedupResult::Rejected { code, detail, message_id: receipt.message_id },
                                };
                                if completed {
                                    if !matches!(result.outcome, CommandOutcome::Applied { .. }) {
                                        send_command_result(&mut socket, &response, message_id_for_payload.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                        next_sequence += 1;
                                    }
                                } else if result.phase == orbisync_application::checkpoint_admission::AdmissionPhase::Replay {
                                    send_command_result(&mut socket, &response, message_id_for_payload.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string()).await;
                                    next_sequence += 1;
                                }
                            } else if let CommandOutcome::Rejected { code, detail } = result.outcome {
                                let retryable = result.phase == orbisync_application::checkpoint_admission::AdmissionPhase::NotAdmitted && code != "COMMAND_BEFORE_BASELINE";
                                send_error_with_retryable(&mut socket, code, detail, message_id_for_payload.clone(), handshake.negotiated_minor, next_sequence, state.clock.as_ref(), &instance_id.to_string(), retryable).await;
                                next_sequence += 1;
                            }
                            continue;
                        }
                        let command_durability_guard = if state.checkpoint_store.is_some() {
                            let Some(guard) =
                                acquire_command_durability_guard(
                                    &state,
                                    instance_id,
                                    foreground_deadline,
                                )
                                .await
                            else {
                                // The reservation has not reached the actor yet, so dropping it
                                // safely releases admission and lets the client retry.
                                send_error_with_retryable(
                                    &mut socket,
                                    "PERSISTENCE_UNAVAILABLE",
                                    "command durability guard was unavailable before its deadline"
                                        .to_owned(),
                                    message_id_for_payload.clone(),
                                    handshake.negotiated_minor,
                                    next_sequence,
                                    state.clock.as_ref(),
                                    &instance_id.to_string(),
                                    true,
                                )
                                .await;
                                next_sequence += 1;
                                continue;
                            };
                            Some(guard)
                        } else {
                            None
                        };
                        let pre_transfer_owner = if operation == "transfer_ownership" {
                            state.registry.read_snapshot(instance_id).await.and_then(|read| {
                                read.entities
                                    .into_iter()
                                    .find(|entity| entity.id() == entity_id)
                                    .and_then(|entity| entity.owner())
                            })
                        } else {
                            None
                        };
                        let outcome = state
                            .registry
                            .submit(instance_id, instance_command)
                            .await;
                        let outcome = outcome.unwrap_or_else(mailbox_rejection);
                        let reservation = command_dedup_reservation.take();
                        match outcome {
                            CommandOutcome::Applied { revision, entity_revision, committed_entity } => {
                                let committed_view = committed_entity.as_deref().map(orbisync_world_runtime::actor::EntityInterestView::from_entity);
                                // D-15: broadcast EntityCommand (reliable) instead of StateDelta (latest-wins).
                                // StateDelta would be dropped on `try_send` overflow (delivery.rs:62) and,
                                // without Resume/Resync, the entity creation/deletion would be permanently lost.
                                // W-17 will classify EntityCommand as reliable.
                                let broadcast_cmd = if operation == "delete" {
                                    let mut fields = std::collections::BTreeMap::new();
                                    if let Some(view) = committed_view.clone() {
                                        match view.visibility {
                                            VisibilityPolicy::Global => {
                                                fields.insert(
                                                    "visibility".to_owned(),
                                                    prost_types::Value {
                                                        kind: Some(prost_types::value::Kind::StringValue(
                                                            "global".to_owned(),
                                                        )),
                                                    },
                                                );
                                            }
                                            VisibilityPolicy::OwnerOnly => {
                                                fields.insert(
                                                    "visibility".to_owned(),
                                                    prost_types::Value {
                                                        kind: Some(prost_types::value::Kind::StringValue(
                                                            "owner_only".to_owned(),
                                                        )),
                                                    },
                                                );
                                            }
                                            VisibilityPolicy::Spatial { radius } => {
                                                fields.insert(
                                                    "visibility".to_owned(),
                                                    prost_types::Value {
                                                        kind: Some(prost_types::value::Kind::StringValue(
                                                            "spatial".to_owned(),
                                                        )),
                                                    },
                                                );
                                                fields.insert(
                                                    "visibility_radius".to_owned(),
                                                    prost_types::Value {
                                                        kind: Some(prost_types::value::Kind::NumberValue(
                                                            radius as f64,
                                                        )),
                                                    },
                                                );
                                            }
                                            VisibilityPolicy::RoleRestricted { .. } => {
                                                fields.insert("visibility".to_owned(), prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::StringValue("role_restricted".to_owned())),
                                                });
                                            }
                                            VisibilityPolicy::Explicit { .. } => {
                                                fields.insert("visibility".to_owned(), prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::StringValue("explicit".to_owned())),
                                                });
                                            }
                                            VisibilityPolicy::Custom { tag } => {
                                                fields.insert("visibility".to_owned(), prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::StringValue(format!("custom:{tag:?}"))),
                                                });
                                            }
                                        }
                                        if let Some(pos) = view.position {
                                            fields.insert(
                                                "position_x".to_owned(),
                                                prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::NumberValue(f64::from(pos.x()))),
                                                },
                                            );
                                            fields.insert(
                                                "position_y".to_owned(),
                                                prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::NumberValue(f64::from(pos.y()))),
                                                },
                                            );
                                            fields.insert(
                                                "position_z".to_owned(),
                                                prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::NumberValue(f64::from(pos.z()))),
                                                },
                                            );
                                        }
                                        if let Some(owner) = view.owner {
                                            fields.insert(
                                                "owner".to_owned(),
                                                prost_types::Value {
                                                    kind: Some(prost_types::value::Kind::StringValue(
                                                        owner.to_string(),
                                                    )),
                                                },
                                            );
                                        }
                                    }
                                    orbisync_protocol::v1::EntityCommand {
                                        command_id: command_id_for_result.clone(),
                                        entity_id: entity_id.to_string(),
                                        expected_revision: revision.as_u64(),
                                        operation: "delete".to_owned(),
                                        arguments: Some(prost_types::Struct { fields }),
                                        instance_revision: Some(revision.as_u64()),
                                    }
                                } else if operation == "spawn" {
                                    let Some(new_rev) = entity_revision else {
                                        // Never acknowledge guessed or later state as this commit.
                                        break;
                                    };
                                    orbisync_protocol::v1::EntityCommand {
                                        command_id: command_id_for_result.clone(),
                                        entity_id: entity_id.to_string(),
                                        expected_revision: new_rev.as_u64(),
                                        operation: "spawn".to_owned(),
                                        arguments: cmd.arguments.clone(),
                                        instance_revision: Some(revision.as_u64()),
                                    }
                                } else if operation == "update" {
                                    let Some(new_rev) = entity_revision else {
                                        break;
                                    };
                                    orbisync_protocol::v1::EntityCommand {
                                        command_id: command_id_for_result.clone(),
                                        entity_id: entity_id.to_string(),
                                        expected_revision: new_rev.as_u64(),
                                        operation: "update".to_owned(),
                                        arguments: cmd.arguments.clone(),
                                        instance_revision: Some(revision.as_u64()),
                                    }
                                } else if operation == "transfer_ownership" {
                                    let mut fields = std::collections::BTreeMap::new();
                                    if let Some(previous_owner) = pre_transfer_owner {
                                        fields.insert(
                                            "previous_owner_id".to_owned(),
                                            prost_types::Value {
                                                kind: Some(prost_types::value::Kind::StringValue(
                                                    previous_owner.to_string(),
                                                )),
                                            },
                                        );
                                    }
                                    if let Some(new_owner) = transfer_new_owner {
                                        fields.insert(
                                            "new_owner_id".to_owned(),
                                            prost_types::Value {
                                                kind: Some(prost_types::value::Kind::StringValue(
                                                    new_owner.to_string(),
                                                )),
                                            },
                                        );
                                    }
                                    orbisync_protocol::v1::EntityCommand {
                                        command_id: command_id_for_result.clone(),
                                        entity_id: entity_id.to_string(),
                                        expected_revision: entity_revision
                                            .map_or(revision.as_u64(), |value| value.as_u64()),
                                        operation: "transfer_ownership".to_owned(),
                                        arguments: Some(prost_types::Struct { fields }),
                                        instance_revision: Some(revision.as_u64()),
                                    }
                                } else {
                                    continue;
                                };
                                let replay_message_id = uuid::Uuid::now_v7().to_string();
                                let result = crate::command_dedup::CommandDedupResult::Applied {
                                    command: broadcast_cmd.clone(),
                                    message_id: replay_message_id.clone(),
                                };
                                let durable = persist_command_outcome_with_publication(
                                    Arc::clone(&state),
                                    instance_id,
                                    reservation,
                                    result,
                                    state.clock.now(),
                                    command_durability_guard,
                                    foreground_deadline,
                                    Some(CommandPublication { negotiated_minor: handshake.negotiated_minor,
                                        views: Arc::new(committed_view.into_iter().collect()), revision: revision.as_u64() }),
                                )
                                .await;
                                if !durable {
                                    send_error_with_retryable(
                                        &mut socket,
                                        "PERSISTENCE_UNAVAILABLE",
                                        "command applied locally; durability recovery is in progress".to_owned(),
                                        message_id_for_payload.clone(),
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                        true,
                                    )
                                    .await;
                                    next_sequence += 1;
                                    continue;
                                }
                            }
                            CommandOutcome::Rejected { code, detail } => {
                                let replay_message_id = uuid::Uuid::now_v7().to_string();
                                let result = crate::command_dedup::CommandDedupResult::Rejected {
                                    code: code.to_owned(),
                                    detail: detail.clone(),
                                    message_id: replay_message_id.clone(),
                                };
                                let durable = persist_command_outcome(
                                    Arc::clone(&state),
                                    instance_id,
                                    reservation,
                                    result,
                                    state.clock.now(),
                                    command_durability_guard,
                                    foreground_deadline,
                                )
                                .await;
                                if !durable {
                                    send_error_with_retryable(
                                        &mut socket,
                                        "PERSISTENCE_UNAVAILABLE",
                                        "command result is awaiting durability recovery".to_owned(),
                                        message_id_for_payload.clone(),
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                        true,
                                    )
                                    .await;
                                    next_sequence += 1;
                                    continue;
                                }
                                send_error_with_message_id(
                                    &mut socket,
                                    code,
                                    detail,
                                    message_id_for_payload.clone(),
                                    replay_message_id,
                                    handshake.negotiated_minor,
                                    next_sequence,
                                    state.clock.as_ref(),
                                    &instance_id.to_string(),
                                    false,
                                )
                                .await;
                                next_sequence += 1;
                            }
                        }
                    }
                    Some(envelope::Payload::ClientHello(_))
                    | Some(envelope::Payload::ServerHello(_))
                    | Some(envelope::Payload::JoinInstance(_))
                    | Some(envelope::Payload::JoinAccepted(_))
                    | Some(envelope::Payload::Snapshot(_))
                    | Some(envelope::Payload::StateDelta(_))
                    | Some(envelope::Payload::Heartbeat(_))
                    | Some(envelope::Payload::HeartbeatAck(_))
                    | Some(envelope::Payload::ResumeSession(_))
                    | Some(envelope::Payload::ResumeAccepted(_))
                    | Some(envelope::Payload::ResyncRequired(_))
                    | Some(envelope::Payload::Error(_))
                    | None => {
                        send_error(&mut socket, "INVALID_MESSAGE",
                            "message type is not accepted on an active connection".to_owned(),
                            message_id_for_payload, handshake.negotiated_minor, next_sequence,
                            state.clock.as_ref(), &instance_id.to_string()).await;
                        next_sequence += 1;
                    }
                }
            }
            _ = delivery_sink.notified() => {
                if delivery_sink.is_closed() {
                    break;
                }
                let capacity = outbound_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .capacity();
                let batch_limit = capacity.clamp(1, 32);
                let mut socket_failed = false;
                while !outbound_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty()
                {
                    let batch = outbound_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .drain_batch(batch_limit);
                    for item in batch {
                        let mut env = match delivery_sink.prepare_delivery(&item) {
                            Some(e) => e,
                            None => continue,
                        };
                        env.sequence = next_sequence;
                        next_sequence += 1;
                        env.sent_at_unix_ms = state.clock.now().to_unix_millis().unwrap_or(0);
                        let mut buf = Vec::new();
                        if env.encode(&mut buf).is_ok() {
                            if socket.write(Message::Binary(buf.into())).await.is_err() {
                                if let Some(violation) = socket.take_size_violation() {
                                    send_outbound_size_error(
                                        &mut socket,
                                        violation,
                                        env.message_id,
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                    )
                                    .await;
                                    next_sequence += 1;
                                }
                                socket_failed = true;
                                break;
                            }
                        }
                    }
                    if socket_failed {
                        break;
                    }
                }
                if socket_failed {
                    break;
                }
                delivery_sink.finish_snapshot();
            }
            _ = heartbeat_interval.tick() => {
                let now = state.clock.now();
                if heartbeat.is_timed_out(now) {
                    tracing::warn!(
                        event = "realtime.heartbeat_timeout",
                        instance_id = %instance_id,
                        "heartbeat timeout on tick – closing connection"
                    );
                    // Reason: heartbeat timeout always closes the connection (break); the state transition is validated but the resulting Option is not used because the task terminates (A-3 allow with reason).
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = heartbeat.next_state_on_timeout(conn_state, now);
                    break;
                }
                if heartbeat.should_send_heartbeat(now) {
                    let now_millis = now.to_unix_millis().unwrap_or(0);
                    let hb_env = heartbeat_envelope(
                        handshake.negotiated_minor,
                        uuid::Uuid::now_v7().to_string(),
                        next_sequence,
                        now_millis,
                        instance_id.to_string(),
                        now_millis,
                    );
                    next_sequence += 1;
                    let mut buf = Vec::new();
                    if hb_env.encode(&mut buf).is_ok() {
                        if socket.write(Message::Binary(buf.into())).await.is_err() {
                            break;
                        }
                    }
                    heartbeat.mark_heartbeat_sent(now);
                }
                // Opportunistically flush any pending outbound queue items so a burst
                // that left remainder due to batch_limit does not wait 20s for next tick.
                let queue_has_items = !outbound_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
                if queue_has_items {
                    let capacity = outbound_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .capacity();
                    let batch_limit = capacity.clamp(1, 32);
                    let batch = outbound_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .drain_batch(batch_limit);
                    let mut socket_failed = false;
                    for item in batch {
                        let mut env = match delivery_sink.prepare_delivery(&item) {
                            Some(e) => e,
                            None => continue,
                        };
                        env.sequence = next_sequence;
                        next_sequence += 1;
                        env.sent_at_unix_ms = state.clock.now().to_unix_millis().unwrap_or(0);
                        let mut buf = Vec::new();
                        if env.encode(&mut buf).is_ok() {
                            if socket.write(Message::Binary(buf.into())).await.is_err() {
                                if let Some(violation) = socket.take_size_violation() {
                                    send_outbound_size_error(
                                        &mut socket,
                                        violation,
                                        env.message_id,
                                        handshake.negotiated_minor,
                                        next_sequence,
                                        state.clock.as_ref(),
                                        &instance_id.to_string(),
                                    )
                                    .await;
                                    next_sequence += 1;
                                }
                                socket_failed = true;
                                break;
                            }
                        }
                    }
                    if socket_failed {
                        break;
                    }
                }
                delivery_sink.finish_snapshot();
            }
        }
    }
    // Leave + cleanup_closed are handled by PresenceGuard Drop (including panic).
    // Drop the registration before the guard so delivery cannot target a closed
    // connection while its presence is being reclaimed.
    drop(delivery_registration);
    drop(_presence_guard);
    // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = socket.write(Message::Close(None)).await;
}

#[cfg(test)]
mod durability_deadline_tests {
    use super::{RealtimeState, acquire_command_durability_guard, persist_command_outcome};
    use crate::command_dedup::{CommandDedupAdmission, CommandDedupResult};
    use crate::delivery::DeliveryRegistry;
    use async_trait::async_trait;
    use orbisync_application::{
        AppCheckpoint, ApplicationError, ApplicationErrorKind, CheckpointStore,
    };
    use orbisync_domain::{Clock, CommandId, InstanceId, SystemClock, Timestamp};
    use orbisync_world_runtime::RuntimeRegistry;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    struct UncalledCheckpointStore;

    #[async_trait]
    impl CheckpointStore for UncalledCheckpointStore {
        async fn save_checkpoint(
            &self,
            _checkpoint: AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            unreachable!("durability guard should time out before the store is called");
        }

        async fn load_latest(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Option<AppCheckpoint>, ApplicationError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn same_instance_durability_wait_is_bounded_in_foreground_and_background() {
        let registry = Arc::new(RuntimeRegistry::new());
        let delivery = Arc::new(DeliveryRegistry::new());
        let clock = Arc::new(SystemClock::new());
        let mut raw_state = RealtimeState::new(
            orbisync_config::Config::default().realtime,
            registry,
            delivery,
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        raw_state.checkpoint_store = Some(Arc::new(UncalledCheckpointStore));
        let state = Arc::new(raw_state);
        let instance_id = InstanceId::generate();
        let command_id = CommandId::generate();
        let fingerprint = [7; 32];
        let reservation = match state
            .command_dedup
            .begin(instance_id, command_id, fingerprint, 0)
        {
            CommandDedupAdmission::Leader(reservation) => reservation,
            other => unreachable!("unexpected admission: {other:?}"),
        };
        let result = CommandDedupResult::Rejected {
            code: String::from("PERSISTENCE_UNAVAILABLE"),
            detail: String::from("test outcome"),
            message_id: command_id.to_string(),
        };

        // Hold the instance-local fence for the complete foreground and
        // background windows. A different-instance lock would incorrectly
        // allow the worker to reach the store instead of returning boundedly.
        let held_guard = state.command_dedup.durability_guard(instance_id).await;
        let foreground_deadline =
            tokio::time::Instant::now() + super::PERSISTENCE_FOREGROUND_DEADLINE;
        let foreground_started = Instant::now();
        let foreground_result = persist_command_outcome(
            Arc::clone(&state),
            instance_id,
            Some(reservation),
            result,
            Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp"),
            None,
            foreground_deadline,
        )
        .await;
        assert!(
            !foreground_result,
            "foreground must map to PERSISTENCE_UNAVAILABLE"
        );
        assert!(
            foreground_started.elapsed() < Duration::from_secs(3),
            "foreground durability wait exceeded its finite deadline"
        );

        tokio::time::timeout(
            Duration::from_secs(7),
            state.drain_recovery(Duration::from_secs(6)),
        )
        .await
        .expect("background durability wait must also be finite");
        assert!(matches!(
            state
                .command_dedup
                .begin(instance_id, command_id, fingerprint, 1),
            CommandDedupAdmission::PersistenceUnavailable
        ));
        drop(held_guard);
    }

    #[tokio::test]
    async fn foreground_guard_and_persistence_share_one_deadline() {
        let instance_id = InstanceId::generate();
        let registry = Arc::new(RuntimeRegistry::new());
        registry.ensure_instance(orbisync_world_runtime::actor::InstanceActor::new(
            orbisync_world_runtime::InstanceRuntimeDescriptor {
                instance_id,
                state: orbisync_world_runtime::RuntimeState::Running,
                revision: orbisync_domain::Revision::INITIAL,
            },
        ));
        let store = Arc::new(CommitThenHangCheckpointStore::default());
        let mut raw_state = RealtimeState::new(
            orbisync_config::Config::default().realtime,
            registry,
            Arc::new(DeliveryRegistry::new()),
            Arc::new(SystemClock::new()),
        );
        raw_state.checkpoint_store = Some(store);
        let state = Arc::new(raw_state);
        let command_id = CommandId::generate();
        let fingerprint = [8; 32];
        let reservation =
            match state
                .command_dedup
                .begin(instance_id, command_id, fingerprint, 1_000)
            {
                CommandDedupAdmission::Leader(reservation) => reservation,
                other => unreachable!("unexpected admission: {other:?}"),
            };
        let result = CommandDedupResult::Rejected {
            code: String::from("CONFLICT"),
            detail: String::from("test result"),
            message_id: command_id.to_string(),
        };
        let held_guard = state.command_dedup.durability_guard(instance_id).await;
        let foreground_deadline =
            tokio::time::Instant::now() + super::PERSISTENCE_FOREGROUND_DEADLINE;
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(held_guard);
        });
        let started = Instant::now();
        let Some(guard) =
            acquire_command_durability_guard(&state, instance_id, foreground_deadline).await
        else {
            unreachable!("guard should be released before the shared deadline expires");
        };
        let durable = persist_command_outcome(
            Arc::clone(&state),
            instance_id,
            Some(reservation),
            result,
            Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp"),
            Some(guard),
            foreground_deadline,
        )
        .await;
        assert!(!durable, "blocked persistence must become unavailable");
        assert!(
            started.elapsed() < Duration::from_millis(2_600),
            "guard acquisition plus foreground persistence exceeded one deadline"
        );
        release.await.expect("guard release task");
        state.drain_recovery(Duration::from_secs(1)).await;
    }

    #[derive(Default)]
    struct CommitThenHangCheckpointStore {
        first_saved: Mutex<Option<AppCheckpoint>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CheckpointStore for CommitThenHangCheckpointStore {
        async fn save_checkpoint(
            &self,
            checkpoint: AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                *self.first_saved.lock().expect("checkpoint lock") = Some(checkpoint);
                // Model a durable commit whose acknowledgement is lost after
                // the store has accepted the checkpoint.
                std::future::pending::<()>().await;
            }
            let saved = self
                .first_saved
                .lock()
                .expect("checkpoint lock")
                .clone()
                .expect("first save must be retained");
            let checkpoint = orbisync_world_runtime::Checkpoint::from_json_bytes(&saved.payload)
                .map_err(|error| {
                    ApplicationError::new(ApplicationErrorKind::PortFailure, error.to_string())
                })?;
            Ok(checkpoint
                .dedup
                .into_iter()
                .map(|entry| orbisync_application::CheckpointSaveReceipt {
                    command_id: entry.command_id,
                    created_at_millis: entry.created_at_millis,
                    expires_at_millis: entry.expires_at_millis,
                })
                .collect())
        }

        async fn load_latest(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Option<AppCheckpoint>, ApplicationError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn timeout_retry_keeps_first_durable_timestamp_in_local_replay() {
        let instance_id = InstanceId::generate();
        let registry = Arc::new(RuntimeRegistry::new());
        registry.ensure_instance(orbisync_world_runtime::actor::InstanceActor::new(
            orbisync_world_runtime::InstanceRuntimeDescriptor {
                instance_id,
                state: orbisync_world_runtime::RuntimeState::Running,
                revision: orbisync_domain::Revision::INITIAL,
            },
        ));
        let store = Arc::new(CommitThenHangCheckpointStore::default());
        let mut raw_state = RealtimeState::new(
            orbisync_config::Config::default().realtime,
            registry,
            Arc::new(DeliveryRegistry::new()),
            Arc::new(SystemClock::new()),
        );
        raw_state.checkpoint_store = Some(store.clone());
        let state = Arc::new(raw_state);
        let command_id = CommandId::generate();
        let fingerprint = [9; 32];
        let reservation =
            match state
                .command_dedup
                .begin(instance_id, command_id, fingerprint, 1_000)
            {
                CommandDedupAdmission::Leader(reservation) => reservation,
                other => unreachable!("unexpected admission: {other:?}"),
            };
        let result = CommandDedupResult::Rejected {
            code: String::from("CONFLICT"),
            detail: String::from("test result"),
            message_id: command_id.to_string(),
        };

        assert!(
            !persist_command_outcome(
                Arc::clone(&state),
                instance_id,
                Some(reservation),
                result,
                Timestamp::from_unix_millis(1_700_000_000_000).expect("timestamp"),
                None,
                tokio::time::Instant::now() + super::PERSISTENCE_FOREGROUND_DEADLINE,
            )
            .await,
            "lost first acknowledgement must return bounded PERSISTENCE_UNAVAILABLE"
        );
        state.drain_recovery(Duration::from_secs(1)).await;

        let first = store
            .first_saved
            .lock()
            .expect("checkpoint lock")
            .clone()
            .expect("first durable save");
        let first_checkpoint = orbisync_world_runtime::Checkpoint::from_json_bytes(&first.payload)
            .expect("checkpoint decode");
        let first_entry = &first_checkpoint.dedup[0];
        let expected_expiry = first_entry
            .created_at_millis
            .checked_add(crate::command_dedup::CommandDedupStore::DEFAULT_TTL_MILLIS)
            .expect("first durable timestamp must have a representable TTL");
        assert_eq!(
            first_entry.expires_at_millis, expected_expiry,
            "durable expiry must be first durable created_at plus the default TTL"
        );
        let local_entry = state
            .command_dedup
            .snapshot(instance_id, first_entry.expires_at_millis - 1)
            .into_iter()
            .find(|entry| entry.command_id == command_id.to_string())
            .expect("local replay entry");
        assert_eq!(local_entry.created_at_millis, first_entry.created_at_millis);
        assert_eq!(local_entry.expires_at_millis, expected_expiry);
        assert!(matches!(
            state.command_dedup.begin(
                instance_id,
                command_id,
                fingerprint,
                first_entry.expires_at_millis + 1,
            ),
            CommandDedupAdmission::Leader(_)
        ));
    }
    struct PublicationStore {
        calls: AtomicUsize,
        hold_first: bool,
        receipts: bool,
        release: tokio::sync::Semaphore,
    }
    #[async_trait]
    impl CheckpointStore for PublicationStore {
        async fn save_checkpoint(&self, checkpoint: AppCheckpoint) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            if self.hold_first && self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                std::future::pending::<()>().await;
            }
            if !self.receipts { return Ok(Vec::new()); }
            self.release.acquire().await.expect("release").forget();
            let checkpoint = orbisync_world_runtime::Checkpoint::from_json_bytes(&checkpoint.payload).expect("codec");
            Ok(checkpoint.dedup.into_iter().map(|e| orbisync_application::CheckpointSaveReceipt {
                command_id: e.command_id, created_at_millis: e.created_at_millis, expires_at_millis: e.expires_at_millis,
            }).collect())
        }
        async fn load_latest(&self, _: InstanceId) -> Result<Option<AppCheckpoint>, ApplicationError> { Ok(None) }
    }

    #[tokio::test]
    async fn publication_after_durability_is_once_for_foreground_background_and_never_dirty() {
        use orbisync_domain::{EntityId, EntityKind, Revision, UserId, VisibilityPolicy, Vec3};
        use super::{InstanceCommand, CommandOutcome, WorldPermissions};
        use crate::delivery::DeliverySink;
        use prost::Message;
        for (background, dirty, disconnect_sender, stop_actor) in [
            (false,false,false,false), (true,false,false,false),
            (true,false,true,false), (true,false,false,true), (true,true,false,false),
        ] {
            let instance = InstanceId::generate();
            let owner = UserId::generate();
            let entity_id = EntityId::generate();
            let mut actor = orbisync_world_runtime::actor::InstanceActor::new(orbisync_world_runtime::InstanceRuntimeDescriptor {
                instance_id: instance, state: orbisync_world_runtime::RuntimeState::Running, revision: Revision::INITIAL,
            });
            let CommandOutcome::Applied { revision, committed_entity: Some(entity), .. } = actor.handle(InstanceCommand::SpawnEntity {
                command_id: None, entity_id, kind: EntityKind::Object, owner: Some(owner), transform: None,
                visibility: VisibilityPolicy::Global, requester: owner, permissions: WorldPermissions::all(),
            }) else { unreachable!("actor spawn"); };
            let registry = Arc::new(RuntimeRegistry::new());
            registry.insert(instance, actor).expect("registry");
            let store = Arc::new(PublicationStore { calls: AtomicUsize::new(0), hold_first: background,
                receipts: !dirty, release: tokio::sync::Semaphore::new(if background {0} else {1}) });
            let mut raw = RealtimeState::new(orbisync_config::Config::default().realtime, registry,
                Arc::new(DeliveryRegistry::new()), Arc::new(SystemClock::new()));
            raw.checkpoint_store = Some(store.clone());
            let state = Arc::new(raw);
            let make_sink = || Arc::new(super::RealtimeDeliverySink::new(state.clone(), instance,
                Vec3::new(0.0,0.0,0.0).expect("position"), owner, None, Default::default()));
            let sender = make_sink(); let peer = make_sink();
            let sender_registration = state.delivery.register_sink(instance, sender.clone() as Arc<dyn DeliverySink>);
            let _peer_registration = state.delivery.register_sink(instance, peer.clone() as Arc<dyn DeliverySink>);
            let command_id = CommandId::generate();
            let request = orbisync_protocol::v1::EntityCommand { command_id: command_id.to_string(), entity_id: entity_id.to_string(),
                operation: "spawn".into(), ..Default::default() };
            let fingerprint = crate::command_dedup::CommandDedupStore::fingerprint(&request);
            let CommandDedupAdmission::Leader(reservation) = state.command_dedup.begin(instance, command_id, fingerprint, 0) else { unreachable!("leader"); };
            let mut command = request;
            command.expected_revision = 1; command.instance_revision = Some(revision.as_u64());
            let result = CommandDedupResult::Applied { command: command.clone(), message_id: command_id.to_string() };
            let publication = super::CommandPublication { negotiated_minor: 0,
                views: Arc::new(std::iter::once(orbisync_world_runtime::actor::EntityInterestView::from_entity(&entity)).collect()),
                revision: revision.as_u64() };
            let durable = super::persist_command_outcome_with_publication(state.clone(), instance, Some(reservation), result,
                state.clock.now(), None, tokio::time::Instant::now()+Duration::from_millis(40), Some(publication)).await;
            assert_eq!(durable, !background);
            if background {
                assert!(sender.queue().lock().expect("queue").is_empty());
                assert!(peer.queue().lock().expect("queue").is_empty());
                if disconnect_sender { drop(sender_registration); }
                if stop_actor {
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while store.calls.load(Ordering::Acquire) < 2 { tokio::task::yield_now().await; }
                    }).await.expect("background save entered");
                    drop(state.registry.remove(&instance).expect("stop actor"));
                }
                store.release.add_permits(1);
            }
            state.drain_recovery(Duration::from_secs(1)).await;
            let frames = peer.queue().lock().expect("queue").drain_all();
            if dirty {
                assert!(frames.is_empty());
                assert!(matches!(state.command_dedup.begin(instance, command_id, fingerprint, 1), CommandDedupAdmission::PersistenceUnavailable));
            } else {
                assert_eq!(frames.len(),1);
                let env = super::Envelope::decode(frames[0].as_slice()).expect("envelope");
                assert!(matches!(env.payload, Some(super::envelope::Payload::EntityCommand(c)) if c == command));
                assert_eq!(sender.queue().lock().expect("queue").len(), usize::from(!disconnect_sender));
                assert!(matches!(state.command_dedup.begin(instance, command_id, fingerprint, 1), CommandDedupAdmission::Duplicate(_)));
                assert!(peer.queue().lock().expect("queue").is_empty(), "duplicate admission never republishes");
                if !stop_actor {
                    let read = state.registry.read_snapshot(instance).await.expect("snapshot");
                    assert_eq!(read.entities.len(),1); assert_eq!(read.entities[0].revision(), Revision::from_u64(1));
                }
            }
        }
    }

}
