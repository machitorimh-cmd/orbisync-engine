#![allow(clippy::unwrap_used, missing_docs)]
use orbisync_application::checkpoint_record::canonical::{
    WORK_QUANTUM, emit,
    reader::{Progress, RecordDecoder, RecordKind},
};
use std::sync::atomic::{AtomicBool, Ordering};
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/checkpoint_resource_allocator.rs"
));

#[test]
fn r3_compaction_overlap_and_cancellation() {
    use orbisync_application::checkpoint_record::{ReceiptResult, canonical::reader::Record};
    std::thread::Builder::new().stack_size(128 * 1024).spawn(|| {
        for size in [1, 262143] {
            let payload = vec![255; size];
            let flag = AtomicBool::new(false);
            let mut bytes = Vec::new();
            let id = "01900000-0000-7000-8000-000000000001";
            emit::receipt(emit::ReceiptRef { command_id: id, message_id: id, fingerprint: &[7;32],
                created_at_millis: 0, expires_at_millis: 86400000, result: emit::ResultRef::Applied(&payload) },
                &mut bytes, &flag).unwrap();
            for mode in ["success", "cancel", "drop"] {
                if size == 1 && mode != "success" { continue; }
                flag.store(false, Ordering::Release);
                let mut scratch = 0;
                let (heap, stack) = tracking::measure(|| {
                    let mut decoder = RecordDecoder::new(RecordKind::Receipt);
                    let mut offset = 0;
                    loop {
                        let step = decoder.step(&bytes[offset..], WORK_QUANTUM, &flag).unwrap();
                        offset += step.consumed;
                        scratch = decoder.stats().scratch_capacity_bound;
                        if let Progress::Ready { record: Record::Receipt(receipt), .. } = step.progress {
                            assert_eq!(mode, "success");
                            let ReceiptResult::Applied { response_payload } = receipt.result else { panic!() };
                            assert_eq!(response_payload, payload);
                            assert_eq!(response_payload.capacity(), size);
                            assert_eq!(receipt.command_id.capacity(), receipt.command_id.len());
                            break;
                        }
                        // No input consumed while copying an already parsed array:
                        // interrupt with both old and compact buffers live.
                        if mode != "success" && offset >= bytes.len() - 2 && step.consumed == 0 {
                            if mode == "cancel" {
                                flag.store(true, Ordering::Release);
                                assert!(decoder.step(&[], WORK_QUANTUM, &flag).is_err());
                            }
                            break;
                        }
                    }
                });
                println!("R3 compact size={size} mode={mode} requested_overlap={heap} scratch_charge={scratch} native_sample={stack}");
                assert!(heap < 540000);
                assert!(scratch < 550000);
                assert!(stack < 128 * 1024);
                if size > 1 { assert!(heap > 524000); }
            }
        }
    }).unwrap().join().unwrap();
}

#[test]
fn resource_record_heap_stack_and_interrupted_drop() {
    // Finite stack execution is a separate observation, not a heap-counter
    // claim. Allocator callbacks sample deeper construction/poll/drop frames.
    std::thread::Builder::new().stack_size(128 * 1024).spawn(|| {
        let flag = AtomicBool::new(false);
        let payload = vec![255; 262144];
        let mut bytes = Vec::new();
        let id = "01900000-0000-7000-8000-000000000001";
        emit::receipt(emit::ReceiptRef { command_id: id, message_id: id, fingerprint: &[7;32],
            created_at_millis: 0, expires_at_millis: 86400000, result: emit::ResultRef::Applied(&payload) },
            &mut bytes, &flag).unwrap();
        for mode in ["success", "error", "cancel", "drop"] {
            flag.store(false, Ordering::Release);
            let (heap, stack) = tracking::measure(|| {
                let mut decoder = RecordDecoder::new(RecordKind::Receipt);
                let mut offset = 0;
                loop {
                    if mode != "success" && offset > bytes.len() / 2 {
                        match mode {
                            "cancel" => { flag.store(true, Ordering::Release); assert!(decoder.step(&[], WORK_QUANTUM, &flag).is_err()); }
                            "error" => { assert!(decoder.step(b"x", WORK_QUANTUM, &flag).is_err()); }
                            _ => {}
                        }
                        break;
                    }
                    let step = decoder.step(&bytes[offset..], WORK_QUANTUM, &flag).unwrap();
                    offset += step.consumed;
                    if let Progress::Ready { record, .. } = step.progress { drop(record); break; }
                }
            });
            println!("record {mode}: requested heap/overlap={heap}, sampled native stack={stack}, thread stack request=131072");
            assert!(heap < 280000);
            assert!(stack < 128 * 1024);
        }
    }).unwrap().join().unwrap();
}

#[test]
fn resource_maximum_entity_fields_and_drop() {
    use orbisync_domain::{
        Entity, EntityId, EntityKind, InstanceId, Revision, Timestamp, UserId, VisibilityPolicy,
    };
    std::thread::Builder::new().stack_size(128 * 1024).spawn(|| {
        let flag = AtomicBool::new(false);
        let timestamp = Timestamp::from_unix_millis(1000).unwrap();
        let users = (0..64u128).map(|n| UserId::new(uuid::Uuid::from_u128(0x01900000_0000_7000_8000_000000000000 | n)).unwrap());
        let entity = Entity::from_persisted(EntityId::generate(), InstanceId::generate(), EntityKind::Object,
            Some(UserId::generate()), None, VisibilityPolicy::explicit(users).unwrap(), Revision::from_u64(1), timestamp, timestamp,
            (0..16).map(|n| (format!("test.{n:03}{}", "x".repeat(120)), vec![255;4096])).collect()).unwrap();
        let mut bytes = Vec::new();
        emit::entity(&entity, &mut bytes, &flag).unwrap();
        for interrupted in [false, true] {
            let (heap, stack) = tracking::measure(|| {
                let mut decoder = RecordDecoder::new(RecordKind::Entity);
                let mut offset = 0;
                loop {
                    let step = decoder.step(&bytes[offset..], WORK_QUANTUM, &flag).unwrap();
                    offset += step.consumed;
                    if let Progress::Ready { record, .. } = step.progress { drop(record); break; }
                    if interrupted && offset > bytes.len() - 1024 { break; }
                }
            });
            println!("maximum entity interrupted={interrupted}: requested heap/overlap={heap}, sampled native stack={stack}, thread stack request=131072");
            assert!(heap < 100000);
            assert!(stack < 128 * 1024);
        }
    }).unwrap().join().unwrap();
}
