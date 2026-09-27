//! Contract test against the shared protocol test vectors.
//!
//! `test-vectors/protocol/v1/` is the cross-SDK golden data (`test-and-ci.md`
//! §2.3). This test builds the vector as generated Protocol Buffers types and
//! checks the encode → decode → encode round trip (`test-and-ci.md` §4.2
//! condition 6). Binary golden files arrive with the realtime implementation in
//! Milestone 2; the JSON vector is the contract that exists today.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use orbisync_domain::UserId;
use orbisync_protocol::v1::{ClientHello, Envelope, envelope::Payload};
use prost::Message as _;

fn vector(name: &str) -> serde_json::Value {
    let path =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"))
            .join("../../test-vectors/protocol/v1")
            .join(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&raw).expect("test vector must be valid JSON")
}

#[test]
fn test_envelope_valid_vector_round_trips() {
    let value = vector("envelope-valid.json");
    let envelope_json = &value["envelope"];
    let hello_json = &envelope_json["client_hello"];

    let envelope = Envelope {
        protocol_major: envelope_json["protocol_major"].as_u64().unwrap() as u32,
        protocol_minor: envelope_json["protocol_minor"].as_u64().unwrap() as u32,
        message_id: envelope_json["message_id"].as_str().unwrap().to_owned(),
        sequence: envelope_json["sequence"].as_u64().unwrap(),
        sent_at_unix_ms: envelope_json["sent_at_unix_ms"].as_i64().unwrap(),
        instance_id: String::new(),
        payload: Some(Payload::ClientHello(ClientHello {
            supported_minor_min: hello_json["supported_minor_min"].as_u64().unwrap() as u32,
            supported_minor_max: hello_json["supported_minor_max"].as_u64().unwrap() as u32,
            realtime_ticket: hello_json["realtime_ticket"].as_str().unwrap().to_owned(),
            client_name: hello_json["client_name"].as_str().unwrap().to_owned(),
            client_version: hello_json["client_version"].as_str().unwrap().to_owned(),
            client_type: hello_json["client_type"].as_str().unwrap().to_owned(),
            supported_compressions: Vec::new(),
            supported_features: Vec::new(),
            resume_token: hello_json["resume_token"].as_str().unwrap().to_owned(),
        })),
    };

    let encoded = envelope.encode_to_vec();
    let decoded = Envelope::decode(encoded.as_slice()).expect("encoded envelope must decode");
    assert_eq!(decoded, envelope);
    assert_eq!(decoded.encode_to_vec(), encoded);
}

#[test]
fn test_envelope_vector_matches_negotiated_protocol_major() {
    let value = vector("envelope-valid.json");
    assert_eq!(
        value["envelope"]["protocol_major"].as_u64().unwrap() as u32,
        orbisync_protocol::PROTOCOL_MAJOR
    );
    assert_eq!(
        value["subprotocol"].as_str().unwrap(),
        orbisync_protocol::WEBSOCKET_SUBPROTOCOL
    );
}

#[test]
fn test_envelope_message_id_is_a_domain_identifier() {
    let value = vector("envelope-valid.json");
    let message_id = value["envelope"]["message_id"].as_str().unwrap();
    UserId::parse(message_id).expect("vector identifiers must be canonical UUIDv7 strings");
}

#[test]
fn test_unknown_fields_are_tolerated_on_decode() {
    // Specification §34.4: unknown fields must not break decoding. Field 999 is
    // not part of the contract; a v1 decoder must skip it.
    let mut encoded = Envelope {
        protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: UserId::generate().to_string(),
        sequence: 1,
        sent_at_unix_ms: 1_785_481_200_000,
        instance_id: String::new(),
        payload: None,
    }
    .encode_to_vec();
    prost::encoding::uint32::encode(999, &7_u32, &mut encoded);

    let decoded = Envelope::decode(encoded.as_slice()).expect("unknown fields must be skipped");
    assert_eq!(decoded.sequence, 1);
}

#[test]
fn transform_wire_coordinates_use_f32() {
    let transform = orbisync_protocol::v1::Transform {
        position_x: 1.0_f32,
        position_y: 2.0_f32,
        position_z: 3.0_f32,
        rotation_x: 0.0_f32,
        rotation_y: 0.0_f32,
        rotation_z: 0.0_f32,
        rotation_w: 1.0_f32,
    };
    let _: f32 = transform.position_x;
    let _: f32 = transform.rotation_w;
}

#[test]
fn join_accepted_contains_existing_join_state_types() {
    let accepted = orbisync_protocol::v1::JoinAccepted {
        presence_id: "presence".to_owned(),
        instance_revision: 7,
        resume_token: "resume".to_owned(),
        user_id: "user".to_owned(),
        instance_id: "instance".to_owned(),
        entity_spawn: true,
        entity_update_own: true,
        entity_update_any: false,
        nearby_entities: Vec::new(),
        nearby_presence_ids: vec!["presence".to_owned()],
        nearby_user_ids: vec!["user".to_owned()],
        server_time_unix_ms: 1_700_000_000_000,
    };
    assert_eq!(accepted.nearby_presence_ids, vec!["presence"]);
    assert_eq!(accepted.nearby_user_ids, vec!["user"]);
    assert_eq!(accepted.server_time_unix_ms, 1_700_000_000_000);
}
