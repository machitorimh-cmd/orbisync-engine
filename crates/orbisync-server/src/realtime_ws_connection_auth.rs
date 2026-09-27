struct AuthenticatedConnection {
    socket: RealtimeSocket,
    state: Arc<RealtimeState>,
    shutdown_rx: tokio::sync::broadcast::Receiver<ShutdownNotice>,
    conn_state: ConnectionState,
    handshake: orbisync_realtime::gateway::HandshakeSuccess,
    bytes: Vec<u8>,
    heartbeat_policy: HeartbeatPolicy,
    user: UserId,
    session_id: Option<orbisync_domain::AuthSessionId>,
    permissions: WorldPermissions,
    viewer_roles: Option<Arc<std::collections::BTreeSet<orbisync_domain::RoleId>>>,
    shutdown_notified: bool,
    client_hello_resume_token: String,
}

/// Resolves the roles carried by an authenticated realtime connection.
///
/// Keeping this as the authentication-stage boundary makes it explicit that
/// the role set passed to the realtime visibility filter comes from the
/// identity repository, rather than from a connection-local default.
async fn resolve_viewer_roles(
    identity_repository: Option<&dyn IdentityRepository>,
    user: UserId,
) -> Option<Arc<std::collections::BTreeSet<orbisync_domain::RoleId>>> {
    match identity_repository {
        Some(repository) => match repository.roles_for_user(user).await {
            Ok(roles) => Some(Arc::new(roles.into_iter().map(|role| role.id()).collect())),
            Err(_) => {
                tracing::warn!(
                    event = "realtime.viewer_roles_unavailable",
                    "viewer roles unavailable; role-restricted entities will be denied"
                );
                None
            }
        },
        None => Some(Arc::new(std::collections::BTreeSet::new())),
    }
}

#[tracing::instrument(
    name = "realtime_gateway.websocket_handshake",
    skip_all,
    fields(connection_id = tracing::field::Empty, message_size = tracing::field::Empty)
)]
async fn authenticate_connection(
    mut socket: RealtimeSocket,
    state: Arc<RealtimeState>,
    subprotocol: Option<String>,
) -> Option<AuthenticatedConnection> {
    if state.shutdown.is_rejecting() {
        return None;
    }
    let mut shutdown_rx = state.shutdown.subscribe();
    let mut conn_state = ConnectionState::Connecting;
    match handle_upgrade(conn_state, subprotocol.as_deref()) {
        Ok(next) => conn_state = next,
        Err(_) => {
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
    }
    // C3: handshake deadline – first ClientHello must arrive within handshake_timeout_ms,
    // otherwise close and count as timeout. The value is config-driven (not hardcoded).
    let handshake_timeout = std::time::Duration::from_millis(state.config.handshake_timeout_ms);
    let bytes = match tokio::select! {
        result = tokio::time::timeout(handshake_timeout, socket.recv()) => result,
        notice = shutdown_rx.recv() => {
            if matches!(notice, Ok(ShutdownNotice::Begin | ShutdownNotice::Force)) {
                return None;
            }
            return None;
        }
    } {
        Ok(Some(Ok(Message::Binary(b)))) => b,
        Ok(Some(Ok(_))) | Ok(Some(Err(_))) | Ok(None) => {
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
        Err(_) => {
            state
                .handshake_timeout_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                event = "realtime.handshake_timeout",
                timeout_ms = state.config.handshake_timeout_ms,
                "handshake timeout waiting for ClientHello"
            );
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
    };
    // Begin may precede its broadcast while the tick/actor drain is pending.
    // A hello received in that gap must not consume a fresh ticket.
    if state.shutdown.is_rejecting() {
        return None;
    }
    let span = tracing::Span::current();
    span.record("message_size", bytes.len());
    if let Ok(envelope) = decode_envelope(&bytes) {
        if envelope.sequence != orbisync_realtime::InboundSequenceTracker::INITIAL {
            send_error(
                &mut socket,
                "SEQUENCE_GAP",
                format!(
                    "expected inbound sequence {}, received {}",
                    orbisync_realtime::InboundSequenceTracker::INITIAL,
                    envelope.sequence
                ),
                envelope.message_id,
                0,
                0,
                state.clock.as_ref(),
                "",
            )
            .await;
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
        if let Err(error) = orbisync_realtime::check_message_size_for_envelope(
            &bytes,
            &envelope,
            state.config.max_normal_message_bytes,
            state.config.max_message_bytes,
        ) {
            send_error(
                &mut socket,
                "MESSAGE_TOO_LARGE",
                error.to_string(),
                String::new(),
                0,
                0,
                state.clock.as_ref(),
                "",
            )
            .await;
            // The transport contract requires an ErrorMessage followed by
            // connection teardown for an oversized realtime message.
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
    }
    let connection_id = RealtimeConnectionId::generate();
    span.record("connection_id", tracing::field::display(connection_id));
    let heartbeat_policy = HeartbeatPolicy::from_config(&state.config);
    let now = state.clock.now();
    let mut handshake = match handle_client_hello(
        conn_state,
        &bytes,
        connection_id,
        heartbeat_policy,
        now,
        state.config.max_message_bytes,
    ) {
        Ok(success) => success,
        Err(_) => {
            // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
            #[allow(clippy::let_underscore_must_use)]
            let _ = socket.write(Message::Close(None)).await;
            return None;
        }
    };

    // RV-A C1: verify realtime ticket before sending ServerHello. The handshake
    // carries `realtime_ticket` (redacted in Debug, never logged) and
    // `handle_client_hello` already rejected empty tickets via
    // `validate_ticket`. Here we resolve the ticket to the authenticated user
    // via the async HMAC verifier which atomically consumes the ticket and
    // checks session active (DELETE ... USING auth_sessions ... RETURNING).
    // Verification must happen before ServerHello so that a consumed or revoked
    // ticket is rejected without a successful handshake (C1 single-use).
    // Do not log `handshake.realtime_ticket` and do not pass the whole
    // handshake to tracing.
    let (user, session_id) = match state
        .tickets
        .verify_with_session(&handshake.realtime_ticket)
        .await
    {
        Ok(pair) => pair,
        Err(_) => {
            // `request_message_id` should be the ClientHello envelope's
            // `message_id`, but `HandshakeSuccess` does not carry it and
            // changing `gateway.rs` is out of scope for this task. Decode the
            // original bytes to recover it; an empty string is used if decoding
            // fails. See completion report.
            let request_message_id = decode_envelope(&bytes)
                .map(|e| e.message_id)
                .unwrap_or_default();
            send_error(
                &mut socket,
                "AUTHENTICATION_REQUIRED",
                "realtime ticket is invalid".to_owned(),
                request_message_id,
                handshake.negotiated_minor,
                2,
                state.clock.as_ref(),
                "",
            )
            .await;
            return None;
        }
    };
    let permissions = resolve_world_permissions(state.world_authorizer.as_ref(), user).await;
    // Negotiate only on this production path, where the snapshot/live handoff
    // and committed command boundaries are implemented. Legacy clients keep
    // their existing hello and ignore the additive command field.
    if let Ok(request) = decode_envelope(&bytes) {
        if let Some(envelope::Payload::ClientHello(request)) = request.payload {
            if let Ok(mut response) = decode_envelope(&handshake.server_hello_bytes) {
                if let Some(envelope::Payload::ServerHello(hello)) = response.payload.as_mut() {
                    negotiate_state_sync(&request, hello);
                    handshake.server_hello_bytes = response.encode_to_vec();
                }
            }
        }
    }
    let viewer_roles = resolve_viewer_roles(state.identity_repository.as_deref(), user).await;
    conn_state = handshake.next_state;
    let shutdown_notified = false;
    if state.shutdown.is_rejecting() {
        send_error(
            &mut socket,
            "SERVER_SHUTTING_DOWN",
            "server is shutting down; please reconnect".to_owned(),
            String::new(),
            handshake.negotiated_minor,
            2,
            state.clock.as_ref(),
            "",
        )
        .await;
        return None;
    }
    // Reason: best-effort send on potentially closed socket; failure means peer already disconnected and is not actionable (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = socket
        .write(Message::Binary(handshake.server_hello_bytes.clone().into()))
        .await;

    // W-18: capture ClientHello resume_token hint (optional_binding_hint, 6.1).
    let client_hello_resume_token = decode_envelope(&bytes)
        .ok()
        .and_then(|e| match e.payload {
            Some(envelope::Payload::ClientHello(h)) => Some(h.resume_token),
            _ => None,
        })
        .unwrap_or_default();

    Some(AuthenticatedConnection {
        socket,
        state,
        shutdown_rx,
        conn_state,
        handshake,
        bytes: bytes.to_vec(),
        heartbeat_policy,
        user,
        session_id,
        permissions,
        viewer_roles,
        shutdown_notified,
        client_hello_resume_token,
    })
}
