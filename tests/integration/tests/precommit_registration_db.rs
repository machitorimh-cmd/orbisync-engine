//! Real PostgreSQL verification of the pre-commit hook registration path
//! (ADR-025 §2.7): the `capabilities @> jsonb_build_array($1::text)` lookup
//! query and the write-time/read-time duplicate-active-capability rejection
//! added to `PgExtensionRegistrationStore` in
//! `crates/orbisync-storage-postgres/src/extension.rs`.
//!
//! `LIMIT 1` on its own is not a uniqueness guarantee — this file exists
//! because the orchestrator's improvement-02 brief §7 point 3/4 explicitly
//! requires that claim to be checked against a real PostgreSQL instance,
//! not asserted from reading the SQL. Skipped (not silently passed) when
//! `DATABASE_URL` is unset — see `common::pool_or_skip` (V-07). A skip here
//! must not be reported as a pass; `cargo test -- --nocapture` prints the
//! distinctive `SKIPPED (V-07)` marker when that happens.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;

use orbisync_application::{
    ApplicationErrorKind, ExtensionRegistration, ExtensionRegistrationStore, ExtensionStatus,
};
use orbisync_storage_postgres::PgExtensionRegistrationStore;

fn registration(
    extension_id: uuid::Uuid,
    capability: &str,
    status: ExtensionStatus,
) -> ExtensionRegistration {
    ExtensionRegistration {
        extension_id,
        name: format!("db-test-{extension_id}"),
        description: None,
        endpoint: String::from("https://precommit-db-test.example/hook"),
        subscribed_events: BTreeSet::new(),
        capabilities: BTreeSet::from([capability.to_owned()]),
        token_scopes: BTreeSet::new(),
        status,
        signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_DB_TEST"),
    }
}

/// No registration exists for the capability: `find_active_registration_by_capability`
/// must return `Ok(None)`, not an error — this is the default (opt-out) case
/// every other pre-commit hook test relies on.
#[tokio::test]
async fn no_registration_for_the_capability_returns_none() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());

    let found = store
        .find_active_registration_by_capability(&capability)
        .await
        .expect("lookup succeeds");
    assert!(found.is_none());
}

/// Exactly one Active registration for the capability: it round-trips
/// through `save_registration` and back out of
/// `find_active_registration_by_capability` unchanged.
#[tokio::test]
async fn a_single_active_registration_is_found_by_capability() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let reg = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);

    store
        .save_registration(reg.clone())
        .await
        .expect("save succeeds");

    let found = store
        .find_active_registration_by_capability(&capability)
        .await
        .expect("lookup succeeds")
        .expect("registration is found");
    assert_eq!(found.extension_id, reg.extension_id);
    assert_eq!(found.capabilities, reg.capabilities);
}

/// A `Suspended` registration is invisible to the lookup: opting a
/// registration out must not require deleting it.
#[tokio::test]
async fn a_suspended_registration_is_not_returned_as_active() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let reg = registration(
        uuid::Uuid::now_v7(),
        &capability,
        ExtensionStatus::Suspended,
    );

    store
        .save_registration(reg)
        .await
        .expect("save succeeds even for a suspended registration");

    let found = store
        .find_active_registration_by_capability(&capability)
        .await
        .expect("lookup succeeds");
    assert!(
        found.is_none(),
        "a suspended registration must not be treated as active"
    );
}

/// Saving a second `Active` registration for a capability another `Active`
/// registration already holds must be rejected outright — not silently
/// accepted and left for the read path to pick an arbitrary winner
/// (ADR-025 §2.7, improvement-02 brief §7 point 3). This exercises the real
/// write path (`save_registration` -> `PgExtensionRegistrationStore::save`),
/// not a unit-level assertion about the SQL text.
#[tokio::test]
async fn saving_a_second_active_registration_for_the_same_capability_is_rejected() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let first = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);
    store
        .save_registration(first)
        .await
        .expect("first save succeeds");

    let second = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);
    let error = store
        .save_registration(second.clone())
        .await
        .expect_err("a second active holder of the same capability must be rejected");
    assert_eq!(error.kind(), ApplicationErrorKind::Conflict);

    // The rejected row must not have been written at all (fail closed, not
    // a partial write): looking it up directly by its own id finds nothing.
    let leaked = store
        .find_registration(second.extension_id)
        .await
        .expect("lookup by id succeeds");
    assert!(
        leaked.is_none(),
        "a rejected duplicate registration must not be persisted"
    );
}

/// A registration may safely re-save itself (the same `extension_id`)
/// while `Active` for a capability it already holds — this is the ordinary
/// update path (e.g. changing `endpoint`), not a conflict with itself.
#[tokio::test]
async fn re_saving_the_same_extension_id_for_its_own_capability_is_not_a_conflict() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let extension_id = uuid::Uuid::now_v7();
    let mut reg = registration(extension_id, &capability, ExtensionStatus::Active);
    store
        .save_registration(reg.clone())
        .await
        .expect("first save succeeds");

    reg.endpoint = String::from("https://precommit-db-test.example/hook-v2");
    store
        .save_registration(reg.clone())
        .await
        .expect("re-saving the same extension_id must not conflict with itself");

    let found = store
        .find_registration(extension_id)
        .await
        .expect("lookup succeeds")
        .expect("registration exists");
    assert_eq!(found.endpoint, reg.endpoint);
}

/// A capability without the `hooks:` prefix is not subject to the
/// at-most-one-active-holder rule: two different Active registrations may
/// both declare an ordinary (non pre-commit-hook) capability, matching
/// existing Extension registration behavior before ADR-025.
#[tokio::test]
async fn non_precommit_capabilities_may_still_be_shared_across_registrations() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let store = PgExtensionRegistrationStore::new(pool);
    let capability = format!("commands:audit:{}", uuid::Uuid::now_v7());
    let first = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);
    let second = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);

    store
        .save_registration(first)
        .await
        .expect("first save succeeds");
    store
        .save_registration(second)
        .await
        .expect("a second holder of a non-hooks capability must not be rejected");
}

/// A pre-existing duplicate created before the write-time check existed
/// (simulated here by inserting both rows directly with SQL, bypassing
/// `save_registration`) must not be resolved by `find_active_registration_by_capability`
/// picking one arbitrarily — it fails closed with `Conflict` instead
/// (improvement-02 brief §7 point 3: "複数登録が既にある場合も任意の1件を
/// 選んで承認せず、制御されたエラーで拒否する").
#[tokio::test]
async fn a_pre_existing_duplicate_created_outside_save_registration_is_rejected_on_read() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let capability_json = serde_json::json!([capability]);
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO extension_registrations \
             (extension_id, name, description, endpoint, subscribed_events, capabilities, token_scopes, status, signing_secret_ref) \
             VALUES ($1, $2, NULL, $3, '[]'::jsonb, $4, '[]'::jsonb, 'active', $5)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(format!("raw-sql-{}", uuid::Uuid::now_v7()))
        .bind("https://precommit-db-test.example/raw-sql")
        .bind(&capability_json)
        .bind("ORBI_EXTENSION_SECRET_DB_TEST")
        .execute(&pool)
        .await
        .expect("direct SQL insert succeeds (bypassing save_registration on purpose)");
    }

    let store = PgExtensionRegistrationStore::new(pool);
    let error = store
        .find_active_registration_by_capability(&capability)
        .await
        .expect_err("a pre-existing duplicate must be rejected, not resolved to one row");
    assert_eq!(error.kind(), ApplicationErrorKind::Conflict);
}

/// Concurrent attempts to register the same capability must not both
/// succeed: the `pg_advisory_xact_lock` in `PgExtensionRegistrationStore::save`
/// serializes them, so exactly one save must fail with `Conflict` even when
/// both transactions start before either commits.
#[tokio::test]
async fn concurrent_saves_for_the_same_capability_do_not_both_succeed() {
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let capability = format!("hooks:entity:{}", uuid::Uuid::now_v7());
    let store_a = PgExtensionRegistrationStore::new(pool.clone());
    let store_b = PgExtensionRegistrationStore::new(pool);
    let a = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);
    let b = registration(uuid::Uuid::now_v7(), &capability, ExtensionStatus::Active);

    let (result_a, result_b) =
        tokio::join!(store_a.save_registration(a), store_b.save_registration(b));
    let successes = usize::from(result_a.is_ok()) + usize::from(result_b.is_ok());
    assert_eq!(
        successes, 1,
        "exactly one concurrent save for the same capability must succeed, got a={result_a:?} b={result_b:?}"
    );
}
