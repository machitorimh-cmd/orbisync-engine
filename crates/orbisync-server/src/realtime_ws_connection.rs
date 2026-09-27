async fn handle_socket(
    socket: RealtimeSocket,
    state: Arc<RealtimeState>,
    subprotocol: Option<String>,
) {
    let Some(AuthenticatedConnection {
        mut socket,
        state,
        mut shutdown_rx,
        mut conn_state,
        handshake,
        bytes: _bytes,
        heartbeat_policy,
        user,
        session_id,
        permissions,
        viewer_roles,
        mut shutdown_notified,
        client_hello_resume_token,
    }) = authenticate_connection(socket, state, subprotocol).await
    else {
        return;
    };
    let state_sync = decode_envelope(&handshake.server_hello_bytes).ok().is_some_and(|envelope| {
        matches!(envelope.payload, Some(envelope::Payload::ServerHello(hello))
            if hello.enabled_features.iter().any(|feature| feature == STATE_SYNC_FEATURE))
    });
    // W-18: join / resume loop. After handshake ready, either JoinInstance or ResumeSession may arrive.
    // Server-stored token path (D-23) kept in process memory (D-24).
    let viewer_entity: Option<EntityId> = None;
    let mut subscribed: std::collections::HashMap<EntityId, bool> =
        std::collections::HashMap::new();
    #[allow(unused_assignments)]
    let mut presence_guard_inner: Option<PresenceGuard> = None;

    // Loop waiting for JoinInstance or ResumeSession.
    let final_instance_id: InstanceId;
    // Resolved alongside the instance in both the join and resume branches,
    // and carried into the runtime loop so every command can consult the
    // participation boundary without another lookup (ADR-026).
    let final_world_id: orbisync_domain::WorldId;
    let final_presence: PresenceId;
    let final_delivery_sink: Arc<RealtimeDeliverySink>;
    let final_delivery_registration: crate::delivery::DeliveryRegistration;
    let next_sequence_after_join: u64;
    // ClientHello consumed sequence one in the authentication phase.  Join
    // and Resume therefore begin at two; the connected loop receives the
    // tracker state after the selected admission message.
    let mut inbound_sequence =
        orbisync_realtime::InboundSequenceTracker::with_expected(2);
    let mut next_admission_sequence = 2_u64;
    let mut heartbeat = HeartbeatManager::new(heartbeat_policy, state.clock.now());
    let mut admission_limiter = RateLimiter::new_with_threshold(
        state.rate_limit_normal_per_sec, state.rate_limit_custom_per_sec,
        state.rate_limit_persistent_threshold, state.clock.now());
    let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_millis(heartbeat_policy.interval_millis()));
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat_interval.tick().await;
    loop {
        let response_sequence = next_admission_sequence;
        let next_message = tokio::select! {
            msg = socket.recv() => msg,
            _ = heartbeat_interval.tick() => {
                let now = state.clock.now();
                if heartbeat.is_timed_out(now) {
                    drop(socket.write(Message::Close(None)).await);
                    return;
                }
                if heartbeat.should_send_heartbeat(now) {
                    let now_ms = now.to_unix_millis().unwrap_or(0);
                    let envelope = heartbeat_envelope(handshake.negotiated_minor,
                        uuid::Uuid::now_v7().to_string(), response_sequence, now_ms, String::new(), now_ms);
                    if socket.write(Message::Binary(envelope.encode_to_vec().into())).await.is_err() { return; }
                    next_admission_sequence += 1;
                    heartbeat.mark_heartbeat_sent(now);
                }
                continue;
            }
            notice = shutdown_rx.recv() => {
                match notice {
                    Ok(ShutdownNotice::Begin) if !shutdown_notified => {
                        send_error(
                            &mut socket,
                            "SERVER_SHUTTING_DOWN",
                            "server is shutting down; please reconnect".to_owned(),
                            String::new(),
                            handshake.negotiated_minor,
                            response_sequence,
                            state.clock.as_ref(),
                            "",
                        )
                        .await;
                        next_admission_sequence += 1;
                        shutdown_notified = true;
                    }
                    Ok(ShutdownNotice::Force) => return,
                    _ => {}
                }
                continue;
            }
        };
        let Some(Ok(Message::Binary(msg_bytes))) = next_message else {
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return;
        };
        let Ok(env) = decode_envelope(&msg_bytes) else {
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return;
        };
        match inbound_sequence.accept(env.sequence) {
            orbisync_realtime::InboundSequence::Exact => {}
            orbisync_realtime::InboundSequence::Duplicate => continue,
            orbisync_realtime::InboundSequence::Gap { expected, received } => {
                send_error(
                    &mut socket,
                    "SEQUENCE_GAP",
                    format!("expected inbound sequence {expected}, received {received}"),
                    env.message_id,
                    handshake.negotiated_minor,
                    response_sequence,
                    state.clock.as_ref(),
                    "",
                )
                .await;
                return;
            }
            orbisync_realtime::InboundSequence::Overflow => {
                send_error(
                    &mut socket,
                    "SEQUENCE_OVERFLOW",
                    "inbound sequence exhausted; reconnect required".to_owned(),
                    env.message_id,
                    handshake.negotiated_minor,
                    response_sequence,
                    state.clock.as_ref(),
                    "",
                )
                .await;
                return;
            }
        }
        if heartbeat.is_timed_out(state.clock.now()) {
            drop(socket.write(Message::Close(None)).await);
            return;
        }
        heartbeat.record_activity(state.clock.now());
        if matches!(env.payload, Some(envelope::Payload::HeartbeatAck(_))) { continue; }
        next_admission_sequence += 1;
        if let Err(error) = admission_limiter.check(state.clock.now(), MessageCategory::Normal) {
            let persistent = error.is_persistent();
            send_error_with_retryable(&mut socket,
                if persistent { "PERSISTENT_RATE_LIMIT" } else { "RATE_LIMITED" },
                error.to_string(), env.message_id, handshake.negotiated_minor,
                response_sequence, state.clock.as_ref(), "", !persistent).await;
            if persistent { return; }
            continue;
        }
        let Some(admission) = state.shutdown.admit().filter(|_| !shutdown_notified) else {
            send_error(
                &mut socket,
                "SERVER_SHUTTING_DOWN",
                "server is shutting down; please reconnect".to_owned(),
                env.message_id,
                handshake.negotiated_minor,
                response_sequence,
                state.clock.as_ref(),
                "",
            )
            .await;
            continue;
        };
        // Retained through Join/Resume completion, including asynchronous
        // authorization, activation and mailbox waits. Idle sockets own none.
        match env.payload {
            Some(envelope::Payload::Heartbeat(hb)) => {
                if let Some(ack) = heartbeat_ack_for_heartbeat(handshake.negotiated_minor,
                    uuid::Uuid::now_v7().to_string(), response_sequence, String::new(), &hb, state.clock.now()) {
                    if socket.write(Message::Binary(ack.encode_to_vec().into())).await.is_err() { return; }
                }
            }
            Some(envelope::Payload::JoinInstance(join)) => {
                state.metrics_recorder.incr(Counter::InstanceCommandsTotal);
                let Some(next) = transition(conn_state, ConnectionEvent::JoinRequested) else {
                    // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = socket.write(Message::Close(None)).await;
                    return;
                };
                conn_state = next;
                let Ok(iid) = join.world_instance_id.parse::<InstanceId>() else {
                    send_error(
                        &mut socket,
                        "INVALID_ARGUMENT",
                        "world_instance_id is not a valid id".to_owned(),
                        env.message_id.clone(),
                        handshake.negotiated_minor,
                        response_sequence,
                        state.clock.as_ref(),
                        "",
                    )
                    .await;
                    let Some(ready) = transition(conn_state, ConnectionEvent::JoinRejected) else { return; };
                    conn_state = ready;
                    continue;
                };
                let inst = match state.world_store.get_instance(iid).await {
                    Ok(Some(v)) => v,
                    _ => {
                        send_error(
                            &mut socket,
                            "NOT_FOUND",
                            format!("instance {iid} not found"),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            response_sequence,
                            state.clock.as_ref(),
                            "",
                        )
                        .await;
                        let Some(ready) = transition(conn_state, ConnectionEvent::JoinRejected) else { return; };
                        conn_state = ready;
                        continue;
                    }
                };
                let capacity = inst.capacity();
                let world_id = inst.world_id();
                let world = match state.world_store.get_world(world_id).await {
                    Ok(Some(w)) => w,
                    _ => {
                        send_error(
                            &mut socket,
                            "INTERNAL_ERROR",
                            format!("world {world_id} not found for instance {iid}"),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            response_sequence,
                            state.clock.as_ref(),
                            &iid.to_string(),
                        )
                        .await;
                        let Some(ready) = transition(conn_state, ConnectionEvent::JoinRejected) else { return; };
                        conn_state = ready;
                        continue;
                    }
                };
                // ADR-026: bound which world a temporary subject may join.
                // The client chooses `world_instance_id`, and before ADR-026
                // nothing authorized that choice; the checks above only confirm
                // the instance exists and its world resolves. Decide on the
                // resolved world rather than the instance id, so an instance
                // created later in an already permitted world is not refused.
                // Placed before `ensure_instance_activated` so a refused join
                // does not restore durable state on the way to being rejected.
                if !ephemeral_participation_allowed(
                    state.ephemeral_scope.as_deref(),
                    user,
                    world_id.as_uuid(),
                    state.clock.now(),
                )
                .await
                {
                    tracing::warn!(
                        event = "realtime.join_denied_by_scope",
                        instance_id = %iid,
                        world_id = %world_id,
                        "join refused: outside the subject's permitted worlds"
                    );
                    send_error(
                        &mut socket,
                        "NOT_AUTHORIZED",
                        "not permitted to join this world".to_owned(),
                        env.message_id.clone(),
                        handshake.negotiated_minor,
                        2,
                        state.clock.as_ref(),
                        &iid.to_string(),
                    )
                    .await;
                    return;
                }
                let viewer_pos = world.default_spawn().position();
                let instance_lifecycle_guard = match state.ensure_instance_activated_admitted(iid, &admission).await {
                    Ok(guard) => guard,
                    Err(error) => {
                        tracing::error!(
                            event = "checkpoint.restore_failed",
                            instance_id = %iid,
                            error = %error,
                            "refusing join because durable instance state could not be restored"
                        );
                        send_error(
                            &mut socket,
                            "INTERNAL_ERROR",
                            "instance state could not be restored".to_owned(),
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            response_sequence,
                            state.clock.as_ref(),
                            &iid.to_string(),
                        )
                        .await;
                        let Some(ready) = transition(conn_state, ConnectionEvent::JoinRejected) else { return; };
                        conn_state = ready;
                        continue;
                    }
                };
                let pres = PresenceId::generate();
                let outcome = if !state.registry.contains(iid) {
                    CommandOutcome::Rejected {
                        code: "NOT_FOUND",
                        detail: String::from("instance not found"),
                    }
                } else {
                    let mut result = match tracing::Instrument::instrument(
                        state.registry.submit(
                            iid,
                            InstanceCommand::Join {
                                presence_id: pres,
                                user_id: user,
                                instance_id: iid,
                                capacity,
                            },
                        ),
                        tracing::info_span!(
                            "application.join_flow",
                            instance_id = tracing::field::display(iid)
                        ),
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => mailbox_rejection(error),
                    };
                        // A presence held open for the resume grace window still
                        // occupies a slot. If that is the only reason the
                        // instance is full, give the slot to the client that is
                        // actually here: evict the longest-held binding, release
                        // its membership and retry once. The evicted client gets
                        // ResyncRequired on resume, which
                        // `mobile-resume-interest-backpressure.md` line 71 calls
                        // the correct degradation. Without this, repeatedly
                        // joining and dropping keeps an instance full for a
                        // whole grace window at no cost to the attacker.
                        if matches!(&result, CommandOutcome::Rejected { code, .. } if *code == "INSTANCE_FULL")
                        {
                            if let Some(evicted) = state.resume_store.evict_oldest_for_instance(iid)
                            {
                                let leave_result = state
                                    .registry
                                    .submit(
                                        iid,
                                        InstanceCommand::Leave {
                                            presence_id: evicted.presence_id,
                                        },
                                    )
                                    .await;
                                if let Err(error) = leave_result {
                                    tracing::warn!(
                                        event = "realtime.resume_binding_leave_failed",
                                        instance_id = %iid,
                                        error = ?error,
                                    );
                                }
                                tracing::info!(
                                    event = "realtime.resume_binding_evicted",
                                    instance_id = %iid,
                                    "evicted a grace-held presence to admit a live member"
                                );
                                result = match tracing::Instrument::instrument(
                                    state.registry.submit(
                                        iid,
                                        InstanceCommand::Join {
                                            presence_id: pres,
                                            user_id: user,
                                            instance_id: iid,
                                            capacity,
                                        },
                                    ),
                                    tracing::info_span!(
                                        "application.join_flow",
                                        instance_id = tracing::field::display(iid)
                                    ),
                                )
                                .await
                                {
                                    Ok(outcome) => outcome,
                                    Err(error) => mailbox_rejection(error),
                                };
                            }
                        }
                        result
                };
                match outcome {
                    CommandOutcome::Applied { revision, .. } => {
                        record_total_members(&state.registry, state.metrics_recorder.as_ref());
                        if let Some(next) = transition(conn_state, ConnectionEvent::JoinAccepted) {
                            conn_state = next;
                        }
                        presence_guard_inner = Some(PresenceGuard {
                            delivery: Arc::clone(&state.delivery),
                            instance_id: iid,
                            presence: pres,
                            resume_store: Arc::clone(&state.resume_store),
                        });
                        // W-18: issue server-stored resume token (D-23, D-24), never logged.
                        let resume_token = state.resume_store.issue(user, iid, pres, 0, revision);
                        let sink = Arc::new(RealtimeDeliverySink::new(
                            Arc::clone(&state), iid, viewer_pos, user, viewer_roles.clone(),
                            std::collections::HashMap::new(),
                        ));
                        sink.begin_snapshot();
                        let registration = state.delivery.register_sink(iid, Arc::clone(&sink) as Arc<dyn DeliverySink>);
                        let Some(read) = state.registry.read_snapshot(iid).await else { return; };
                        let revision = read.revision;
                        let server_time_unix_ms =
                            state.clock.now().to_unix_millis().unwrap_or(0);
                        let (nearby_entities, nearby_presence_ids, nearby_user_ids) = if state_sync {
                                (Vec::new(), Vec::new(), Vec::new())
                            } else {
                                filtered_join_accepted_state(
                                    &read,
                                    viewer_pos,
                                    user,
                                    &state.interest_grid,
                                    &mut subscribed,
                                    viewer_roles.as_deref(),
                                )
                            };
                        let join_accepted = orbisync_protocol::v1::JoinAccepted {
                            presence_id: pres.to_string(),
                            instance_revision: revision.as_u64(),
                            resume_token,
                            user_id: user.to_string(),
                            instance_id: iid.to_string(),
                            entity_spawn: permissions.entity_spawn,
                            entity_update_own: permissions.entity_update_own,
                            entity_update_any: permissions.entity_update_any,
                            nearby_entities,
                            nearby_presence_ids,
                            nearby_user_ids,
                            server_time_unix_ms,
                        };
                        let (snapshot_revision, snapshot) = {
                                prepare_snapshot_for_viewer(
                                    &read,
                                    Some(viewer_pos),
                                    Some(user),
                                    &state.interest_grid,
                                    &mut subscribed,
                                    viewer_roles.as_deref(),
                                    SnapshotContext {
                                        instance: &inst,
                                        world: &world,
                                        metrics: state.metrics_recorder.as_ref(),
                                        permissions,
                                        server_time_unix_ms,
                                    },
                                )
                            };
                        sink.install_snapshot(snapshot_revision, std::mem::take(&mut subscribed));
                        drop(instance_lifecycle_guard);
                        let envelope1 = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: iid.to_string(),
                            payload: Some(envelope::Payload::JoinAccepted(join_accepted)),
                        };
                        let mut buf = Vec::new();
                        if envelope1.encode(&mut buf).is_ok() {
                            if socket.write(Message::Binary(buf.into())).await.is_err() {
                                if let Some(violation) = socket.take_size_violation() {
                                    send_outbound_size_error(
                                        &mut socket,
                                        violation,
                                        env.message_id,
                                        handshake.negotiated_minor,
                                        response_sequence,
                                        state.clock.as_ref(),
                                        &iid.to_string(),
                                    )
                                    .await;
                                }
                                return;
                            }
                        }
                        next_sequence_after_join = send_snapshot_chunks(
                            &mut socket,
                            (snapshot_revision, snapshot),
                            iid,
                            handshake.negotiated_minor,
                            response_sequence + 1,
                            state.clock.as_ref(),
                            state.metrics_recorder.as_ref(),
                            &sink,
                        )
                        .await;
                        sink.finish_snapshot();
                        final_delivery_sink = sink;
                        final_delivery_registration = registration;
                        final_instance_id = iid;
                        final_world_id = world_id;
                        final_presence = pres;
                        break;
                    }
                    CommandOutcome::Rejected { code, detail } => {
                        let Some(ready) = transition(conn_state, ConnectionEvent::JoinRejected) else { return; };
                        conn_state = ready;
                        send_error(
                            &mut socket,
                            code,
                            detail,
                            env.message_id.clone(),
                            handshake.negotiated_minor,
                            response_sequence,
                            state.clock.as_ref(),
                            "",
                        )
                        .await;
                        continue;
                    }
                }
            }
            Some(envelope::Payload::ResumeSession(sess)) => {
                state.metrics_recorder.incr(Counter::ResumeAttemptsTotal);
                let ch_opt = if client_hello_resume_token.is_empty() {
                    None
                } else {
                    Some(client_hello_resume_token.as_str())
                };
                let rs_opt = if sess.resume_token.is_empty() {
                    None
                } else {
                    Some(sess.resume_token.as_str())
                };
                let resume_outcome = decide_resume(ch_opt, rs_opt);
                if resume_outcome.decision != orbisync_realtime::ResumeDecision::AcceptResumeRequest
                {
                    let rs = orbisync_protocol::v1::ResyncRequired {
                        reason_code: orbisync_realtime::RESUME_TOKEN_MISMATCH.to_owned(),
                        current_revision: 0,
                    };
                    let env_rs = Envelope {
                        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                        protocol_minor: handshake.negotiated_minor,
                        message_id: uuid::Uuid::now_v7().to_string(),
                        sequence: response_sequence,
                        sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                        instance_id: String::new(),
                        payload: Some(envelope::Payload::ResyncRequired(rs)),
                    };
                    let mut buf = Vec::new();
                    if env_rs.encode(&mut buf).is_ok() {
                        // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = socket.write(Message::Binary(buf.into())).await;
                    }
                    if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired) {
                        conn_state = next;
                    } else if let Some(next) =
                        transition(conn_state, ConnectionEvent::ResumeRequested)
                    {
                        conn_state = next;
                    }
                    continue;
                }
                if let Some(next) = transition(conn_state, ConnectionEvent::ResumeRequested) {
                    conn_state = next;
                } else {
                    // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = socket.write(Message::Close(None)).await;
                    return;
                }
                let consumed = state.resume_store.consume_for_user(&sess.resume_token, user);
                let binding = match consumed {
                    Some(b) => b,
                    None => {
                        let rs = orbisync_protocol::v1::ResyncRequired {
                            reason_code: "history_unavailable".to_owned(),
                            current_revision: 0,
                        };
                        let env_rs = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: String::new(),
                            payload: Some(envelope::Payload::ResyncRequired(rs)),
                        };
                        let mut buf = Vec::new();
                        if env_rs.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                        if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired)
                        {
                            conn_state = next;
                        }
                        continue;
                    }
                };
                if binding.user_id != user {
                    let rs = orbisync_protocol::v1::ResyncRequired {
                        reason_code: "history_unavailable".to_owned(),
                        current_revision: 0,
                    };
                    let env_rs = Envelope {
                        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                        protocol_minor: handshake.negotiated_minor,
                        message_id: uuid::Uuid::now_v7().to_string(),
                        sequence: response_sequence,
                        sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                        instance_id: binding.instance_id.to_string(),
                        payload: Some(envelope::Payload::ResyncRequired(rs)),
                    };
                    let mut buf = Vec::new();
                    if env_rs.encode(&mut buf).is_ok() {
                        // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = socket.write(Message::Binary(buf.into())).await;
                    }
                    if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired) {
                        conn_state = next;
                    }
                    continue;
                }
                let iid = binding.instance_id;
                let publication_guard = state.registry.lifecycle_guard(iid).await;
                let inst = match state.world_store.get_instance(iid).await {
                    Ok(Some(v)) => v,
                    _ => {
                        let rs = orbisync_protocol::v1::ResyncRequired {
                            reason_code: "history_unavailable".to_owned(),
                            current_revision: 0,
                        };
                        let env_rs = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: iid.to_string(),
                            payload: Some(envelope::Payload::ResyncRequired(rs)),
                        };
                        let mut buf = Vec::new();
                        if env_rs.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                        if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired)
                        {
                            conn_state = next;
                        }
                        continue;
                    }
                };
                let world_id = inst.world_id();
                let world = match state.world_store.get_world(world_id).await {
                    Ok(Some(w)) => w,
                    _ => {
                        let rs = orbisync_protocol::v1::ResyncRequired {
                            reason_code: "history_unavailable".to_owned(),
                            current_revision: 0,
                        };
                        let env_rs = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: iid.to_string(),
                            payload: Some(envelope::Payload::ResyncRequired(rs)),
                        };
                        let mut buf = Vec::new();
                        if env_rs.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                        if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired)
                        {
                            conn_state = next;
                        }
                        continue;
                    }
                };
                // ADR-026: resume returns straight to `binding.instance_id`
                // without passing through the join branch, so the participation
                // boundary has to be checked here too. The ticket already
                // proved who the subject is; what still needs deciding is
                // where it may be. A refusal here is explicit rather than a
                // resync, which would invite the client to retry into the same
                // world it is not permitted to enter.
                if !ephemeral_participation_allowed(
                    state.ephemeral_scope.as_deref(),
                    user,
                    world_id.as_uuid(),
                    state.clock.now(),
                )
                .await
                {
                    tracing::warn!(
                        event = "realtime.resume_denied_by_scope",
                        instance_id = %iid,
                        world_id = %world_id,
                        "resume refused: outside the subject's permitted worlds"
                    );
                    send_error(
                        &mut socket,
                        "NOT_AUTHORIZED",
                        "not permitted to resume in this world".to_owned(),
                        env.message_id.clone(),
                        handshake.negotiated_minor,
                        2,
                        state.clock.as_ref(),
                        &iid.to_string(),
                    )
                    .await;
                    return;
                }
                let sink = Arc::new(RealtimeDeliverySink::new(
                    Arc::clone(&state), iid, world.default_spawn().position(), user,
                    viewer_roles.clone(), std::collections::HashMap::new(),
                ));
                sink.begin_snapshot();
                let registration = state.delivery.register_sink(iid, Arc::clone(&sink) as Arc<dyn DeliverySink>);
                let Some(read) = state.registry.read_snapshot(iid).await else { return; };
                let (current_rev, oldest_retained) = (read.revision, Some(orbisync_domain::Revision::from_u64(read.reliable_floor.as_u64().saturating_add(1))));
                let resume_read = Some(read.clone());
                let window = HistoryWindow::new(current_rev, oldest_retained);
                // A resume token never grants access to events before admission.
                // Keep that floor when rotating tokens, including interrupted replay.
                let last_applied = orbisync_domain::Revision::from_u64(sess.last_applied_revision)
                    .max(binding.last_revision);
                let resync = decide_resync(last_applied, window);
                match resync {
                    orbisync_realtime::ResyncOutcome::ResyncRequired { reason, current } => {
                        let rs = orbisync_protocol::v1::ResyncRequired {
                            reason_code: reason.as_str().to_owned(),
                            current_revision: current.as_u64(),
                        };
                        let env_rs = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: iid.to_string(),
                            payload: Some(envelope::Payload::ResyncRequired(rs)),
                        };
                        let mut buf = Vec::new();
                        if env_rs.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                        if let Some(next) = transition(conn_state, ConnectionEvent::ResyncRequired)
                        {
                            conn_state = next;
                        }
                        continue;
                    }
                    orbisync_realtime::ResyncOutcome::Replay { from: _, current } => {
                        let Some(read) = resume_read else { return; };
                        let vp = world.default_spawn().position();
                        let mut replay_subscribed = std::collections::HashMap::new();
                        let received_ids: std::collections::HashSet<&str> = sess.received_message_ids.iter().map(String::as_str).collect();
                        let replay: Vec<Vec<u8>> = read.reliable_events.iter()
                            .filter(|(revision, _)| *revision > last_applied)
                            .filter(|(_, payload)| decode_envelope(payload).is_ok_and(|event| !received_ids.contains(event.message_id.as_str())))
                            .filter_map(|(_, payload)| filter_payload_with_index(
                                payload, Some(vp), Some(user), &state.interest_grid,
                                &read.interest_views, viewer_roles.as_deref(), &mut replay_subscribed,
                            )).collect();
                        drop(publication_guard);
                        state.metrics_recorder.incr(Counter::ResumeSuccessTotal);
                        let new_token = state.resume_store.issue(
                            user,
                            iid,
                            binding.presence_id,
                            binding.epoch.wrapping_add(1),
                            binding.last_revision,
                        );
                        let accepted = orbisync_protocol::v1::ResumeAccepted {
                            presence_id: binding.presence_id.to_string(),
                            current_revision: current.as_u64(),
                            replay_follows: true,
                            resume_token: new_token,
                        };
                        let env_acc = Envelope {
                            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
                            protocol_minor: handshake.negotiated_minor,
                            message_id: uuid::Uuid::now_v7().to_string(),
                            sequence: response_sequence,
                            sent_at_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            instance_id: iid.to_string(),
                            payload: Some(envelope::Payload::ResumeAccepted(accepted)),
                        };
                        let mut buf = Vec::new();
                        if env_acc.encode(&mut buf).is_ok() {
                            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = socket.write(Message::Binary(buf.into())).await;
                        }
                        if let Some(next) = transition(conn_state, ConnectionEvent::ResumeAccepted)
                        {
                            conn_state = next;
                        }
                        let mut replay_sequence = response_sequence + 1;
                        for payload in replay {
                            let Ok(mut replay_envelope) = decode_envelope(&payload) else { return; };
                            replay_envelope.sequence = replay_sequence;
                            replay_sequence += 1;
                            replay_envelope.sent_at_unix_ms = state.clock.now().to_unix_millis().unwrap_or(0);
                            if socket.write(Message::Binary(replay_envelope.encode_to_vec().into())).await.is_err() {
                                return;
                            }
                        }
                        // Replay: send filtered snapshot of current state as diff follow-up.
                        // Compute snapshot while holding the lock, then drop before await (Send).
                        let mut snapshot_subscribed = std::collections::HashMap::new();
                        let (snapshot_revision, snap_data) = prepare_snapshot_for_viewer(
                            &read, Some(world.default_spawn().position()), Some(user),
                            &state.interest_grid, &mut snapshot_subscribed, viewer_roles.as_deref(),
                            SnapshotContext {
                                instance: &inst, world: &world, metrics: state.metrics_recorder.as_ref(),
                                permissions, server_time_unix_ms: state.clock.now().to_unix_millis().unwrap_or(0),
                            },
                        );
                        sink.install_snapshot(snapshot_revision, snapshot_subscribed);
                        next_sequence_after_join = send_snapshot_chunks(
                            &mut socket, (snapshot_revision, snap_data), iid, handshake.negotiated_minor,
                            replay_sequence, state.clock.as_ref(), state.metrics_recorder.as_ref(), &sink,
                        ).await;
                        sink.finish_snapshot();
                        final_delivery_sink = sink;
                        final_delivery_registration = registration;
                        final_instance_id = iid;
                        final_world_id = world_id;
                        final_presence = binding.presence_id;
                        presence_guard_inner = Some(PresenceGuard {
                            delivery: Arc::clone(&state.delivery),
                            instance_id: iid,
                            presence: binding.presence_id,
                            resume_store: Arc::clone(&state.resume_store),
                        });
                        break;
                    }
                }
            }
            _ => {
                // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
                #[allow(clippy::let_underscore_must_use)]
                let _ = socket.write(Message::Close(None)).await;
                return;
            }
        }
    }

    run_connected_socket(
        socket,
        state,
        shutdown_rx,
        conn_state,
        handshake,
        heartbeat_policy,
        user,
        session_id,
        permissions,
        viewer_entity,
        final_delivery_sink,
        final_delivery_registration,
        final_instance_id,
        final_world_id,
        final_presence,
        next_sequence_after_join,
        inbound_sequence.expected(),
        shutdown_notified,
        presence_guard_inner,
    )
    .await;
}
