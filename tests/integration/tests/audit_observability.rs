//! Regression test for C4: audit writes must not be silently discarded.
//! Verifies that when `record_world_audit` or `PasswordChangeRejected` audit
//! fails, the failure is observable via `tracing::warn!` (A-3 fix).
//! If the `tracing::warn!` is removed, these tests will fail (red), proving
//! the observability is not lost.

#![allow(missing_docs)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orbisync_application::{ApplicationErrorKind, CreateWorldCommand, RequestId};
use orbisync_domain::{Timestamp, UserId, transform::Transform};
use orbisync_testkit::{DenyAllAuthorizer, FakeWorldDirectoryStore};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// World audit observability
// ---------------------------------------------------------------------------

#[test]
fn world_audit_failure_is_observed_via_tracing() {
    let store = FakeWorldDirectoryStore::new();
    store.fail_next_audit_with(orbisync_application::ApplicationError::new(
        ApplicationErrorKind::PortFailure,
        "injected audit failure",
    ));
    let uc = orbisync_application::WorldDirectoryUseCase::new(store, DenyAllAuthorizer);

    let cmd = CreateWorldCommand {
        actor_id: UserId::generate(),
        name: "test-world".to_owned(),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 10,
        now: Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("req"),
    };

    let logs: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let logs_clone = logs.clone();
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || {
            struct Writer(Arc<Mutex<Vec<u8>>>);
            impl std::io::Write for Writer {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            Box::new(Writer(logs_clone.clone())) as Box<dyn std::io::Write>
        })
        .finish();

    let err =
        tracing::dispatcher::with_default(&tracing::dispatcher::Dispatch::new(subscriber), || {
            pollster::block_on(uc.create_world(cmd)).expect_err("must be NotAuthorized")
        });

    assert_eq!(err.kind(), ApplicationErrorKind::NotAuthorized);
    let output = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8");
    assert!(
        output.contains("failed to persist world audit")
            || output.contains("failed to append world audit"),
        "audit failure must be observable via tracing::warn, got: {output}"
    );
}

#[test]
fn world_audit_failure_does_not_mask_authorization_error() {
    let store = FakeWorldDirectoryStore::new();
    store.fail_next_audit_with(orbisync_application::ApplicationError::new(
        ApplicationErrorKind::PortFailure,
        "injected",
    ));
    let uc = orbisync_application::WorldDirectoryUseCase::new(store, DenyAllAuthorizer);
    let cmd = CreateWorldCommand {
        actor_id: UserId::generate(),
        name: "w".to_owned(),
        description: None,
        default_spawn: Transform::identity(),
        capacity: 5,
        now: Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
        request_id: RequestId::new(format!("req_{}", UserId::generate())).expect("req"),
    };
    let err = pollster::block_on(uc.create_world(cmd)).expect_err("must be NotAuthorized");
    assert_eq!(err.kind(), ApplicationErrorKind::NotAuthorized);
}

// ---------------------------------------------------------------------------
// Identity audit observability (PasswordChangeRejected) – source check
// ---------------------------------------------------------------------------

#[test]
fn password_change_rejected_audit_is_logged_in_source() {
    // V-13: the compile-time manifest-dir macro bakes the path into the binary, so a
    // cached artifact looks for another (possibly deleted) checkout. Read it at runtime.
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap().parent().unwrap();
    let admin_rs = std::fs::read_to_string(root.join("crates/orbisync-identity/src/admin.rs"))
        .expect("read admin.rs");
    assert!(
        admin_rs.contains("failed to persist password change rejected audit"),
        "admin.rs must contain tracing::warn for PasswordChangeRejected audit failure"
    );
    assert!(
        admin_rs.contains("tracing::warn!"),
        "admin.rs must use tracing::warn! for audit failures"
    );
    assert!(
        !admin_rs.contains("the storage layer logs persistence failures"),
        "false claim about storage logging must be removed"
    );
}

#[test]
fn world_audit_is_logged_in_application_and_storage() {
    // V-13: the compile-time manifest-dir macro bakes the path into the binary, so a
    // cached artifact looks for another (possibly deleted) checkout. Read it at runtime.
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap().parent().unwrap();
    let world_rs = std::fs::read_to_string(root.join("crates/orbisync-application/src/world.rs"))
        .expect("read world.rs");
    assert!(
        world_rs.contains("failed to persist world audit"),
        "world.rs must contain tracing::warn for world audit failure"
    );
    let storage_world =
        std::fs::read_to_string(root.join("crates/orbisync-storage-postgres/src/world.rs"))
            .expect("read storage world.rs");
    assert!(
        storage_world.contains("tracing::warn!"),
        "storage world.rs must contain tracing::warn!"
    );
    assert!(
        storage_world.contains("failed to append world audit")
            || storage_world.contains("failed to begin transaction for world audit"),
        "storage world.rs must log on audit failure"
    );
    let storage_identity =
        std::fs::read_to_string(root.join("crates/orbisync-storage-postgres/src/identity.rs"))
            .expect("read storage identity.rs");
    assert!(
        storage_identity.contains("tracing::warn!"),
        "storage identity.rs must contain tracing::warn!"
    );
    assert!(
        storage_identity.contains("failed to append audit event"),
        "storage identity.rs must log on audit failure"
    );
}

/// Checks that every `let_underscore_must_use` allow carries a `Reason:` comment and
/// that no allow repeats a reason we already proved false.
///
/// This does NOT verify that the reasons are true - truthfulness cannot be checked
/// mechanically. The name deliberately claims only what is actually verified.
#[test]
fn allow_reasons_are_present_and_not_known_false() {
    // V-13: the compile-time manifest-dir macro bakes the path into the binary, so a
    // cached artifact looks for another (possibly deleted) checkout. Read it at runtime.
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap().parent().unwrap();
    let files = [
        "crates/orbisync-application/src/world.rs",
        "crates/orbisync-identity/src/admin.rs",
        "crates/orbisync-identity/src/service.rs",
        "crates/orbisync-server/src/delivery.rs",
        "crates/orbisync-server/src/realtime_ws.rs",
    ];
    let mut total_allows = 0;
    let mut with_reason = 0;
    for rel in files {
        let content = std::fs::read_to_string(root.join(rel)).expect("read file");
        let lines: Vec<&str> = content.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.contains("let_underscore_must_use") {
                total_allows += 1;
                let prev = lines[i.saturating_sub(1)..i].join("\n");
                let prev2 = if i >= 2 { lines[i - 2] } else { "" };
                if prev.contains("Reason:") || prev2.contains("Reason:") {
                    with_reason += 1;
                }
                assert!(
                    !prev.contains("the storage layer logs persistence failures")
                        && !prev2.contains("the storage layer logs persistence failures"),
                    "false claim still present in {rel}:{}",
                    i + 1
                );
            }
        }
    }
    // 33 - 4 (false claims fixed to `if let Err`) - 1 (B-3 removed delivery.rs's
    // silent drop, whose reason contradicted the reliability contract) = 28.
    // world 0 + admin 0 + service 2 + delivery 0 + realtime_ws 26 = 28
    assert_eq!(
        total_allows, 28,
        "expected 28 allows in production, got {total_allows}"
    );
    assert_eq!(
        with_reason, 28,
        "all 28 allows must have Reason: comment, got {with_reason}"
    );
}
