//! Closed-schema record reader. Rust futures retain parser continuation; the
//! explicit driver supplies input and a bounded work allowance. No executor,
//! background task, JSON tree or complete encoded record is retained.

use super::{Number, NumberKind, NumberReader, ScalarError, StringReader, WORK_QUANTUM};
use orbisync_domain::{
    Entity, EntityId, EntityKind, InstanceId, Quaternion, Revision, RoleId, Timestamp, Transform,
    UserId, Vec3, VisibilityPolicy,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

/// Original receipt fields. Response bytes remain opaque.
#[derive(Debug)]
pub struct Receipt {
    /// Command identity.
    pub command_id: String,
    /// Payload fingerprint.
    pub fingerprint: Vec<u8>,
    /// Original completion time.
    pub created_at_millis: i64,
    /// Original expiration time.
    pub expires_at_millis: i64,
    /// Original replay envelope identity.
    pub message_id: String,
    /// Deterministic replay outcome.
    pub result: crate::checkpoint_record::ReceiptResult,
}

/// Record schema chosen by its enclosing array.
#[derive(Debug, Clone, Copy)]
pub enum RecordKind {
    /// Domain entity.
    Entity,
    /// Original command receipt.
    Receipt,
}
/// Successfully validated record. Storage must count this temporary domain
/// state as scratch until it drops it; runtime can transfer it to restored state.
#[derive(Debug)]
pub enum Record {
    /// Valid domain entity, preserving opaque component bytes.
    Entity(Entity),
    /// Structurally valid receipt; stream-relative time/aggregate checks remain
    /// the enclosing stream validator's responsibility.
    Receipt(Receipt),
}
/// Decoder progress; a digest exists only after schema/domain validation.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Fixed control scratch; avoid an extra heap allocation on transfer.
pub enum Progress {
    /// More input is needed.
    Input,
    /// Work allowance exhausted; resume even when the next input is empty.
    Yield,
    /// Exact canonical record bytes and semantic record have been validated.
    Ready {
        /// Validated record.
        record: Record,
        /// SHA-256 of the original canonical bytes.
        digest: [u8; 32],
    },
}
/// Work and input consumed by a single explicit poll cycle.
#[derive(Debug)]
pub struct Step {
    /// Bytes accepted from this input window.
    pub consumed: usize,
    /// Charged data work, including input copies and record hashing.
    pub work: usize,
    /// Current continuation state.
    pub progress: Progress,
}

/// Conservative codec-owned allocation accounting. The allocation sum retains
/// charges after temporary destruction, so it bounds simultaneous requested
/// capacities. Allocator bookkeeping and native call-stack high-water are not
/// measured by this counter and must be audited separately.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceStats {
    /// Future continuation, driver/shared state and accumulated allocation charges.
    pub scratch_capacity_bound: usize,
    /// Largest observed charged step.
    pub max_step_work: usize,
    /// Sum of charged steps.
    pub total_work: usize,
    /// Exact bytes hashed as record input.
    pub encoded_bytes: usize,
}

struct Shared {
    byte: Option<u8>,
    budget: usize,
    work: usize,
    waiting_input: bool,
    hash: Sha256,
    bytes: usize,
    limit: usize,
    stats: ResourceStats,
}
#[derive(Clone)]
struct Input(Arc<Mutex<Shared>>);
impl Input {
    fn allocation(&self, bytes: usize) -> Result<(), ScalarError> {
        let mut state = self.0.lock().map_err(|_| ScalarError::Invalid)?;
        state.stats.scratch_capacity_bound = state
            .stats
            .scratch_capacity_bound
            .checked_add(bytes)
            .ok_or(ScalarError::Invalid)?;
        if state.stats.scratch_capacity_bound > 2_097_152 {
            return Err(ScalarError::Invalid);
        }
        Ok(())
    }
    async fn timestamp(&self) -> Result<Timestamp, ScalarError> {
        let text = self.text(64).await?;
        self.charge(1024).await?;
        let timestamp = Timestamp::parse_rfc3339(&text).map_err(|_| ScalarError::Invalid)?;
        self.allocation(128)?; // bounded RFC3339 formatting including growth overlap
        if timestamp.to_rfc3339().map_err(|_| ScalarError::Invalid)? != text {
            return Err(ScalarError::Invalid);
        }
        Ok(timestamp)
    }
    async fn uuid_text(&self) -> Result<String, ScalarError> {
        let text = self.text(36).await?;
        self.charge(256).await?;
        let id = uuid::Uuid::parse_str(&text).map_err(|_| ScalarError::Invalid)?;
        let mut spelling = [0; 36];
        if id.hyphenated().encode_lower(&mut spelling) != text.as_str() {
            return Err(ScalarError::Invalid);
        }
        Ok(text)
    }
    async fn charge(&self, amount: usize) -> Result<(), ScalarError> {
        poll_fn(|_| {
            let Ok(mut state) = self.0.lock() else {
                return Poll::Ready(Err(ScalarError::Invalid));
            };
            if state.budget - state.work < amount {
                return Poll::Pending;
            }
            state.work += amount;
            Poll::Ready(Ok(()))
        })
        .await
    }
    async fn peek(&self) -> Result<u8, ScalarError> {
        poll_fn(|_| {
            let Ok(mut state) = self.0.lock() else {
                return Poll::Ready(Err(ScalarError::Invalid));
            };
            if state.budget - state.work < 32 {
                return Poll::Pending;
            }
            match state.byte {
                Some(byte) => {
                    state.work += 32;
                    Poll::Ready(Ok(byte))
                }
                None => {
                    state.waiting_input = true;
                    Poll::Pending
                }
            }
        })
        .await
    }
    async fn byte(&self) -> Result<u8, ScalarError> {
        poll_fn(|_| {
            let Ok(mut state) = self.0.lock() else {
                return Poll::Ready(Err(ScalarError::Invalid));
            };
            // Includes input load/copy, scalar transition and SHA byte input.
            // SHA block compression is fixed-size; reserve it on every byte.
            if state.budget - state.work < 256 {
                return Poll::Pending;
            }
            let Some(byte) = state.byte.take() else {
                state.waiting_input = true;
                return Poll::Pending;
            };
            if state.bytes == state.limit {
                return Poll::Ready(Err(ScalarError::Invalid));
            }
            state.work += 256;
            state.bytes += 1;
            state.hash.update([byte]);
            Poll::Ready(Ok(byte))
        })
        .await
    }
    async fn literal(&self, literal: &[u8]) -> Result<(), ScalarError> {
        for &byte in literal {
            if self.byte().await? != byte {
                return Err(ScalarError::Invalid);
            }
        }
        Ok(())
    }
    async fn text(&self, cap: usize) -> Result<String, ScalarError> {
        self.charge(256).await?;
        let mut reader = StringReader::new(cap)?;
        self.allocation(reader.scratch_bytes())?;
        let flag = AtomicBool::new(false);
        loop {
            if reader.push(self.byte().await?, &flag)? {
                let value = reader.finish(&flag)?;
                if value.capacity() == value.len() {
                    return Ok(value);
                }
                let mut compact = String::with_capacity(value.len());
                self.allocation(compact.capacity())?;
                for character in value.chars() {
                    self.charge(32).await?;
                    compact.push(character);
                }
                return Ok(compact);
            }
        }
    }
    async fn number(&self, kind: NumberKind) -> Result<Number, ScalarError> {
        let mut reader = NumberReader::default();
        let flag = AtomicBool::new(false);
        loop {
            let byte = self.peek().await?;
            if matches!(byte, b',' | b'}' | b']') {
                break;
            }
            reader.push(self.byte().await?, &flag)?;
        }
        self.charge(2048).await?;
        reader.finish(kind, &flag)
    }
    async fn unsigned(&self) -> Result<u64, ScalarError> {
        match self.number(NumberKind::Unsigned).await? {
            Number::Unsigned(n) => Ok(n),
            _ => Err(ScalarError::Invalid),
        }
    }
    async fn signed(&self) -> Result<i64, ScalarError> {
        match self.number(NumberKind::Signed).await? {
            Number::Signed(n) => Ok(n),
            _ => Err(ScalarError::Invalid),
        }
    }
    async fn float(&self) -> Result<f32, ScalarError> {
        match self.number(NumberKind::Float).await? {
            Number::Float(n) => Ok(n),
            _ => Err(ScalarError::Invalid),
        }
    }
    async fn bytes(&self, cap: usize) -> Result<Vec<u8>, ScalarError> {
        self.literal(b"[").await?;
        self.charge(256).await?;
        // Reserve once without initializing/copying cap bytes. Every push is
        // within this capacity. Deallocation does not scan u8 elements.
        let mut bytes = Vec::with_capacity(cap);
        self.allocation(bytes.capacity() + size_of::<Vec<u8>>())?;
        if self.peek().await? != b']' {
            loop {
                if bytes.len() == cap {
                    return Err(ScalarError::Invalid);
                }
                let value = self.unsigned().await?;
                bytes.push(u8::try_from(value).map_err(|_| ScalarError::Invalid)?);
                if self.peek().await? == b']' {
                    break;
                }
                self.literal(b",").await?;
            }
        }
        self.literal(b"]").await?;
        // The schema-sized reservation is scratch, not a reason to retain
        // 256 KiB for a one-byte receipt in every unpublished assembly slot.
        if bytes.capacity() != bytes.len() {
            let mut compact = Vec::with_capacity(bytes.len());
            self.allocation(compact.capacity())?;
            for page in bytes.chunks(512) {
                self.charge(2048).await?;
                compact.extend_from_slice(page);
            }
            return Ok(compact);
        }
        Ok(bytes)
    }
    async fn vector(&self) -> Result<Vec3, ScalarError> {
        self.literal(b"{\"x\":").await?;
        let x = self.float().await?;
        self.literal(b",\"y\":").await?;
        let y = self.float().await?;
        self.literal(b",\"z\":").await?;
        let z = self.float().await?;
        self.literal(b"}").await?;
        Vec3::new(x, y, z).map_err(|_| ScalarError::Invalid)
    }
    async fn transform(&self) -> Result<Option<Transform>, ScalarError> {
        if self.peek().await? == b'n' {
            self.literal(b"null").await?;
            return Ok(None);
        }
        self.literal(b"{\"position\":").await?;
        let position = self.vector().await?;
        self.literal(b",\"rotation\":{\"x\":").await?;
        let x = self.float().await?;
        self.literal(b",\"y\":").await?;
        let y = self.float().await?;
        self.literal(b",\"z\":").await?;
        let z = self.float().await?;
        self.literal(b",\"w\":").await?;
        let w = self.float().await?;
        self.literal(b"},\"scale\":").await?;
        let scale = self.vector().await?;
        self.literal(b"}").await?;
        let rotation = Quaternion::new(x, y, z, w).map_err(|_| ScalarError::Invalid)?;
        Transform::new(position, rotation, scale)
            .map(Some)
            .map_err(|_| ScalarError::Invalid)
    }
    async fn strings(&self, count: usize) -> Result<Vec<String>, ScalarError> {
        self.literal(b"[").await?;
        let mut values = Vec::with_capacity(count);
        self.allocation(values.capacity() * size_of::<String>())?;
        if self.peek().await? != b']' {
            loop {
                if values.len() == count {
                    return Err(ScalarError::Invalid);
                }
                values.push(self.uuid_text().await?);
                if self.peek().await? == b']' {
                    break;
                }
                self.literal(b",").await?;
            }
        }
        self.literal(b"]").await?;
        Ok(values)
    }
    async fn visibility(&self) -> Result<VisibilityPolicy, ScalarError> {
        self.literal(b"{\"type\":").await?;
        let kind = self.text(15).await?;
        let value = match kind.as_str() {
            "global" => VisibilityPolicy::Global,
            "owner_only" => VisibilityPolicy::OwnerOnly,
            "spatial" => {
                self.literal(b",\"radius\":").await?;
                VisibilityPolicy::spatial(self.float().await?).map_err(|_| ScalarError::Invalid)?
            }
            "custom" => {
                self.literal(b",\"tag\":").await?;
                VisibilityPolicy::custom(self.text(64).await?).map_err(|_| ScalarError::Invalid)?
            }
            "role_restricted" => {
                self.literal(b",\"roles\":").await?;
                let values = self.strings(16).await?;
                self.allocation(16 * 256)?;
                let mut ids = std::collections::BTreeSet::new();
                for value in &values {
                    self.charge(4096).await?;
                    ids.insert(RoleId::parse(value).map_err(|_| ScalarError::Invalid)?);
                }
                self.charge(256).await?;
                VisibilityPolicy::from_role_set(ids).map_err(|_| ScalarError::Invalid)?
            }
            "explicit" => {
                self.literal(b",\"users\":").await?;
                let values = self.strings(64).await?;
                self.allocation(64 * 256)?;
                let mut ids = std::collections::BTreeSet::new();
                for value in &values {
                    self.charge(4096).await?;
                    ids.insert(UserId::parse(value).map_err(|_| ScalarError::Invalid)?);
                }
                self.charge(256).await?;
                VisibilityPolicy::from_user_set(ids).map_err(|_| ScalarError::Invalid)?
            }
            _ => return Err(ScalarError::Invalid),
        };
        self.literal(b"}").await?;
        Ok(value)
    }
    async fn entity(&self) -> Result<Record, ScalarError> {
        self.literal(b"{\"id\":").await?;
        let id = EntityId::parse(&self.uuid_text().await?).map_err(|_| ScalarError::Invalid)?;
        self.literal(b",\"instance_id\":").await?;
        let instance =
            InstanceId::parse(&self.uuid_text().await?).map_err(|_| ScalarError::Invalid)?;
        self.literal(b",\"kind\":").await?;
        let kind = EntityKind::parse(&self.text(7).await?).map_err(|_| ScalarError::Invalid)?;
        self.literal(b",\"owner\":").await?;
        let owner = if self.peek().await? == b'n' {
            self.literal(b"null").await?;
            None
        } else {
            Some(UserId::parse(&self.uuid_text().await?).map_err(|_| ScalarError::Invalid)?)
        };
        self.literal(b",\"transform\":").await?;
        let transform = self.transform().await?;
        self.literal(b",\"visibility\":").await?;
        let visibility = self.visibility().await?;
        self.literal(b",\"revision\":").await?;
        let revision = Revision::from_u64(self.unsigned().await?);
        self.literal(b",\"created_at\":").await?;
        let created = self.timestamp().await?;
        self.literal(b",\"updated_at\":").await?;
        let updated = self.timestamp().await?;
        self.literal(b",\"components\":{").await?;
        let mut components = HashMap::with_capacity(16);
        self.allocation(16 * 256)?;
        let mut previous = String::with_capacity(128);
        self.allocation(previous.capacity())?;
        if self.peek().await? != b'}' {
            loop {
                if components.len() == 16 {
                    return Err(ScalarError::Invalid);
                }
                let key = self.text(128).await?;
                self.charge(512).await?;
                if key <= previous {
                    return Err(ScalarError::Invalid);
                }
                previous.clone_from(&key);
                self.literal(b":").await?;
                let bytes = self.bytes(4096).await?;
                self.charge(8192).await?;
                components.insert(key, bytes);
                if self.peek().await? == b'}' {
                    break;
                }
                self.literal(b",").await?;
            }
        }
        self.literal(b"}}").await?;
        // <=16 bounded keys/entries; account for both tables during rehash.
        self.charge(8192).await?;
        self.allocation(16 * 256)?;
        components.shrink_to_fit();
        self.charge(8192).await?;
        Entity::from_persisted(
            id, instance, kind, owner, transform, visibility, revision, created, updated,
            components,
        )
        .map(Record::Entity)
        .map_err(|_| ScalarError::Invalid)
    }
    async fn receipt(&self) -> Result<Record, ScalarError> {
        self.literal(b"{\"command_id\":").await?;
        let command_id = self.uuid_text().await?;
        orbisync_domain::CommandId::parse(&command_id).map_err(|_| ScalarError::Invalid)?;
        self.literal(b",\"fingerprint\":").await?;
        let fingerprint = self.bytes(32).await?;
        if fingerprint.len() != 32 {
            return Err(ScalarError::Invalid);
        }
        self.literal(b",\"created_at_millis\":").await?;
        let created_at_millis = self.signed().await?;
        self.literal(b",\"expires_at_millis\":").await?;
        let expires_at_millis = self.signed().await?;
        if expires_at_millis.checked_sub(created_at_millis) != Some(86_400_000) {
            return Err(ScalarError::Invalid);
        }
        self.literal(b",\"message_id\":").await?;
        let message_id = self.uuid_text().await?;
        orbisync_domain::CommandId::parse(&message_id).map_err(|_| ScalarError::Invalid)?;
        self.literal(b",\"result\":{\"type\":").await?;
        let kind = self.text(8).await?;
        use crate::checkpoint_record::ReceiptResult;
        let result = match kind.as_str() {
            "applied" => {
                self.literal(b",\"response_payload\":").await?;
                let response_payload = self.bytes(crate::checkpoint_record::RESPONSE_BYTES).await?;
                if response_payload.is_empty() {
                    return Err(ScalarError::Invalid);
                }
                ReceiptResult::Applied { response_payload }
            }
            "rejected" => {
                self.literal(b",\"code\":").await?;
                let code = self.text(crate::checkpoint_record::CODE_BYTES).await?;
                if code.is_empty() {
                    return Err(ScalarError::Invalid);
                }
                self.literal(b",\"detail\":").await?;
                let detail = self.text(crate::checkpoint_record::DETAIL_BYTES).await?;
                ReceiptResult::Rejected { code, detail }
            }
            _ => return Err(ScalarError::Invalid),
        };
        self.literal(b"}}").await?;
        Ok(Record::Receipt(Receipt {
            command_id,
            fingerprint,
            created_at_millis,
            expires_at_millis,
            message_id,
            result,
        }))
    }
}

type Continuation = Pin<Box<dyn Future<Output = Result<Record, ScalarError>> + Send>>;
/// Owned, resumable record continuation. Cancellation and errors are terminal.
pub struct RecordDecoder {
    input: Input,
    future: Option<Continuation>,
    failed: Option<ScalarError>,
}
impl Drop for RecordDecoder {
    fn drop(&mut self) {
        // At most one closed-schema record: <=16 component entries, <=64
        // visibility IDs, fixed scalar continuations and byte buffers (no
        // per-byte destructor). No stream-sized collection is reachable here.
        let _observed =
            super::work::observe(&AtomicBool::new(false), super::work::Stage::Cleanup, 8192);
        self.future = None;
    }
}
impl RecordDecoder {
    /// Start one independent canonical record at its opening brace.
    pub fn new(kind: RecordKind) -> Self {
        let input = Input(Arc::new(Mutex::new(Shared {
            byte: None,
            budget: 0,
            work: 0,
            waiting_input: false,
            hash: Sha256::new(),
            bytes: 0,
            limit: match kind {
                RecordKind::Entity => 1_048_576,
                RecordKind::Receipt => 2_097_152,
            },
            stats: ResourceStats::default(),
        })));
        let reader = input.clone();
        let future = Box::pin(async move {
            let record = match kind {
                RecordKind::Entity => reader.entity().await?,
                RecordKind::Receipt => reader.receipt().await?,
            };
            reader.charge(1024).await?;
            Ok(record)
        });
        if let Ok(mut state) = input.0.lock() {
            state.stats.scratch_capacity_bound = size_of_val(future.as_ref().get_ref())
                + size_of::<Shared>() + 2 * size_of::<usize>() // Arc counters
                + size_of::<Self>()
                + size_of::<Step>(); // Native stack and allocator overhead are measured separately.
        }
        Self {
            input,
            future: Some(future),
            failed: None,
        }
    }
    /// Current capacity/work evidence, including temporary validation state.
    pub fn stats(&self) -> ResourceStats {
        self.input
            .0
            .lock()
            .map(|state| state.stats)
            .unwrap_or_default()
    }
    /// Drive parsing/validation/hash work using at most `budget` (capped at
    /// 16384). A budget below the next indivisible scalar/domain operation
    /// yields without consuming that operation. Use 16384 for forward progress.
    pub fn step(
        &mut self,
        input: &[u8],
        budget: usize,
        cancelled: &AtomicBool,
    ) -> Result<Step, ScalarError> {
        let _observed = super::work::observe(
            cancelled,
            super::work::Stage::Parser,
            budget.min(WORK_QUANTUM),
        );
        if cancelled.load(Ordering::Acquire) {
            self.failed = Some(ScalarError::Cancelled);
            let _observed = super::work::observe(cancelled, super::work::Stage::Cleanup, 8192);
            self.future = None;
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        let future = self.future.as_mut().ok_or(ScalarError::Invalid)?;
        {
            let mut state = self.input.0.lock().map_err(|_| ScalarError::Invalid)?;
            state.budget = budget.min(WORK_QUANTUM);
            state.work = 0;
            state.waiting_input = false;
        }
        let mut consumed = 0;
        let mut context = Context::from_waker(Waker::noop());
        loop {
            let polled = future.as_mut().poll(&mut context);
            let mut state = self.input.0.lock().map_err(|_| ScalarError::Invalid)?;
            match polled {
                Poll::Ready(Ok(record)) => {
                    let digest = state.hash.clone().finalize().into();
                    let work = state.work;
                    state.stats.max_step_work = state.stats.max_step_work.max(work);
                    state.stats.total_work += work;
                    state.stats.encoded_bytes = state.bytes;
                    drop(state);
                    self.future = None;
                    return Ok(Step {
                        consumed,
                        work,
                        progress: Progress::Ready { record, digest },
                    });
                }
                Poll::Ready(Err(error)) => {
                    drop(state);
                    self.failed = Some(error);
                    let _observed =
                        super::work::observe(cancelled, super::work::Stage::Cleanup, 8192);
                    self.future = None;
                    return Err(error);
                }
                Poll::Pending => {
                    if state.waiting_input && consumed < input.len() {
                        state.byte = Some(input[consumed]);
                        consumed += 1;
                        state.waiting_input = false;
                    } else {
                        state.stats.max_step_work = state.stats.max_step_work.max(state.work);
                        state.stats.total_work += state.work;
                        state.stats.encoded_bytes = state.bytes;
                        let progress = if state.waiting_input {
                            Progress::Input
                        } else {
                            Progress::Yield
                        };
                        return Ok(Step {
                            consumed,
                            work: state.work,
                            progress,
                        });
                    }
                }
            }
        }
    }
}
