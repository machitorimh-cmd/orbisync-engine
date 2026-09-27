//! PostgreSQL persistence for extension registrations.

use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use orbisync_application::{
    ApplicationError, ApplicationErrorKind, ExtensionDeliveryStore, ExtensionEvent,
    ExtensionOutboxStore, ExtensionRegistration, ExtensionRegistrationStore, ExtensionStatus,
    PendingExtensionDelivery,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::StorageError;

/// PostgreSQL-backed extension registration store.
#[derive(Debug, Clone)]
pub struct PgExtensionRegistrationStore {
    pool: PgPool,
}

/// PostgreSQL-backed durable extension outbox writer.
#[derive(Debug, Clone)]
pub struct PgExtensionOutboxStore {
    pool: PgPool,
    active_registration_count: Arc<AtomicU64>,
    active_registration_cache_known: Arc<AtomicBool>,
}

type ExtensionRegistrationRow = (
    Uuid,
    String,
    Option<String>,
    String,
    Value,
    Value,
    Value,
    String,
    String,
);

/// Counts inconsistent extension terminal state for reconciliation/alerting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionTerminalStateCounts {
    /// Dead-letter rows whose delivery row is still pending.
    pub stranded_dead_letters: u64,
    /// Events with no pending delivery and no eligible registration omitted.
    pub events_ready_to_finalize: u64,
    /// Terminal delivery rows whose parent event is still pending.
    pub terminal_deliveries_with_pending_event: u64,
}

/// Result of repairing stranded extension terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionTerminalStateReconciliation {
    /// Number of delivery rows repaired from an existing DLQ row.
    pub dead_letters_repaired: u64,
    /// Number of parent outbox rows marked delivered.
    pub events_repaired: u64,
}

impl PgExtensionOutboxStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            active_registration_count: Arc::new(AtomicU64::new(0)),
            active_registration_cache_known: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Refreshes the cached number of active extension registrations.
    ///
    /// An unavailable count deliberately leaves the cache unknown. The
    /// append path treats an unknown cache as permission to persist, so a
    /// transient database failure cannot lose an extension event.
    pub async fn refresh_active_registration_cache(&self) -> Result<(), ApplicationError> {
        let result: Result<i64, sqlx::Error> = sqlx::query_scalar(
            "SELECT count(*) FROM extension_registrations WHERE status = 'active'",
        )
        .fetch_one(&self.pool)
        .await;
        match result {
            Ok(count) => {
                self.active_registration_count
                    .store(u64::try_from(count).unwrap_or(u64::MAX), Ordering::Release);
                self.active_registration_cache_known
                    .store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.active_registration_cache_known
                    .store(false, Ordering::Release);
                Err(ApplicationError::new(
                    ApplicationErrorKind::PortFailure,
                    format!("active extension registration count failed: {error}"),
                ))
            }
        }
    }

    fn should_persist(&self) -> bool {
        !self.active_registration_cache_known.load(Ordering::Acquire)
            || self.active_registration_count.load(Ordering::Acquire) > 0
    }

    /// Deletes a bounded batch of delivered extension outbox events older
    /// than `before`, including their delivery rows.
    ///
    /// Pending events are intentionally excluded. Delivery rows are removed
    /// first because the schema's foreign key does not cascade from
    /// `outbox_events`.
    pub async fn delete_expired_delivered(
        &self,
        before: time::OffsetDateTime,
        limit: i64,
    ) -> Result<u64, ApplicationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        let event_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT o.event_id
             FROM outbox_events o
             WHERE o.owner_module = 'extensions'
               AND o.delivered_at IS NOT NULL
               AND o.delivered_at <= $1
               AND NOT EXISTS (
                   SELECT 1 FROM extension_deliveries d
                   WHERE d.event_id = o.event_id
                     AND d.delivered_at IS NULL
                     AND d.dead_lettered_at IS NULL
               )
             ORDER BY o.delivered_at, o.event_id
             LIMIT $2
             FOR UPDATE SKIP LOCKED",
        )
        .bind(before)
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        if event_ids.is_empty() {
            transaction
                .commit()
                .await
                .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
            return Ok(0);
        }

        sqlx::query(
            "DELETE FROM extension_dead_letters
              WHERE delivery_id IN (
                  SELECT delivery_id FROM extension_deliveries WHERE event_id = ANY($1)
              )",
        )
        .bind(&event_ids)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        sqlx::query("DELETE FROM extension_deliveries WHERE event_id = ANY($1)")
            .bind(&event_ids)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        let deleted = sqlx::query(
            "DELETE FROM outbox_events
             WHERE owner_module = 'extensions' AND event_id = ANY($1)",
        )
        .bind(&event_ids)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?
        .rows_affected();
        transaction
            .commit()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        Ok(deleted)
    }

    /// Counts pending extension outbox events for the retention gauge.
    pub async fn count_pending_events(&self) -> Result<u64, ApplicationError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM outbox_events
             WHERE owner_module = 'extensions' AND delivered_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        Ok(u64::try_from(count).unwrap_or(u64::MAX))
    }

    /// Counts terminal-state inconsistencies without changing any rows.
    pub async fn count_terminal_state_inconsistencies(
        &self,
    ) -> Result<ExtensionTerminalStateCounts, ApplicationError> {
        let (
            stranded_dead_letters,
            events_ready_to_finalize,
            terminal_deliveries_with_pending_event,
        ): (i64, i64, i64) = sqlx::query_as(
            "SELECT
                 (SELECT count(*)
                    FROM extension_dead_letters dl
                    LEFT JOIN extension_deliveries d ON d.delivery_id = dl.delivery_id
                   WHERE d.delivery_id IS NULL
                      OR (d.delivered_at IS NULL AND d.dead_lettered_at IS NULL)),
                 (SELECT count(*)
                    FROM outbox_events o
                   WHERE o.owner_module = 'extensions'
                     AND o.delivered_at IS NULL
                     AND NOT EXISTS (
                         SELECT 1 FROM extension_deliveries d
                          WHERE d.event_id = o.event_id
                            AND d.delivered_at IS NULL
                            AND d.dead_lettered_at IS NULL
                     )
                     AND NOT EXISTS (
                         SELECT 1 FROM extension_registrations r
                          WHERE r.status = 'active'
                            AND r.created_at <= o.created_at
                            AND r.subscribed_events ? o.event_kind
                            AND NOT EXISTS (
                                SELECT 1 FROM extension_deliveries d
                                 WHERE d.event_id = o.event_id
                                   AND d.extension_id = r.extension_id
                            )
                     )),
                 (SELECT count(*)
                    FROM extension_deliveries d
                    JOIN outbox_events o ON o.event_id = d.event_id
                   WHERE o.owner_module = 'extensions'
                     AND o.delivered_at IS NULL
                     AND (d.delivered_at IS NOT NULL OR d.dead_lettered_at IS NOT NULL))",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        Ok(ExtensionTerminalStateCounts {
            stranded_dead_letters: u64::try_from(stranded_dead_letters).unwrap_or(u64::MAX),
            events_ready_to_finalize: u64::try_from(events_ready_to_finalize).unwrap_or(u64::MAX),
            terminal_deliveries_with_pending_event: u64::try_from(
                terminal_deliveries_with_pending_event,
            )
            .unwrap_or(u64::MAX),
        })
    }

    /// Repairs at most `limit` parent events per invocation.
    ///
    /// Parent rows are locked first and all child updates follow that lock
    /// order. The limit makes reconciliation safe to run in a bounded
    /// operator loop rather than turning a backlog into one unbounded update.
    pub async fn reconcile_terminal_states(
        &self,
        limit: u32,
    ) -> Result<ExtensionTerminalStateReconciliation, ApplicationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        let parent_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT o.event_id
               FROM outbox_events o
              WHERE o.owner_module = 'extensions'
                AND o.delivered_at IS NULL
              ORDER BY o.created_at, o.event_id
              LIMIT $1
              FOR UPDATE SKIP LOCKED",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;

        let mut dead_letters_repaired = 0u64;
        let mut events_repaired = 0u64;
        for event_id in parent_ids {
            let repaired = sqlx::query(
                "UPDATE extension_deliveries d
                    SET dead_lettered_at = dl.dead_lettered_at,
                        attempt_count = dl.attempt_count,
                        last_error_code = dl.error_code,
                        lease_owner = NULL,
                        lease_token = NULL,
                        lease_expires_at = NULL
                   FROM extension_dead_letters dl
                  WHERE dl.delivery_id = d.delivery_id
                    AND d.event_id = $1
                    AND d.delivered_at IS NULL
                    AND d.dead_lettered_at IS NULL",
            )
            .bind(event_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?
            .rows_affected();
            dead_letters_repaired = dead_letters_repaired.saturating_add(repaired);

            let can_finalize: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS (
                     SELECT 1 FROM extension_deliveries d
                      WHERE d.event_id = $1
                        AND d.delivered_at IS NULL
                        AND d.dead_lettered_at IS NULL
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM extension_registrations r
                      JOIN outbox_events o ON o.event_id = $1
                     WHERE r.status = 'active'
                       AND r.created_at <= o.created_at
                       AND r.subscribed_events ? o.event_kind
                       AND NOT EXISTS (
                           SELECT 1 FROM extension_deliveries d
                            WHERE d.event_id = $1
                              AND d.extension_id = r.extension_id
                       )
                 )",
            )
            .bind(event_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
            if can_finalize {
                let updated = sqlx::query(
                    "UPDATE outbox_events
                        SET delivered_at = CURRENT_TIMESTAMP
                      WHERE event_id = $1
                        AND owner_module = 'extensions'
                        AND delivered_at IS NULL",
                )
                .bind(event_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
                events_repaired = events_repaired.saturating_add(updated.rows_affected());
            }
        }

        transaction
            .commit()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        Ok(ExtensionTerminalStateReconciliation {
            dead_letters_repaired,
            events_repaired,
        })
    }

    async fn append(&self, event: ExtensionEvent) -> Result<Uuid, StorageError> {
        let event_id = Uuid::now_v7();
        if !self.should_persist() {
            return Ok(event_id);
        }
        let created_at = time::OffsetDateTime::now_utc();
        sqlx::query(
            "INSERT INTO outbox_events (id, event_id, owner_module, event_type, event_kind, payload, created_at, available_at) VALUES ($1, $1, 'extensions', $2, $2, $3, $4, $4)",
        )
        .bind(event_id)
        .bind(event.kind())
        .bind(event.payload())
        .bind(created_at)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        Ok(event_id)
    }
}

#[async_trait::async_trait]
impl ExtensionOutboxStore for PgExtensionOutboxStore {
    async fn append_event(&self, event: ExtensionEvent) -> Result<Uuid, ApplicationError> {
        self.append(event).await.map_err(|error| {
            ApplicationError::new(ApplicationErrorKind::PortFailure, error.to_string())
        })
    }
}

#[async_trait::async_trait]
impl ExtensionDeliveryStore for PgExtensionOutboxStore {
    async fn claim_due(
        &self,
        now: time::OffsetDateTime,
        lease_owner: Uuid,
        lease_expires_at: time::OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<PendingExtensionDelivery>, ApplicationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        // Lock parents before materializing/claiming children. Every terminal
        // transition uses the same parent -> delivery order, preventing a
        // claim/finalize deadlock while keeping this work bounded by `limit`.
        let parent_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT o.event_id
               FROM outbox_events o
              WHERE o.owner_module = 'extensions'
                AND o.delivered_at IS NULL
                AND (
                    (o.available_at <= $1 AND EXISTS (
                        SELECT 1 FROM extension_deliveries d
                         WHERE d.event_id = o.event_id
                           AND d.available_at <= $1
                           AND d.delivered_at IS NULL
                           AND d.dead_lettered_at IS NULL
                           AND (d.lease_expires_at IS NULL OR d.lease_expires_at <= $1)
                    ))
                    OR
                    (o.available_at <= $1 AND EXISTS (
                        SELECT 1 FROM extension_registrations r
                         WHERE r.status = 'active'
                           AND r.created_at <= o.created_at
                           AND r.subscribed_events ? o.event_kind
                           AND NOT EXISTS (
                               SELECT 1 FROM extension_deliveries d
                                WHERE d.event_id = o.event_id
                                  AND d.extension_id = r.extension_id
                           )
                    ))
                )
              ORDER BY o.available_at, o.event_id
              LIMIT $2
              FOR UPDATE SKIP LOCKED",
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        for &event_id in &parent_ids {
            // The payload comes only from outbox_events; signing secrets are
            // never copied into delivery tables.
            sqlx::query(
                "INSERT INTO extension_deliveries
                    (delivery_id, event_id, extension_id, available_at)
                 SELECT gen_random_uuid(), o.event_id, r.extension_id, o.available_at
                   FROM outbox_events o
                   JOIN extension_registrations r
                     ON r.status = 'active'
                    AND r.subscribed_events ? o.event_kind
                    AND o.created_at >= r.created_at
                  WHERE o.event_id = $1
                 ON CONFLICT (event_id, extension_id) DO NOTHING",
            )
            .bind(event_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        }

        let rows: Vec<ExtensionDeliveryRow> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT d.delivery_id
                   FROM extension_deliveries d
                   JOIN outbox_events o ON o.event_id = d.event_id
                  WHERE d.available_at <= $1
                    AND o.owner_module = 'extensions'
                    AND o.delivered_at IS NULL
                    AND d.event_id = ANY($5::uuid[])
                    AND d.delivered_at IS NULL
                    AND d.dead_lettered_at IS NULL
                    AND (d.lease_expires_at IS NULL OR d.lease_expires_at <= $1)
                  ORDER BY d.available_at, d.delivery_id
                  FOR UPDATE OF d SKIP LOCKED
                   LIMIT $4
             ), claimed AS (
                 UPDATE extension_deliveries d
                    SET lease_owner = $2, lease_token = gen_random_uuid(), lease_expires_at = $3
                   FROM candidates c
                  WHERE d.delivery_id = c.delivery_id
                 RETURNING d.delivery_id, d.lease_owner, d.lease_token, d.lease_expires_at
             )
             SELECT d.delivery_id, o.event_id, o.event_kind, o.payload,
                    r.extension_id, r.name, r.description, r.endpoint,
                    r.subscribed_events, r.capabilities, r.token_scopes,
                    r.status, r.signing_secret_ref, d.attempt_count,
                    d.available_at, c.lease_owner, c.lease_token, c.lease_expires_at
               FROM claimed c
               JOIN extension_deliveries d ON d.delivery_id = c.delivery_id
               JOIN outbox_events o ON o.event_id = d.event_id
               JOIN extension_registrations r ON r.extension_id = d.extension_id",
        )
        .bind(now)
        .bind(lease_owner)
        .bind(lease_expires_at)
        .bind(i64::from(limit))
        .bind(&parent_ids)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        transaction
            .commit()
            .await
            .map_err(|error| ApplicationError::port_failure(error.to_string()))?;
        rows.into_iter()
            .map(try_into_pending)
            .collect::<Result<Vec<_>, _>>()
    }

    async fn mark_delivered(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: time::OffsetDateTime,
    ) -> Result<(), ApplicationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| ApplicationError::port_failure("extension delivery update failed"))?;
        let event_id: Option<Uuid> =
            sqlx::query_scalar("SELECT event_id FROM extension_deliveries WHERE delivery_id = $1")
                .bind(delivery_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| ApplicationError::port_failure("extension delivery update failed"))?;
        let Some(event_id) = event_id else {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotFound,
                "extension delivery does not exist",
            ));
        };
        lock_extension_parent(&mut transaction, event_id).await?;
        let result = sqlx::query(
            "UPDATE extension_deliveries SET delivered_at = CURRENT_TIMESTAMP,
                    last_error_code = NULL, lease_owner = NULL,
                    lease_token = NULL, lease_expires_at = NULL
              WHERE delivery_id = $1 AND lease_owner = $2 AND lease_token = $3
                AND lease_expires_at = $4
                AND lease_expires_at > CURRENT_TIMESTAMP
                AND delivered_at IS NULL AND dead_lettered_at IS NULL",
        )
        .bind(delivery_id)
        .bind(lease_owner)
        .bind(lease_token)
        .bind(lease_expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApplicationError::port_failure("extension delivery update failed"))?;
        if result.rows_affected() != 1 {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "extension delivery lease is stale",
            ));
        }
        finalize_parent_event(&mut transaction, event_id).await?;
        transaction
            .commit()
            .await
            .map_err(|_| ApplicationError::port_failure("extension delivery update failed"))?;
        Ok(())
    }

    async fn reschedule(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: time::OffsetDateTime,
        attempt_count: u32,
        available_at: time::OffsetDateTime,
        error_code: &str,
    ) -> Result<(), ApplicationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| ApplicationError::port_failure("extension retry update failed"))?;
        let event_id: Option<Uuid> =
            sqlx::query_scalar("SELECT event_id FROM extension_deliveries WHERE delivery_id = $1")
                .bind(delivery_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| ApplicationError::port_failure("extension retry update failed"))?;
        let Some(event_id) = event_id else {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotFound,
                "extension delivery does not exist",
            ));
        };
        lock_extension_parent(&mut transaction, event_id).await?;
        let result = sqlx::query(
            "UPDATE extension_deliveries SET attempt_count = $2, available_at = $3,
                    last_error_code = $4, lease_owner = NULL,
                    lease_token = NULL, lease_expires_at = NULL
              WHERE delivery_id = $1 AND lease_owner = $5 AND lease_token = $6
                AND lease_expires_at = $7
                AND lease_expires_at > CURRENT_TIMESTAMP
                AND delivered_at IS NULL AND dead_lettered_at IS NULL",
        )
        .bind(delivery_id)
        .bind(i32::try_from(attempt_count).unwrap_or(i32::MAX))
        .bind(available_at)
        .bind(error_code)
        .bind(lease_owner)
        .bind(lease_token)
        .bind(lease_expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApplicationError::port_failure("extension retry update failed"))?;
        if result.rows_affected() != 1 {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "extension delivery lease is stale",
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|_| ApplicationError::port_failure("extension retry update failed"))?;
        Ok(())
    }

    async fn move_to_dead_letter(
        &self,
        delivery_id: Uuid,
        lease_owner: Uuid,
        lease_token: Uuid,
        lease_expires_at: time::OffsetDateTime,
        attempt_count: u32,
        error_code: &str,
        retained_until: time::OffsetDateTime,
    ) -> Result<(), ApplicationError> {
        let mut transaction =
            self.pool.begin().await.map_err(|_| {
                ApplicationError::port_failure("extension dead-letter update failed")
            })?;
        let parent_event_id: Option<Uuid> =
            sqlx::query_scalar("SELECT event_id FROM extension_deliveries WHERE delivery_id = $1")
                .bind(delivery_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| {
                    ApplicationError::port_failure("extension dead-letter update failed")
                })?;
        let Some(parent_event_id) = parent_event_id else {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotFound,
                "extension delivery does not exist",
            ));
        };
        lock_extension_parent(&mut transaction, parent_event_id).await?;
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "UPDATE extension_deliveries SET dead_lettered_at = CURRENT_TIMESTAMP,
                    attempt_count = $2, last_error_code = $3,
                    lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL
              WHERE delivery_id = $1 AND lease_owner = $4 AND lease_token = $5
                AND lease_expires_at = $6
                AND lease_expires_at > CURRENT_TIMESTAMP
                AND delivered_at IS NULL AND dead_lettered_at IS NULL
             RETURNING event_id, extension_id",
        )
        .bind(delivery_id)
        .bind(i32::try_from(attempt_count).unwrap_or(i32::MAX))
        .bind(error_code)
        .bind(lease_owner)
        .bind(lease_token)
        .bind(lease_expires_at)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| ApplicationError::port_failure("extension dead-letter update failed"))?;
        let Some((event_id, extension_id)) = row else {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "extension delivery lease is stale",
            ));
        };
        let inserted = sqlx::query(
            "INSERT INTO extension_dead_letters
                (delivery_id, event_id, extension_id, event_kind, payload,
                 attempt_count, error_code, retained_until)
             SELECT $1, $2, $3, o.event_kind, o.payload, $4, $5, $6
               FROM outbox_events o WHERE o.event_id = $2",
        )
        .bind(delivery_id)
        .bind(event_id)
        .bind(extension_id)
        .bind(i32::try_from(attempt_count).unwrap_or(i32::MAX))
        .bind(error_code)
        .bind(retained_until)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApplicationError::port_failure("extension dead-letter insert failed"))?;
        if inserted.rows_affected() != 1 {
            return Err(ApplicationError::port_failure(
                "extension dead-letter event is missing",
            ));
        }
        finalize_parent_event(&mut transaction, event_id).await?;
        transaction
            .commit()
            .await
            .map_err(|_| ApplicationError::port_failure("extension dead-letter update failed"))?;
        Ok(())
    }

    async fn purge_dead_letters(
        &self,
        before: time::OffsetDateTime,
    ) -> Result<u64, ApplicationError> {
        let result = sqlx::query("DELETE FROM extension_dead_letters WHERE retained_until <= $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|_| ApplicationError::port_failure("extension dead-letter purge failed"))?;
        Ok(result.rows_affected())
    }
}

async fn lock_extension_parent(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
) -> Result<(), ApplicationError> {
    let parent: Option<(String,)> =
        sqlx::query_as("SELECT owner_module FROM outbox_events WHERE event_id = $1 FOR UPDATE")
            .bind(event_id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| ApplicationError::port_failure("extension outbox event lock failed"))?;
    match parent {
        None => Err(ApplicationError::new(
            ApplicationErrorKind::NotFound,
            "extension outbox event does not exist",
        )),
        Some((owner_module,)) if owner_module != "extensions" => Err(
            ApplicationError::port_failure("extension delivery references a non-extension event"),
        ),
        Some(_) => Ok(()),
    }
}

async fn finalize_parent_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
) -> Result<(), ApplicationError> {
    let updated = sqlx::query(
        "UPDATE outbox_events o
            SET delivered_at = CURRENT_TIMESTAMP
          WHERE o.event_id = $1
            AND o.owner_module = 'extensions'
            AND o.delivered_at IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM extension_deliveries d
                 WHERE d.event_id = o.event_id
                   AND d.delivered_at IS NULL
                   AND d.dead_lettered_at IS NULL
            )
            AND NOT EXISTS (
                SELECT 1 FROM extension_registrations r
                 WHERE r.status = 'active'
                   AND r.created_at <= o.created_at
                   AND r.subscribed_events ? o.event_kind
                   AND NOT EXISTS (
                       SELECT 1 FROM extension_deliveries d
                        WHERE d.event_id = o.event_id
                          AND d.extension_id = r.extension_id
                   )
            )",
    )
    .bind(event_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| ApplicationError::port_failure("extension outbox update failed"))?;
    if updated.rows_affected() > 1 {
        return Err(ApplicationError::port_failure(
            "extension outbox finalization affected multiple events",
        ));
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct ExtensionDeliveryRow {
    delivery_id: Uuid,
    event_id: Uuid,
    event_kind: String,
    payload: Value,
    extension_id: Uuid,
    name: String,
    description: Option<String>,
    endpoint: String,
    subscribed_events: Value,
    capabilities: Value,
    token_scopes: Value,
    status: String,
    signing_secret_ref: String,
    attempt_count: i32,
    available_at: time::OffsetDateTime,
    lease_owner: Uuid,
    lease_token: Uuid,
    lease_expires_at: time::OffsetDateTime,
}

fn try_into_pending(
    row: ExtensionDeliveryRow,
) -> Result<PendingExtensionDelivery, ApplicationError> {
    let ExtensionDeliveryRow {
        delivery_id,
        event_id,
        event_kind,
        payload,
        extension_id,
        name,
        description,
        endpoint,
        subscribed_events,
        capabilities,
        token_scopes,
        status,
        signing_secret_ref,
        attempt_count,
        available_at,
        lease_owner,
        lease_token,
        lease_expires_at,
    } = row;
    let parse_set = |value: Value| {
        serde_json::from_value::<BTreeSet<String>>(value)
            .map_err(|_| ApplicationError::port_failure("extension registration is invalid"))
    };
    let status = match status.as_str() {
        "active" => ExtensionStatus::Active,
        "suspended" => ExtensionStatus::Suspended,
        _ => {
            return Err(ApplicationError::port_failure(
                "extension registration is invalid",
            ));
        }
    };
    Ok(PendingExtensionDelivery {
        delivery_id,
        event_id,
        event_kind,
        payload,
        registration: ExtensionRegistration {
            extension_id,
            name,
            description,
            endpoint,
            subscribed_events: parse_set(subscribed_events)?,
            capabilities: parse_set(capabilities)?,
            token_scopes: parse_set(token_scopes)?,
            status,
            signing_secret_ref,
        },
        attempt_count: u32::try_from(attempt_count).unwrap_or(u32::MAX),
        available_at,
        lease_owner,
        lease_token,
        lease_expires_at,
    })
}

impl PgExtensionRegistrationStore {
    /// Creates a store backed by `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn map_error(error: StorageError) -> ApplicationError {
        match error {
            StorageError::DuplicatePrecommitCapability => {
                ApplicationError::new(ApplicationErrorKind::Conflict, error.to_string())
            }
            other => ApplicationError::new(ApplicationErrorKind::PortFailure, other.to_string()),
        }
    }

    /// Prefix marking a capability as a pre-commit-hook capability subject
    /// to the ADR-025 §2.7 at-most-one-active-holder rule. Capabilities that
    /// do not use this prefix (e.g. plain command/event capabilities) are
    /// unaffected and may be shared across registrations as before.
    const PRECOMMIT_CAPABILITY_PREFIX: &'static str = "hooks:";

    /// Serializes a Postgres advisory-lock key from a fixed, namespaced
    /// string so every `save` call contends on the exact same lock
    /// regardless of which row it is about to write. This turns the
    /// check-then-write race on `extension_registrations` into a real
    /// critical section: a concurrent `save` for a different `extension_id`
    /// cannot commit a conflicting capability between this transaction's
    /// SELECT and its INSERT/UPDATE, because it blocks on the same lock
    /// until this transaction commits or rolls back.
    fn precommit_capability_lock_key() -> i64 {
        // A stable 63-bit key derived from a fixed string, not from any
        // row data, so all callers hash to the identical lock.
        const NAMESPACE: &str = "orbisync:extension_registrations:precommit_capability";
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in NAMESPACE.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        (hash & 0x7fff_ffff_ffff_ffff) as i64
    }

    async fn save(&self, registration: ExtensionRegistration) -> Result<(), StorageError> {
        registration
            .validate()
            .map_err(|_| StorageError::InvalidExtensionRegistration)?;
        let subscribed_events = serde_json::to_value(&registration.subscribed_events)
            .map_err(|_| StorageError::InvalidExtensionRegistration)?;
        let capabilities = serde_json::to_value(&registration.capabilities)
            .map_err(|_| StorageError::InvalidExtensionRegistration)?;
        let token_scopes = serde_json::to_value(&registration.token_scopes)
            .map_err(|_| StorageError::InvalidExtensionRegistration)?;

        let mut transaction = self.pool.begin().await.map_err(StorageError::Database)?;

        // Serializes with every other `save` (see `precommit_capability_lock_key`)
        // for the lifetime of this transaction, so the conflict check below
        // and the write it guards observe a consistent snapshot even under
        // concurrent registration attempts.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(Self::precommit_capability_lock_key())
            .execute(&mut *transaction)
            .await
            .map_err(StorageError::Database)?;

        if registration.status == ExtensionStatus::Active {
            let precommit_capabilities: Vec<String> = registration
                .capabilities
                .iter()
                .filter(|capability| capability.starts_with(Self::PRECOMMIT_CAPABILITY_PREFIX))
                .cloned()
                .collect();
            if !precommit_capabilities.is_empty() {
                let conflict: Option<Uuid> = sqlx::query_scalar(
                    "SELECT extension_id FROM extension_registrations \
                     WHERE status = 'active' AND extension_id <> $1 AND capabilities ?| $2 \
                     LIMIT 1",
                )
                .bind(registration.extension_id)
                .bind(&precommit_capabilities)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(StorageError::Database)?;
                if conflict.is_some() {
                    // Roll back explicitly: leaving the transaction to drop
                    // would also roll back, but doing it here documents that
                    // no partial write happens on this path.
                    transaction
                        .rollback()
                        .await
                        .map_err(StorageError::Database)?;
                    return Err(StorageError::DuplicatePrecommitCapability);
                }
            }
        }

        sqlx::query(
            "INSERT INTO extension_registrations (extension_id, name, description, endpoint, subscribed_events, capabilities, token_scopes, status, signing_secret_ref) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (extension_id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, endpoint = EXCLUDED.endpoint, subscribed_events = EXCLUDED.subscribed_events, capabilities = EXCLUDED.capabilities, token_scopes = EXCLUDED.token_scopes, status = EXCLUDED.status, signing_secret_ref = EXCLUDED.signing_secret_ref, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(registration.extension_id)
        .bind(registration.name)
        .bind(registration.description)
        .bind(registration.endpoint)
        .bind(subscribed_events)
        .bind(capabilities)
        .bind(token_scopes)
        .bind(registration.status.as_str())
        .bind(registration.signing_secret_ref)
        .execute(&mut *transaction)
        .await
        .map_err(StorageError::Database)?;
        transaction.commit().await.map_err(StorageError::Database)?;
        Ok(())
    }

    async fn find(
        &self,
        extension_id: Uuid,
    ) -> Result<Option<ExtensionRegistration>, StorageError> {
        let row: Option<ExtensionRegistrationRow> = sqlx::query_as("SELECT extension_id, name, description, endpoint, subscribed_events, capabilities, token_scopes, status, signing_secret_ref FROM extension_registrations WHERE extension_id = $1")
            .bind(extension_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::Database)?;
        let Some((
            extension_id,
            name,
            description,
            endpoint,
            subscribed_events,
            capabilities,
            token_scopes,
            status,
            signing_secret_ref,
        )) = row
        else {
            return Ok(None);
        };
        let status = match status.as_str() {
            "active" => ExtensionStatus::Active,
            "suspended" => ExtensionStatus::Suspended,
            _ => return Err(StorageError::InvalidExtensionRegistration),
        };
        let parse_set = |value: Value| {
            serde_json::from_value::<BTreeSet<String>>(value)
                .map_err(|_| StorageError::InvalidExtensionRegistration)
        };
        Ok(Some(ExtensionRegistration {
            extension_id,
            name,
            description,
            endpoint,
            subscribed_events: parse_set(subscribed_events)?,
            capabilities: parse_set(capabilities)?,
            token_scopes: parse_set(token_scopes)?,
            status,
            signing_secret_ref,
        }))
    }

    async fn find_active_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, StorageError> {
        // LIMIT 2, not 1: fetching one extra row is how this distinguishes
        // "exactly one holder" from "more than one holder" without a
        // separate COUNT query. `save_registration` prevents new duplicates,
        // but a duplicate created before that check existed (e.g. a row
        // inserted directly with SQL) must not be resolved here by silently
        // picking one — it fails closed instead (ADR-025 §2.7).
        let mut rows: Vec<ExtensionRegistrationRow> = sqlx::query_as(
            "SELECT extension_id, name, description, endpoint, subscribed_events, capabilities, token_scopes, status, signing_secret_ref \
             FROM extension_registrations \
             WHERE status = 'active' AND capabilities @> jsonb_build_array($1::text) \
             ORDER BY extension_id LIMIT 2",
        )
        .bind(capability)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::Database)?;
        if rows.len() > 1 {
            return Err(StorageError::DuplicatePrecommitCapability);
        }
        let row = rows.pop();
        let Some((
            extension_id,
            name,
            description,
            endpoint,
            subscribed_events,
            capabilities,
            token_scopes,
            status,
            signing_secret_ref,
        )) = row
        else {
            return Ok(None);
        };
        let status = match status.as_str() {
            "active" => ExtensionStatus::Active,
            "suspended" => ExtensionStatus::Suspended,
            _ => return Err(StorageError::InvalidExtensionRegistration),
        };
        let parse_set = |value: Value| {
            serde_json::from_value::<BTreeSet<String>>(value)
                .map_err(|_| StorageError::InvalidExtensionRegistration)
        };
        Ok(Some(ExtensionRegistration {
            extension_id,
            name,
            description,
            endpoint,
            subscribed_events: parse_set(subscribed_events)?,
            capabilities: parse_set(capabilities)?,
            token_scopes: parse_set(token_scopes)?,
            status,
            signing_secret_ref,
        }))
    }
}

#[async_trait::async_trait]
impl ExtensionRegistrationStore for PgExtensionRegistrationStore {
    async fn save_registration(
        &self,
        registration: ExtensionRegistration,
    ) -> Result<(), ApplicationError> {
        self.save(registration).await.map_err(Self::map_error)
    }

    async fn find_registration(
        &self,
        extension_id: Uuid,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        self.find(extension_id).await.map_err(Self::map_error)
    }

    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        self.find_active_by_capability(capability)
            .await
            .map_err(Self::map_error)
    }
}
