//! Mock server for local performance measurement.

use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use prost::Message as ProstMessage;

const PROTOCOL_MAJOR: u32 = 1;
const WEBSOCKET_SUBPROTOCOL: &str = "orbisync.v1.protobuf";

#[allow(missing_docs, clippy::all, clippy::pedantic, unused_qualifications)]
mod realtime {
    include!(concat!(env!("OUT_DIR"), "/orbisync.v1.rs"));
}

use realtime::{EntityState, Envelope, JoinAccepted, ServerHello, Snapshot, StateDelta, envelope};

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn encode_envelope(env: &Envelope) -> Vec<u8> {
    let mut b = Vec::new();
    env.encode(&mut b).unwrap();
    b
}
async fn login() -> impl IntoResponse {
    let body = serde_json::json!({
        "access_token": "mock-access-token",
        "token_type": "Bearer",
        "expires_in": 900
    });
    (StatusCode::OK, axum::Json(body))
}
async fn ticket() -> impl IntoResponse {
    let body = serde_json::json!({
        "realtime_ticket": "mock-realtime-ticket",
        "expires_in": 60
    });
    (StatusCode::OK, axum::Json(body))
}
async fn world() -> impl IntoResponse {
    let body = serde_json::json!({
        "id": uuid::Uuid::now_v7().to_string(),
        "name": "mock-world",
        "revision": 1,
        "status": "running"
    });
    (StatusCode::CREATED, axum::Json(body))
}
async fn instance() -> impl IntoResponse {
    let body = serde_json::json!({
        "id": uuid::Uuid::now_v7().to_string(),
        "world_id": uuid::Uuid::now_v7().to_string(),
        "status": "running",
        "revision": 1
    });
    (StatusCode::CREATED, axum::Json(body))
}
async fn metrics() -> impl IntoResponse {
    let body = concat!(
        "# mock implementation of the metrics exported by the server\n",
        "process_cpu_seconds_total 1.0\n",
        "process_resident_memory_bytes 1024\n",
        "instance_command_queue_depth{queue=\"control\"} 0\n",
        "instance_mailbox_saturated_total 0\n",
        "instance_mailbox_dropped_total 0\n",
        "http_requests_total{method=\"GET\",status=\"200\"} 1\n",
        "http_request_duration_seconds{method=\"GET\"} 0.001\n",
        "auth_login_failures_total 0\n",
        "rate_limit_rejected_total{scope=\"login\"} 0\n",
        "db_query_duration_seconds 0.001\n",
        "extension_delivery_total{result=\"ok\"} 0\n",
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}
async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.protocols([WEBSOCKET_SUBPROTOCOL]).on_upgrade(handle_ws)
}
async fn handle_ws(mut socket: WebSocket) {
    let msg = match socket.recv().await {
        Some(Ok(AxumWsMessage::Binary(b))) => b,
        _ => return,
    };
    let env = match Envelope::decode(msg.as_ref()) {
        Ok(e) => e,
        Err(_) => return,
    };
    if !matches!(env.payload, Some(envelope::Payload::ClientHello(_))) {
        return;
    }
    let hello = ServerHello {
        negotiated_minor: 0,
        connection_id: uuid::Uuid::now_v7().to_string(),
        heartbeat_interval_ms: 30000,
        server_time_unix_ms: current_unix_ms(),
        negotiated_compression: String::new(),
        enabled_features: Vec::new(),
    };
    let resp = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 1,
        sent_at_unix_ms: current_unix_ms(),
        instance_id: String::new(),
        payload: Some(envelope::Payload::ServerHello(hello)),
    };
    if socket
        .send(AxumWsMessage::Binary(encode_envelope(&resp).into()))
        .await
        .is_err()
    {
        return;
    }
    let msg = match socket.recv().await {
        Some(Ok(AxumWsMessage::Binary(b))) => b,
        _ => return,
    };
    let env = match Envelope::decode(msg.as_ref()) {
        Ok(e) => e,
        Err(_) => return,
    };
    let instance_id = match env.payload {
        Some(envelope::Payload::JoinInstance(ref j)) => j.world_instance_id.clone(),
        _ => return,
    };
    let ja = JoinAccepted {
        presence_id: uuid::Uuid::now_v7().to_string(),
        instance_revision: 1,
        resume_token: "mock-resume".to_string(),
        ..Default::default()
    };
    let env_ja = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 2,
        sent_at_unix_ms: current_unix_ms(),
        instance_id: instance_id.clone(),
        payload: Some(envelope::Payload::JoinAccepted(ja)),
    };
    let snap = Snapshot {
        snapshot_id: uuid::Uuid::now_v7().to_string(),
        chunk_index: 0,
        chunk_count: 1,
        instance_revision: 1,
        data: b"{}".to_vec(),
    };
    let env_snap = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 3,
        sent_at_unix_ms: current_unix_ms(),
        instance_id: instance_id.clone(),
        payload: Some(envelope::Payload::Snapshot(snap)),
    };
    let _ = socket
        .send(AxumWsMessage::Binary(encode_envelope(&env_ja).into()))
        .await;
    let _ = socket
        .send(AxumWsMessage::Binary(encode_envelope(&env_snap).into()))
        .await;
    let mut rev: u64 = 1;
    while let Some(Ok(AxumWsMessage::Binary(b))) = socket.recv().await {
        let env = match Envelope::decode(b.as_ref()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let Some(envelope::Payload::TransformInput(inp)) = env.payload {
            rev += 1;
            let state = EntityState {
                entity_id: inp.entity_id.clone(),
                revision: rev,
                transform: inp.transform,
                properties: None,
                ..Default::default()
            };
            let delta = StateDelta {
                from_revision: rev - 1,
                to_revision: rev,
                entities: vec![state],
            };
            let resp = Envelope {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: 0,
                message_id: uuid::Uuid::now_v7().to_string(),
                sequence: 4,
                sent_at_unix_ms: current_unix_ms(),
                instance_id: instance_id.clone(),
                payload: Some(envelope::Payload::StateDelta(delta)),
            };
            let _ = socket
                .send(AxumWsMessage::Binary(encode_envelope(&resp).into()))
                .await;
        }
    }
}
#[tokio::main]
async fn main() {
    let addr = std::env::var("MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:18080".to_string());
    let app = axum::Router::new()
        .route("/v1/auth/login", post(login))
        .route("/v1/realtime/tickets", post(ticket))
        .route("/v1/worlds", post(world))
        .route("/v1/instances", post(instance))
        .route("/metrics", get(metrics))
        .route("/ws", get(ws_handler))
        .route("/v1/realtime/ws", get(ws_handler))
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("mock perf server listening on http://{addr} ws://{addr}/ws");
    axum::serve(listener, app).await.unwrap();
}
