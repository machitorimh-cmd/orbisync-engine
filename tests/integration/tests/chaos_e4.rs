//! E-4 chaos coverage for the P2-04 database-stop scenario.
//!
//! The test deliberately controls only a caller-supplied PostgreSQL container;
//! it does not use a chaos-specific dependency or expose database credentials
//! in process arguments.  Set `CHAOS_POSTGRES_CONTAINER` to the container name
//! when running this test against a disposable PostgreSQL instance.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use orbisync_application::HealthProbe;
use orbisync_storage_postgres::PgHealthProbe;
use std::process::Command;
use std::time::Duration;

mod common;

const CONTAINER_ENV: &str = "CHAOS_POSTGRES_CONTAINER";

fn docker_container_action(action: &str, container: &str) -> bool {
    Command::new("docker")
        .args([action, container])
        .status()
        .is_ok_and(|status| status.success())
}

async fn wait_for_readiness(probe: &PgHealthProbe, expected: bool) -> bool {
    for _ in 0..30 {
        if probe.check().await.is_ok() == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn chaos_db_stop_and_restart_restores_readiness() {
    let Some(container) = std::env::var_os(CONTAINER_ENV) else {
        eprintln!(
            "SKIPPED (E-4): {CONTAINER_ENV} is not set; provide a disposable PostgreSQL container"
        );
        return;
    };
    let container = container.to_string_lossy();
    let Some(pool) = common::pool_or_skip().await else {
        return;
    };
    let probe = PgHealthProbe::new(pool);
    assert!(
        wait_for_readiness(&probe, true).await,
        "database must be ready before the chaos injection"
    );

    let stopped = docker_container_action("stop", &container);
    let not_ready = wait_for_readiness(&probe, false).await;
    let restarted = docker_container_action("start", &container);
    let ready_again = wait_for_readiness(&probe, true).await;

    assert!(
        stopped,
        "database container must stop for the chaos injection"
    );
    assert!(
        not_ready,
        "readiness must become false while PostgreSQL is stopped"
    );
    assert!(
        restarted,
        "database container must restart after the injection"
    );
    assert!(
        ready_again,
        "readiness must recover after PostgreSQL is restarted"
    );
}
