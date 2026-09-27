use axum::Error;
use axum::extract::ws::{Message, WebSocket};
use orbisync_realtime::gateway::{
    GatewayError, check_message_size, check_message_size_for_envelope, decode_envelope,
};

/// Details retained when an outbound message is rejected before reaching the
/// WebSocket transport. The connection loop uses this to send the specified
/// `ErrorMessage` before disconnecting.
#[derive(Debug, Clone, Copy)]
pub(super) struct MessageSizeViolation {
    pub(super) max_bytes: u64,
    pub(super) got_bytes: usize,
}

/// Checks a binary message at the outbound WebSocket boundary.
///
/// This is deliberately separate from the inbound connection checks so the
/// send path cannot accidentally rely on receive-side validation.
fn check_outbound_message_size(
    message: &Message,
    max_normal_message_bytes: u64,
    max_message_bytes: u64,
) -> Result<(), GatewayError> {
    let Message::Binary(bytes) = message else {
        return Ok(());
    };

    match decode_envelope(bytes) {
        Ok(envelope) => check_message_size_for_envelope(
            bytes,
            &envelope,
            max_normal_message_bytes,
            max_message_bytes,
        ),
        Err(_) => check_message_size(bytes, max_message_bytes),
    }
}

/// The only owner of the underlying WebSocket write operation.
///
/// Connection code can receive and write messages through this boundary, but
/// it cannot access the wrapped socket directly. Keeping the transport write
/// here gives the realtime connection a single ordering point without adding
/// another queue or buffer.
pub(super) struct RealtimeSocket {
    inner: SocketTransport,
    max_normal_message_bytes: u64,
    max_message_bytes: u64,
    size_violation: Option<MessageSizeViolation>,
}

// Production has one variant. Keep its existing inline allocation/layout;
// the small controlled transport exists only in test builds.
#[cfg_attr(test, allow(clippy::large_enum_variant))]
enum SocketTransport {
    WebSocket(WebSocket),
    #[cfg(test)]
    Controlled {
        inbound: tokio::sync::mpsc::Receiver<Message>,
        outbound: tokio::sync::mpsc::Sender<Message>,
    },
}

impl RealtimeSocket {
    /// Wraps an upgraded Axum WebSocket.
    pub(super) fn new(
        inner: WebSocket,
        max_normal_message_bytes: u64,
        max_message_bytes: u64,
    ) -> Self {
        Self {
            inner: SocketTransport::WebSocket(inner),
            max_normal_message_bytes,
            max_message_bytes,
            size_violation: None,
        }
    }

    /// Reads the next message from the peer.
    pub(super) async fn recv(&mut self) -> Option<Result<Message, Error>> {
        match &mut self.inner {
            SocketTransport::WebSocket(socket) => socket.recv().await,
            #[cfg(test)]
            SocketTransport::Controlled { inbound, .. } => inbound.recv().await.map(Ok),
        }
    }

    #[cfg(test)]
    pub(super) fn controlled() -> (
        Self,
        tokio::sync::mpsc::Sender<Message>,
        tokio::sync::mpsc::Receiver<Message>,
    ) {
        let (input, inbound) = tokio::sync::mpsc::channel(8);
        let (outbound, output) = tokio::sync::mpsc::channel(8);
        (
            Self {
                inner: SocketTransport::Controlled { inbound, outbound },
                max_normal_message_bytes: 16_384,
                max_message_bytes: 65_536,
                size_violation: None,
            },
            input,
            output,
        )
    }

    /// Writes one message to the peer.
    pub(super) async fn write(&mut self, message: Message) -> Result<(), Error> {
        if let Err(GatewayError::OversizedMessage {
            max_bytes,
            got_bytes,
        }) = check_outbound_message_size(
            &message,
            self.max_normal_message_bytes,
            self.max_message_bytes,
        ) {
            self.size_violation = Some(MessageSizeViolation {
                max_bytes,
                got_bytes,
            });
            return Err(Error::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "outbound message exceeds configured limit",
            )));
        }
        match &mut self.inner {
            SocketTransport::WebSocket(socket) => socket.send(message).await,
            #[cfg(test)]
            SocketTransport::Controlled { outbound, .. } => outbound
                .send(message)
                .await
                .map_err(|error| Error::new(std::io::Error::other(error.to_string()))),
        }
    }

    /// Takes the most recent outbound size violation, if any.
    pub(super) fn take_size_violation(&mut self) -> Option<MessageSizeViolation> {
        self.size_violation.take()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::check_outbound_message_size;
    use orbisync_protocol::v1::{Envelope, Snapshot, envelope};
    use prost::Message as _;

    const NORMAL_LIMIT: u64 = 16_384;
    const TRANSPORT_LIMIT: u64 = 65_536;

    #[test]
    fn outbound_normal_message_limit_is_checked_independently() {
        let envelope = Envelope {
            payload: Some(envelope::Payload::Snapshot(Snapshot {
                data: vec![0; NORMAL_LIMIT as usize + 1],
                ..Snapshot::default()
            })),
            ..Envelope::default()
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).unwrap();

        assert!(
            check_outbound_message_size(
                &axum::extract::ws::Message::Binary(bytes.into()),
                NORMAL_LIMIT,
                TRANSPORT_LIMIT,
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_transport_ceiling_is_checked_independently() {
        let envelope = Envelope {
            payload: Some(envelope::Payload::Snapshot(Snapshot {
                snapshot_id: "x".repeat(TRANSPORT_LIMIT as usize),
                data: vec![0],
                ..Snapshot::default()
            })),
            ..Envelope::default()
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).unwrap();
        assert!(bytes.len() > TRANSPORT_LIMIT as usize);

        assert!(
            check_outbound_message_size(
                &axum::extract::ws::Message::Binary(bytes.into()),
                NORMAL_LIMIT,
                TRANSPORT_LIMIT,
            )
            .is_err()
        );
    }
}
