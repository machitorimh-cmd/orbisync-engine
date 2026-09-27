#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use orbisync_domain::{Entity, EntityId, EntityKind, InstanceId, Timestamp, VisibilityPolicy};

#[derive(Arbitrary, Debug)]
struct ComponentUpdate {
    key: String,
    payload: Vec<u8>,
}

fuzz_target!(|input: ComponentUpdate| {
    let Ok(now) = Timestamp::from_unix_millis(1_000) else {
        return;
    };
    let mut entity = Entity::new(
        EntityId::generate(),
        InstanceId::generate(),
        EntityKind::Object,
        None,
        None,
        VisibilityPolicy::Global,
        now,
    );
    let _ = entity.update_component(entity.revision(), input.key, input.payload, now);
});
