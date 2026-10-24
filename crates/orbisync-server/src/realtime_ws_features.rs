const DIRECTED_EVENTS_FEATURE: &str = "orbisync.directed-events.v1";
const ACTIVITY_FEATURE: &str = "orbisync.activity.v1";

fn negotiate_collaboration_features(
    request: &orbisync_protocol::v1::ClientHello,
    response: &mut orbisync_protocol::v1::ServerHello,
) {
    for feature in [DIRECTED_EVENTS_FEATURE, ACTIVITY_FEATURE] {
        if request
            .supported_features
            .iter()
            .any(|supported| supported == feature)
        {
            response.enabled_features.push(feature.to_owned());
        }
    }
}

fn negotiated_feature(
    handshake: &orbisync_realtime::gateway::HandshakeSuccess,
    feature: &str,
) -> bool {
    decode_envelope(&handshake.server_hello_bytes)
        .ok()
        .is_some_and(|envelope| {
            matches!(envelope.payload, Some(envelope::Payload::ServerHello(hello))
            if hello.enabled_features.iter().any(|enabled| enabled == feature))
        })
}

struct ActivityPresenceGuard {
    state: Arc<RealtimeState>,
    sender: ActivitySender,
}

impl Drop for ActivityPresenceGuard {
    fn drop(&mut self) {
        clear_activity_presence(&self.state, self.sender);
    }
}

impl RealtimeState {
    /// Expire temporary activity and publish removals from an owned maintenance task.
    pub fn sweep_activities(&self) {
        sweep_activities(self);
    }
}

#[cfg(test)]
mod collaboration_feature_tests {
    use super::*;

    #[test]
    fn collaboration_features_require_explicit_negotiation() {
        let mut response = orbisync_protocol::v1::ServerHello::default();
        negotiate_collaboration_features(
            &orbisync_protocol::v1::ClientHello::default(),
            &mut response,
        );
        assert!(response.enabled_features.is_empty());
        let request = orbisync_protocol::v1::ClientHello {
            supported_features: vec![
                DIRECTED_EVENTS_FEATURE.to_owned(),
                ACTIVITY_FEATURE.to_owned(),
            ],
            ..Default::default()
        };
        negotiate_collaboration_features(&request, &mut response);
        assert_eq!(
            response.enabled_features,
            [DIRECTED_EVENTS_FEATURE, ACTIVITY_FEATURE]
        );
    }
}
