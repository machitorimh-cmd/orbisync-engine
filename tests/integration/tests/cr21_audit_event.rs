//! CR-21: AuditQueryPort::event must be required, not default Ok(None),
//! and FakeIdentityStore must implement it from the same source as search.

#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs)]

use std::sync::Arc;

use orbisync_application::{
    AuditFilter, AuditQueryPort, IdentityAdministrationStore, PageRequest, RequestId,
};
use orbisync_domain::{LoginId, Timestamp};
use orbisync_identity::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService, PasswordPolicy,
    PasswordService,
};
use orbisync_testkit::{FakeIdentityStore, FixedClock};
use uuid::Uuid;

#[tokio::test]
async fn cr21_fake_store_event_returns_created_audit() {
    let store = Arc::new(FakeIdentityStore::new());
    let clock = Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ));
    let passwords = PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pw");
    let dyn_store =
        DynIdentityAdministrationStore(store.clone() as Arc<dyn IdentityAdministrationStore>);
    let dyn_clock = DynClock(clock.clone() as Arc<dyn orbisync_domain::Clock>);
    let admin =
        IdentityAdministrationService::new(Arc::new(dyn_store), Arc::new(dyn_clock), passwords);

    // Create a user via admin service – this will generate an audit event inside FakeIdentityStore
    let (_user, _) = admin
        .bootstrap_administrator(
            LoginId::new("admin").expect("login"),
            "Admin".to_owned(),
            RequestId::new(format!("req_{}", Uuid::now_v7())).expect("req"),
        )
        .await
        .expect("bootstrap");

    // Search to find the audit event for this bootstrap
    // The FakeIdentityStore stores AuditView with id = now_v7 string, we need to find it
    let page = store
        .search(
            AuditFilter {
                action: Some("administrator.bootstrapped".to_owned()),
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                after: None,
            },
        )
        .await
        .expect("search");
    assert!(
        !page.items.is_empty(),
        "should have at least one administrator.bootstrapped audit"
    );
    let first = &page.items[0];
    let event_id = Uuid::parse_str(&first.id).expect("audit id is uuid");
    // Now fetch via event() – this is the required method after CR-21
    let fetched = store
        .event(event_id)
        .await
        .expect("event should succeed")
        .expect("event should be Some");
    assert_eq!(
        fetched.id, first.id,
        "event() should return same id as search"
    );
    assert_eq!(fetched.action, "administrator.bootstrapped");
    // Verify that event uses same source as search: if we had only default Ok(None), this would be None
    // For bootstrap, actor_id is None (system action), not user.id()
    assert_eq!(fetched.action, "administrator.bootstrapped");
}

/// This test documents that removing `event` from `FakeIdentityStore` is a
/// compile-time error, not a silent 404. To verify, comment out the `event`
/// impl in `crates/orbisync-testkit/src/identity.rs` and run `cargo build`:
/// it will fail with `error[E0046]: not all trait items implemented, missing: event`.
/// After restoring, `cargo build` succeeds, proving the trait now requires the method.

#[tokio::test]
async fn cr21_event_not_found_returns_none() {
    let store = FakeIdentityStore::new();
    let missing = Uuid::now_v7();
    let res = store
        .event(missing)
        .await
        .expect("event query should not error");
    assert!(res.is_none(), "missing audit should be None, not error");
}
