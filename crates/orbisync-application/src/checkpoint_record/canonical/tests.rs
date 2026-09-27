#![allow(clippy::unwrap_used)]
use super::*;

fn record(
    bytes: &[u8],
    kind: reader::RecordKind,
    window: usize,
) -> Result<(reader::Record, [u8; 32]), ScalarError> {
    let mut decoder = reader::RecordDecoder::new(kind);
    let flag = AtomicBool::new(false);
    let mut offset = 0;
    loop {
        let end = (offset + window).min(bytes.len());
        let step = decoder.step(&bytes[offset..end], WORK_QUANTUM, &flag)?;
        assert!(step.work <= WORK_QUANTUM);
        offset += step.consumed;
        match step.progress {
            reader::Progress::Ready { record, digest } => {
                assert_eq!(offset, bytes.len());
                return Ok((record, digest));
            }
            reader::Progress::Input if offset == bytes.len() => return Err(ScalarError::Invalid),
            _ => {}
        }
    }
}

#[test]
fn canonical_record_reader_entity_golden_and_closed_schema() {
    use sha2::{Digest, Sha256};
    let entity = include_bytes!("../../../../../test-vectors/checkpoint-codec5-entity.json");
    for window in [1, 2, 7, 16384] {
        let (value, digest) = record(entity, reader::RecordKind::Entity, window).unwrap();
        assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(entity)));
        assert_eq!(
            digest.map(|byte| format!("{byte:02x}")).concat(),
            "7daaf0d1ed8dbfb64468e03a0a33d2a3a1257b007c1cdb07f53278a064d1ce5a"
        );
        let reader::Record::Entity(value) = value else {
            unreachable!()
        };
        assert_eq!(value.components()["test.bytes"], [0, 255, 123, 34]);
        let mut emitted = Vec::new();
        let count = emit::entity(&value, &mut emitted, &AtomicBool::new(false)).unwrap();
        assert_eq!(emitted, entity);
        assert_eq!(count, entity.len());
        assert_eq!(
            emit::entity(&value, io::sink(), &AtomicBool::new(false)).unwrap(),
            count
        );
    }
    let text = std::str::from_utf8(entity).unwrap();
    for invalid in [
        text.replace("\"revision\":1", "\"revision\":01"),
        text.replace("\"revision\":1", "\"revision\":1,\"revision\":1"),
        text.replace(
            "\"owner\":null,\"transform\":null",
            "\"transform\":null,\"owner\":null",
        ),
        text.replace("\"global\"", "\"global\",\"opaque\":{}"),
        text.replace("[0,255,123,34]", "[256]"),
        text.replace(
            "\"test.bytes\":[0,255,123,34]",
            "\"test.z\":[],\"test.a\":[]",
        ),
        text.replace("\"test.bytes\"", "\"core.bytes\""),
    ] {
        assert!(
            record(invalid.as_bytes(), reader::RecordKind::Entity, 1).is_err(),
            "{invalid}"
        );
    }
}

fn receipt_bytes(size: usize) -> Vec<u8> {
    let payload = vec![255; size];
    let fingerprint = [7; 32];
    let id = "01900000-0000-7000-8000-000000000001";
    let mut bytes = Vec::new();
    let count = emit::receipt(
        emit::ReceiptRef {
            command_id: id,
            message_id: id,
            fingerprint: &fingerprint,
            created_at_millis: i64::MIN,
            expires_at_millis: i64::MIN + 86_400_000,
            result: emit::ResultRef::Applied(&payload),
        },
        &mut bytes,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(count, bytes.len());
    bytes
}

#[test]
fn canonical_record_maximum_receipt_resources_and_linear_work() {
    use sha2::{Digest, Sha256};
    let mut totals = Vec::new();
    for size in [131072, super::super::RESPONSE_BYTES] {
        let bytes = receipt_bytes(size);
        let mut decoder = reader::RecordDecoder::new(reader::RecordKind::Receipt);
        let flag = AtomicBool::new(false);
        let mut offset = 0;
        let receipt = loop {
            let step = decoder
                .step(
                    &bytes[offset..(offset + 137).min(bytes.len())],
                    WORK_QUANTUM,
                    &flag,
                )
                .unwrap();
            offset += step.consumed;
            if let reader::Progress::Ready { record, digest } = step.progress {
                assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&bytes)));
                let reader::Record::Receipt(receipt) = record else {
                    unreachable!()
                };
                break receipt;
            }
        };
        let stats = decoder.stats();
        assert_eq!(stats.encoded_bytes, bytes.len());
        assert!(stats.scratch_capacity_bound < 400_000, "{stats:?}");
        assert!(stats.max_step_work <= WORK_QUANTUM);
        let crate::checkpoint_record::ReceiptResult::Applied { response_payload } = receipt.result
        else {
            unreachable!()
        };
        assert_eq!(response_payload, vec![255; size]);
        assert_eq!(receipt.created_at_millis, i64::MIN);
        assert_eq!(receipt.expires_at_millis, i64::MIN + 86_400_000);
        println!("receipt payload={size}; {stats:?}; allocator bookkeeping/native stack excluded");
        totals.push(stats.total_work);
    }
    assert!(totals[1] <= 2 * totals[0]);
    assert!(totals[1] > totals[0]);
    assert!(
        record(
            &receipt_bytes(super::super::RESPONSE_BYTES + 1),
            reader::RecordKind::Receipt,
            137
        )
        .is_err()
    );
}

#[test]
fn canonical_record_cancellation_and_receipt_alternate_fields() {
    let bytes = receipt_bytes(4096);
    for cutoff in [0, 1, 23, 256, bytes.len() - 3] {
        let mut decoder = reader::RecordDecoder::new(reader::RecordKind::Receipt);
        let flag = AtomicBool::new(false);
        let mut offset = 0;
        while offset < cutoff {
            let step = decoder
                .step(&bytes[offset..cutoff], WORK_QUANTUM, &flag)
                .unwrap();
            offset += step.consumed;
        }
        flag.store(true, Ordering::Release);
        assert_eq!(
            decoder.step(&[], WORK_QUANTUM, &flag).unwrap_err(),
            ScalarError::Cancelled
        );
        flag.store(false, Ordering::Release);
        assert_eq!(
            decoder.step(&[], WORK_QUANTUM, &flag).unwrap_err(),
            ScalarError::Cancelled
        );
    }
    let text = String::from_utf8(receipt_bytes(1)).unwrap();
    for invalid in [
        text.replace(
            "\"response_payload\":[255]",
            "\"response_payload\":[255],\"opaque\":null",
        ),
        text.replace(
            "\"type\":\"applied\",\"response_payload\":[255]",
            "\"response_payload\":[255],\"type\":\"applied\"",
        ),
        text.replace(
            "\"response_payload\":[255]",
            "\"response_payload\":[255],\"response_payload\":[255]",
        ),
        text.replace("[255]", "[255.0]"),
    ] {
        assert!(record(invalid.as_bytes(), reader::RecordKind::Receipt, 1).is_err());
    }
}

#[test]
fn canonical_record_domain_shapes_and_unicode_rejection_result() {
    use orbisync_domain::*;
    let time = Timestamp::from_unix_millis(1000).unwrap();
    let transform = Transform::new(
        Vec3::new(-0.0, 1_000_000.0, -1_000_000.0).unwrap(),
        Quaternion::new(f32::MAX, f32::MIN, -0.0, f32::from_bits(1)).unwrap(),
        Vec3::new(1.0, 1000.0, 0.000001).unwrap(),
    )
    .unwrap();
    let roles = (0..16).map(|_| RoleId::generate()).collect::<Vec<_>>();
    let users = (0..64).map(|_| UserId::generate()).collect::<Vec<_>>();
    for visibility in [
        VisibilityPolicy::Global,
        VisibilityPolicy::OwnerOnly,
        VisibilityPolicy::spatial(10_000.0).unwrap(),
        VisibilityPolicy::custom("test.custom").unwrap(),
        VisibilityPolicy::role_restricted(roles).unwrap(),
        VisibilityPolicy::explicit(users).unwrap(),
    ] {
        let entity = Entity::from_persisted(
            EntityId::generate(),
            InstanceId::generate(),
            EntityKind::Trigger,
            Some(UserId::generate()),
            Some(transform),
            visibility,
            Revision::from_u64(u64::MAX),
            time,
            time,
            (0..16)
                .rev()
                .map(|i| {
                    (
                        format!("test.{}{i:02}", "a".repeat(121)),
                        (0..4096).map(|n| (n % 256) as u8).collect(),
                    )
                })
                .collect(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        emit::entity(&entity, &mut bytes, &AtomicBool::new(false)).unwrap();
        let (reader::Record::Entity(restored), _) =
            record(&bytes, reader::RecordKind::Entity, 137).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(restored, entity);
        let mut reencoded = Vec::new();
        emit::entity(&restored, &mut reencoded, &AtomicBool::new(false)).unwrap();
        assert_eq!(reencoded, bytes);
    }
    let id = "01900000-0000-7000-8000-000000000001";
    let detail = format!("{}🦀", "\0".repeat(8188));
    let mut bytes = Vec::new();
    emit::receipt(
        emit::ReceiptRef {
            command_id: id,
            message_id: id,
            fingerprint: &[255; 32],
            created_at_millis: i64::MAX - 86_400_000,
            expires_at_millis: i64::MAX,
            result: emit::ResultRef::Rejected {
                code: "é\0",
                detail: &detail,
            },
        },
        &mut bytes,
        &AtomicBool::new(false),
    )
    .unwrap();
    let (reader::Record::Receipt(restored), _) =
        record(&bytes, reader::RecordKind::Receipt, 1).unwrap()
    else {
        unreachable!()
    };
    let crate::checkpoint_record::ReceiptResult::Rejected {
        code,
        detail: restored_detail,
    } = restored.result
    else {
        unreachable!()
    };
    assert_eq!(code, "é\0");
    assert_eq!(restored_detail, detail);
    assert_eq!(restored.expires_at_millis, i64::MAX);
}

fn decode(bytes: &[u8], kind: NumberKind) -> Result<Number, ScalarError> {
    let cancel = AtomicBool::new(false);
    let mut reader = NumberReader::default();
    for &byte in bytes {
        reader.push(byte, &cancel)?;
    }
    reader.finish(kind, &cancel)
}

#[test]
fn integer_golden_and_noncanonical_rejection() {
    for (value, golden, kind) in [
        (
            Number::Unsigned(u64::MAX),
            "18446744073709551615",
            NumberKind::Unsigned,
        ),
        (
            Number::Signed(i64::MIN),
            "-9223372036854775808",
            NumberKind::Signed,
        ),
        (
            Number::Signed(i64::MAX),
            "9223372036854775807",
            NumberKind::Signed,
        ),
        (Number::Unsigned(0), "0", NumberKind::Unsigned),
    ] {
        assert_eq!(Scalar::number(value).unwrap().as_bytes(), golden.as_bytes());
        assert_eq!(decode(golden.as_bytes(), kind).unwrap(), value);
    }
    for spelling in ["00", "-0", "+1", "1.0", "1e0", "18446744073709551616", " 1"] {
        assert!(
            decode(spelling.as_bytes(), NumberKind::Unsigned).is_err(),
            "{spelling}"
        );
    }
    for spelling in [
        "-0",
        "00",
        "+1",
        "9223372036854775808",
        "-9223372036854775809",
    ] {
        assert!(
            decode(spelling.as_bytes(), NumberKind::Signed).is_err(),
            "{spelling}"
        );
    }
}

#[test]
fn finite_float_bound_and_roundtrip_every_exponent() {
    // Every exponent, both signs and boundary/mid mantissas. The source-level
    // decimal-width argument is the bound; this is a formatter regression
    // matrix, not an assertion that sampling proves all 2^32 bit patterns.
    for exponent in 0..255 {
        for mantissa in [0, 1, 2, 0x3fffff, 0x400000, 0x7ffffe, 0x7fffff] {
            for sign in [0, 0x80000000] {
                let number = f32::from_bits(sign | (exponent << 23) | mantissa);
                let scalar = Scalar::number(Number::Float(number)).unwrap();
                assert!(scalar.as_bytes().len() <= SCALAR_BYTES);
                let Number::Float(restored) = decode(scalar.as_bytes(), NumberKind::Float).unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(number.to_bits(), restored.to_bits());
            }
        }
    }
    assert_eq!(
        Scalar::number(Number::Float(-0.0)).unwrap().as_bytes(),
        b"-0.0"
    );
    assert_eq!(
        Scalar::number(Number::Float(f32::MAX)).unwrap().as_bytes(),
        b"3.4028235e+38"
    );
    assert_eq!(
        Scalar::number(Number::Float(f32::from_bits(1)))
            .unwrap()
            .as_bytes(),
        b"1e-45"
    );
    for spelling in ["0", "1", "1.00", "1E0", "1e400", "NaN", "null", "-0"] {
        assert!(
            decode(spelling.as_bytes(), NumberKind::Float).is_err(),
            "{spelling}"
        );
    }
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(Scalar::number(Number::Float(value)).is_err());
    }
}

#[test]
fn long_numbers_fail_at_cap_and_cancellation_is_terminal() {
    let flag = AtomicBool::new(false);
    let mut reader = NumberReader::default();
    for _ in 0..SCALAR_BYTES {
        reader.push(b'1', &flag).unwrap();
    }
    assert_eq!(reader.push(b'1', &flag), Err(ScalarError::Invalid));
    assert_eq!(
        reader.finish(NumberKind::Float, &flag),
        Err(ScalarError::Invalid)
    );
    let mut reader = NumberReader::default();
    flag.store(true, Ordering::Release);
    assert_eq!(reader.push(b'1', &flag), Err(ScalarError::Cancelled));
    flag.store(false, Ordering::Release);
    assert_eq!(reader.push(b'1', &flag), Err(ScalarError::Cancelled));
}

#[test]
fn string_emission_matches_serde_at_tiny_boundaries() {
    let all_controls: String = (0u8..=0x7f).map(char::from).collect();
    for input in [
        "",
        "譌･譛ｬ隱橇洶/ﾃｩ\u{2028}",
        &all_controls,
        &"\0\nｦ".repeat(8192),
    ] {
        let expected = serde_json::to_vec(input).unwrap();
        for window in [1, 2, 3, 7, 127] {
            for budget in [8, 9, 10, 17, WORK_QUANTUM] {
                let mut cursor = StringEmitter::new(input);
                let mut output = vec![0; window];
                let mut actual = Vec::new();
                loop {
                    let step = cursor
                        .step(&mut output, budget, &AtomicBool::new(false))
                        .unwrap();
                    assert!(step.work <= budget);
                    actual.extend_from_slice(&output[..step.written]);
                    if step.done {
                        break;
                    }
                    assert!(step.work > 0);
                }
                assert_eq!(actual, expected);
            }
        }
    }
}

#[test]
fn string_cancel_and_small_budget_do_not_advance() {
    let flag = AtomicBool::new(false);
    let mut cursor = StringEmitter::new("text");
    let mut output = [0; 16];
    let step = cursor.step(&mut output, 0, &flag).unwrap();
    assert_eq!((step.written, step.work, step.done), (0, 0, false));
    flag.store(true, Ordering::Release);
    assert_eq!(
        cursor.step(&mut output, 100, &flag).unwrap_err(),
        ScalarError::Cancelled
    );
    flag.store(false, Ordering::Release);
    assert_eq!(
        cursor.step(&mut output, 100, &flag).unwrap_err(),
        ScalarError::Cancelled
    );
}

#[test]
fn string_reader_canonical_utf8_bounds_and_ownership() {
    let cancel = AtomicBool::new(false);
    let controls: String = (0..32).map(char::from).collect();
    for text in ["日本語🦀é\u{2028}/", &controls, &"x".repeat(8192)] {
        let bytes = serde_json::to_vec(text).unwrap();
        let mut reader = StringReader::new(text.len()).unwrap();
        let scratch = reader.scratch_bytes();
        for (index, &byte) in bytes.iter().enumerate() {
            assert_eq!(
                reader.push(byte, &cancel).unwrap(),
                index + 1 == bytes.len()
            );
            assert_eq!(reader.scratch_bytes(), scratch);
        }
        assert_eq!(reader.finish(&cancel).unwrap(), text);
    }
    assert!(StringReader::new(8193).is_err());
    for bytes in [
        &b"\"\\u0041\""[..],
        b"\"\\u000a\"",
        b"\"\\u001F\"",
        b"\"\\/\"",
        b"\"\\ud83e\\udd80\"",
        b"\"\xc0\x80\"",
        b"\"\xed\xa0\x80\"",
        b"\"\xf4\x90\x80\x80\"",
        b"\"\x80\"",
        b"\"\n\"",
        b"\"\xf0\x9f",
        b"\"\\u00",
        b"\"\\",
        b"\"aaaa\"",
    ] {
        let mut reader = StringReader::new(3).unwrap();
        let mut failed = false;
        for &byte in bytes {
            if reader.push(byte, &cancel).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed || reader.finish(&cancel).is_err(), "{bytes:?}");
    }
    let mut reader = StringReader::new(1).unwrap();
    cancel.store(true, Ordering::Release);
    assert_eq!(reader.push(b'"', &cancel), Err(ScalarError::Cancelled));
    cancel.store(false, Ordering::Release);
    assert_eq!(reader.push(b'"', &cancel), Err(ScalarError::Cancelled));
}
