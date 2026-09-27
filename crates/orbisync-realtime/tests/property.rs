//! Property tests for realtime sequence assignment.

use orbisync_protocol::v1::{Envelope, ErrorMessage, Heartbeat, Snapshot, envelope};
use orbisync_realtime::gateway::{decode_envelope, encode_envelope};
use orbisync_realtime::heartbeat::heartbeat_envelope;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

fn ascii_string() -> impl Strategy<Value = String> {
    prop::collection::vec(b'a'..=b'z', 0..=16)
        .prop_map(|bytes| String::from_utf8(bytes).unwrap_or_default())
}

fn payload() -> impl Strategy<Value = Option<envelope::Payload>> {
    prop_oneof![
        any::<i64>().prop_map(|client_time_unix_ms| {
            Some(envelope::Payload::Heartbeat(Heartbeat {
                client_time_unix_ms,
            }))
        }),
        prop::collection::vec(any::<u8>(), 0..=64).prop_map(|data| {
            Some(envelope::Payload::Snapshot(Snapshot {
                snapshot_id: "property".to_owned(),
                chunk_index: 0,
                chunk_count: 1,
                instance_revision: 1,
                data,
            }))
        }),
        (ascii_string(), ascii_string(), any::<bool>()).prop_map(|(code, message, retryable)| {
            Some(envelope::Payload::Error(ErrorMessage {
                code,
                message,
                request_message_id: "request".to_owned(),
                retryable,
            }))
        },),
    ]
}

proptest! {
    #[test]
    fn valid_envelope_round_trips_through_protobuf(
        protocol_major in any::<u32>(),
        protocol_minor in any::<u32>(),
        message_id in ascii_string(),
        sequence in any::<u64>(),
        sent_at_unix_ms in any::<i64>(),
        instance_id in ascii_string(),
        payload in payload(),
    ) {
        let original = Envelope {
            protocol_major,
            protocol_minor,
            message_id,
            sequence,
            sent_at_unix_ms,
            instance_id,
            payload,
        };
        let encoded = encode_envelope(&original)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let decoded = decode_envelope(&encoded)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(decoded, original);
    }

    #[test]
    fn heartbeat_sequences_remain_strictly_increasing(
        start in 0_u64..=(u64::MAX - 32_000),
        increments in prop::collection::vec(1_u64..=1_000, 1..=32),
    ) {
        let mut sequence = start;
        let mut previous = None;
        for increment in increments {
            sequence += increment;
            let envelope = heartbeat_envelope(
                0,
                "property-message".to_owned(),
                sequence,
                0,
                "property-instance".to_owned(),
                0,
            );
            prop_assert_eq!(envelope.sequence, sequence);
            if let Some(previous_sequence) = previous {
                prop_assert!(previous_sequence < envelope.sequence);
            }
            previous = Some(envelope.sequence);
        }
    }
}
