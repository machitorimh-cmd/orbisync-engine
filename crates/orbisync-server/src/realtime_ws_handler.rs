/// Axum handler for `GET /ws` and `GET /v1/realtime/ws` (C3).
pub async fn realtime_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<RealtimeState>>,
) -> impl IntoResponse {
    if state.shutdown.is_rejecting() {
        tracing::info!(
            event = "realtime.upgrade_rejected",
            reason = "server_draining",
            "rejected WebSocket upgrade during graceful shutdown"
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // C3: configure codec limits from config (not hardcoded) before upgrade.
    // The codec enforces the 64 KiB transport ceiling, dropping oversized
    // frames before allocating and before handle_client_hello sees them.
    // The payload-specific normal 16 KiB ceiling is checked after decoding.
    let max = state.config.max_message_bytes as usize;
    let ws = ws
        .max_message_size(max)
        .max_frame_size(max)
        .protocols([orbisync_protocol::WEBSOCKET_SUBPROTOCOL]);
    let subprotocol = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // C3: semaphore per-connection, held for the lifetime of the socket.
    // If the semaphore is full, reject with 503 before allocating a task.
    let permit = match state.connection_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            state.rejected_upgrade_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                event = "realtime.upgrade_rejected",
                reason = "semaphore_full",
                active = state.active_connections.load(Ordering::Relaxed),
                max = state.config.max_connections,
                "rejected WebSocket upgrade: too many connections"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let state_for_upgrade = Arc::clone(&state);
    ws.on_upgrade(move |socket| async move {
        let _permit: OwnedSemaphorePermit = permit;
        // C3: active connection metric.
        state_for_upgrade
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        state_for_upgrade
            .metrics_recorder
            .incr(Counter::WebsocketConnectionsTotal);
        let active = state_for_upgrade.active_connections.load(Ordering::Relaxed);
        state_for_upgrade.metrics_recorder.set(
            Gauge::WebsocketConnectionsCurrent,
            i64::try_from(active).unwrap_or(i64::MAX),
        );
        tracing::info!(
            event = "realtime.connection_opened",
            active = state_for_upgrade.active_connections.load(Ordering::Relaxed),
            max = state_for_upgrade.config.max_connections,
        );
        let _active_guard = ActiveConnectionGuard {
            active: Arc::clone(&state_for_upgrade.active_connections),
            metrics: Arc::clone(&state_for_upgrade.metrics_recorder),
        };
        handle_socket(
            RealtimeSocket::new(
                socket,
                state_for_upgrade.config.max_normal_message_bytes,
                state_for_upgrade.config.max_message_bytes,
            ),
            state_for_upgrade,
            subprotocol,
        )
        .await;
        tracing::info!(
            event = "realtime.connection_closed",
            active = _active_guard.active.load(Ordering::Relaxed),
        );
        // _permit and _active_guard dropped here, releasing semaphore and decrementing.
    })
}
