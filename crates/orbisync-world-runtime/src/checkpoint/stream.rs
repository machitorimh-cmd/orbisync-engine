//! Canonical v5 generation streams. Legacy JSON readers stay separate.
//! A job retains an O(state) pinned snapshot plus at most three C buffers.
//! Decoder scratch is one bounded record; restored domain state is additional.
use super::*;
use orbisync_application::checkpoint_record::canonical::work::{Stage, observe};
use orbisync_application::{
    ApplicationError, CheckpointLimits,
    checkpoint_stream::{CheckpointSource, StreamManifest},
};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::mpsc;

const ENTITY: usize = 1024 * 1024;
const RECEIPT: usize = 2 * ENTITY;
const ENTITIES: usize = 65_536;
fn error(detail: impl std::fmt::Display) -> ApplicationError {
    ApplicationError::port_failure(detail.to_string())
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg(test)]
enum Visibility<'a> {
    Global,
    Spatial { radius: f32 },
    OwnerOnly,
    RoleRestricted { roles: Vec<String> },
    Explicit { users: Vec<String> },
    Custom { tag: &'a str },
}
#[derive(Serialize)]
#[cfg(test)]
pub(super) struct BorrowedEntity<'a> {
    id: String,
    instance_id: String,
    kind: &'a str,
    owner: Option<String>,
    transform: Option<StoredTransform>,
    visibility: Visibility<'a>,
    revision: u64,
    created_at: String,
    updated_at: String,
    components: BTreeMap<&'a str, &'a [u8]>,
}
#[cfg(test)]
pub(super) fn borrowed_entity<'a>(
    entity: &'a Entity,
    cancelled: &AtomicBool,
) -> Result<BorrowedEntity<'a>, serde_json::Error> {
    let mut components = BTreeMap::new();
    let mut lower_bound = 0usize;
    for (k, v) in entity.components() {
        if cancelled.load(Ordering::Acquire) {
            return Err(serde_json::Error::custom("checkpoint cancelled"));
        }
        lower_bound = lower_bound
            .saturating_add(k.len())
            .saturating_add(v.len())
            .saturating_add(6);
        if lower_bound > ENTITY {
            return Err(serde_json::Error::custom("entity record limit"));
        }
        components.insert(k.as_str(), v.as_slice());
    }
    let visibility = match entity.visibility() {
        VisibilityPolicy::Global => Visibility::Global,
        VisibilityPolicy::OwnerOnly => Visibility::OwnerOnly,
        VisibilityPolicy::Spatial { radius } => Visibility::Spatial { radius: *radius },
        VisibilityPolicy::RoleRestricted { roles } => Visibility::RoleRestricted {
            roles: roles.iter().map(ToString::to_string).collect(),
        },
        VisibilityPolicy::Explicit { users } => Visibility::Explicit {
            users: users.iter().map(ToString::to_string).collect(),
        },
        VisibilityPolicy::Custom { tag } => Visibility::Custom { tag: tag.as_str() },
    };
    Ok(BorrowedEntity {
        id: entity.id().to_string(),
        instance_id: entity.instance_id().to_string(),
        kind: entity.kind().as_str(),
        owner: entity.owner().map(|v| v.to_string()),
        transform: entity.transform().map(stored_transform),
        visibility,
        revision: entity.revision().as_u64(),
        created_at: entity
            .created_at()
            .to_rfc3339()
            .map_err(serde_json::Error::custom)?,
        updated_at: entity
            .updated_at()
            .to_rfc3339()
            .map_err(serde_json::Error::custom)?,
        components,
    })
}

/// Observed buffer high-water marks; excludes pinned/restored state and driver copies.
#[derive(Debug, Default)]
pub struct StreamStats {
    producer: AtomicUsize,
    queued: AtomicUsize,
    consumer: AtomicUsize,
    record: AtomicUsize,
    hash_unit: AtomicUsize,
    hashed: AtomicUsize,
    canonical_scratch: AtomicUsize,
    canonical_work: AtomicUsize,
    cleaned: AtomicUsize,
    assembly_slots: AtomicUsize,
    assembly_heap: AtomicUsize,
    assembly_live: AtomicUsize,
    assembly_peak: AtomicUsize,
    assembly_slots_peak: AtomicUsize,
    assembly_heap_peak: AtomicUsize,
    assemblies: AtomicUsize,
    assemblies_peak: AtomicUsize,
    stopped: AtomicBool,
}
impl StreamStats {
    /// Separately charged unpublished output: slot-capacity peak, retained
    /// heap-charge peak, simultaneous total peak, current live charge, current
    /// assemblies, peak simultaneous assemblies. Share stats to measure jobs
    /// together; callers still enforce the existing two-job process gate.
    /// Heap charges include conservative container storage, not allocator metadata.
    pub fn assembly_resources(&self) -> [usize; 6] {
        [
            self.assembly_slots_peak.load(Ordering::Relaxed),
            self.assembly_heap_peak.load(Ordering::Relaxed),
            self.assembly_peak.load(Ordering::Relaxed),
            self.assembly_live.load(Ordering::Relaxed),
            self.assemblies.load(Ordering::Relaxed),
            self.assemblies_peak.load(Ordering::Relaxed),
        ]
    }
    /// Retained records destroyed through observed cleanup steps.
    pub fn cleaned_records(&self) -> usize {
        self.cleaned.load(Ordering::Relaxed)
    }
    /// Producer, queue, consumer buffer peaks and largest visited encoded record in bytes.
    pub fn peaks(&self) -> [usize; 4] {
        [
            self.producer.load(Ordering::Relaxed),
            self.queued.load(Ordering::Relaxed),
            self.consumer.load(Ordering::Relaxed),
            self.record.load(Ordering::Relaxed),
        ]
    }
    /// Producer or decoder worker has finished, including after explicit cancellation.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    /// Largest synchronous input hash unit and total bytes hashed by the decoder.
    /// This does not measure record deserialization or semantic validation work.
    pub fn hash_work(&self) -> [usize; 2] {
        [
            self.hash_unit.load(Ordering::Relaxed),
            self.hashed.load(Ordering::Relaxed),
        ]
    }
    /// Maximum shared-reader plus active identity-index capacity and step work.
    /// Includes temporary domain validation fields before ownership transfer;
    /// excludes allocator bookkeeping, native stack and retained stream state.
    pub fn canonical_resources(&self) -> [usize; 2] {
        [
            self.canonical_scratch.load(Ordering::Relaxed),
            self.canonical_work.load(Ordering::Relaxed),
        ]
    }
}

// Lexical guard runs before allocation/deserialization, including skipped tokens.
#[derive(Default)]
struct Lexical {
    depth: usize,
    string: bool,
    escape: bool,
    token: usize,
}
impl Lexical {
    fn byte(&mut self, b: u8) -> io::Result<()> {
        if self.string {
            self.token += 1;
            if self.token > ENTITY {
                return Err(io::Error::other("encoded string token limit"));
            }
            if self.escape {
                self.escape = false;
            } else if b == b'\\' {
                self.escape = true;
            } else if b == b'"' {
                self.string = false;
            }
        } else {
            match b {
                b'"' => {
                    self.string = true;
                    self.token = 1;
                }
                b'{' | b'[' => {
                    self.depth += 1;
                    if self.depth > 32 {
                        return Err(io::Error::other("JSON depth limit"));
                    }
                }
                b'}' | b']' => {
                    self.depth = self
                        .depth
                        .checked_sub(1)
                        .ok_or_else(|| io::Error::other("invalid JSON nesting"))?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(super) fn validate_legacy_structure(bytes: &[u8]) -> Result<(), CheckpointDecodeError> {
    let mut lexical = Lexical::default();
    for &b in bytes {
        lexical
            .byte(b)
            .map_err(|e| CheckpointDecodeError::new(e.to_string()))?;
    }
    Ok(())
}

struct Output<'a, W> {
    sink: W,
    total: usize,
    limit: usize,
    hash: Sha256,
    lexical: Lexical,
    cancelled: &'a AtomicBool,
}
impl<W: Write> Write for Output<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for part in bytes.chunks(512) {
            observe(self.cancelled, Stage::Copy, 8192).map_err(io::Error::other)?;
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::other("checkpoint cancelled"));
            }
            if part.len() > self.limit.saturating_sub(self.total) {
                return Err(io::Error::other("checkpoint total limit"));
            }
            for &b in part {
                self.lexical.byte(b)?;
            }
            self.sink.write_all(part)?;
            for page in part.chunks(32) {
                observe(self.cancelled, Stage::Hash, 8192).map_err(io::Error::other)?;
                self.hash.update(page);
            }
            self.total += part.len();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

fn encode<W: Write>(
    checkpoint: &Checkpoint,
    limits: CheckpointLimits,
    cancelled: &AtomicBool,
    sink: W,
) -> Result<StreamManifest, ApplicationError> {
    if checkpoint.entities.len() > ENTITIES || checkpoint.revision.as_u64() > i64::MAX as u64 {
        return Err(error("entity count or revision limit"));
    }
    validate_dedup_entries_cancellable(&checkpoint.dedup, Some(cancelled)).map_err(error)?;
    let millis = checkpoint.timestamp.to_unix_millis().map_err(error)?;
    for entry in &checkpoint.dedup {
        if cancelled.load(Ordering::Acquire) {
            return Err(error("checkpoint cancelled"));
        }
        if entry.created_at_millis > millis {
            return Err(error("future receipt"));
        }
    }
    let mut out = Output {
        sink,
        total: 0,
        limit: limits.max_serialized_bytes(),
        hash: Sha256::new(),
        lexical: Lexical::default(),
        cancelled,
    };
    write!(
        out,
        "{{\"format_version\":{},\"instance_id\":\"{}\",\"revision\":{},\"timestamp\":",
        orbisync_application::checkpoint_record::canonical::VERSION,
        checkpoint.instance_id,
        checkpoint.revision.as_u64()
    )
    .map_err(error)?;
    serde_json::to_writer(&mut out, &checkpoint.timestamp.to_rfc3339().map_err(error)?)
        .map_err(error)?;
    out.write_all(b",\"entities\":[").map_err(error)?;
    let mut seen =
        orbisync_application::checkpoint_record::canonical::index::PositionIndex::new(ENTITIES);
    for (i, entity) in checkpoint.entities.iter().enumerate() {
        if entity.instance_id() != checkpoint.instance_id || entity.revision() > checkpoint.revision
        {
            return Err(error("entity identity or revision mismatch"));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(error("checkpoint cancelled"));
        }
        observe(cancelled, Stage::Index, 4096).map_err(error)?;
        seen.push(i as u32, |position| {
            checkpoint.entities[position as usize].id().as_uuid()
        })
        .map_err(error)?;
        // Same bounded record serializer and conservative charge as admission.
        entity_charge(entity, cancelled).map_err(error)?;
        if i != 0 {
            out.write_all(b",").map_err(error)?;
        }
        orbisync_application::checkpoint_record::canonical::emit::entity(
            entity, &mut out, cancelled,
        )
        .map_err(error)?;
    }
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(error("checkpoint cancelled"));
        }
        observe(cancelled, Stage::Index, 4096).map_err(error)?;
        if seen
            .drain_step(|position| checkpoint.entities[position as usize].id().as_uuid())
            .map_err(error)?
        {
            break;
        }
    }
    drop(seen);
    out.write_all(b"],\"dedup\":[").map_err(error)?;
    for (i, entry) in checkpoint.dedup.iter().enumerate() {
        receipt_charge(entry, cancelled).map_err(error)?;
        if i != 0 {
            out.write_all(b",").map_err(error)?;
        }
        orbisync_application::checkpoint_record::canonical::emit::receipt(
            borrowed_receipt(entry),
            &mut out,
            cancelled,
        )
        .map_err(error)?;
    }
    out.write_all(b"]}").map_err(error)?;
    out.flush().map_err(error)?;
    let total = out.total as u64;
    Ok(StreamManifest {
        serialized_bytes: total,
        chunk_bytes: limits.chunk_bytes() as u64,
        chunk_count: 1 + (total - 1) / limits.chunk_bytes() as u64,
        digest: out.hash.finalize().into(),
    })
}

impl Checkpoint {
    /// Count/hash without collecting JSON. Call after acquiring global/instance
    /// permits; the immutable snapshot itself may have cloned O(state).
    pub fn stream_manifest(
        &self,
        limits: CheckpointLimits,
        cancelled: &AtomicBool,
    ) -> Result<StreamManifest, ApplicationError> {
        encode(self, limits, cancelled, io::sink())
    }
}

struct ChunkWriter {
    tx: mpsc::Sender<Result<Vec<u8>, ApplicationError>>,
    buffer: Vec<u8>,
    chunk: usize,
    stats: Arc<StreamStats>,
}
impl ChunkWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buffer);
        self.stats.queued.fetch_max(bytes.len(), Ordering::Relaxed);
        self.tx
            .blocking_send(Ok(bytes))
            .map_err(|_| io::Error::other("checkpoint consumer closed"))?;
        self.buffer = Vec::with_capacity(self.chunk);
        Ok(())
    }
}
impl Write for ChunkWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let len = bytes.len();
        while !bytes.is_empty() {
            let n = bytes.len().min(self.chunk - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..n]);
            self.stats
                .producer
                .fetch_max(self.buffer.len(), Ordering::Relaxed);
            bytes = &bytes[n..];
            if self.buffer.len() == self.chunk {
                self.send()?;
            }
        }
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// Bounded capacity-one producer. Explicit cancel joins the blocking worker;
/// drop closes the receiver and signals it, but permit owners must await cancel.
pub struct EncodedCheckpoint {
    limits: CheckpointLimits,
    manifest: StreamManifest,
    rx: mpsc::Receiver<Result<Vec<u8>, ApplicationError>>,
    cancelled: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
    stats: Arc<StreamStats>,
}
impl EncodedCheckpoint {
    /// Start from pinned immutable state and its precomputed manifest. Final
    /// metadata mismatch is an error, never successful EOF.
    pub fn new(
        snapshot: Arc<Checkpoint>,
        limits: CheckpointLimits,
        manifest: StreamManifest,
    ) -> Result<Self, ApplicationError> {
        limits.validate_manifest(&manifest)?;
        if manifest.chunk_bytes != limits.chunk_bytes() as u64 {
            return Err(error("writer chunk policy mismatch"));
        }
        let (tx, rx) = mpsc::channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StreamStats::default());
        let flag = Arc::clone(&cancelled);
        let producer_stats = Arc::clone(&stats);
        let expected = manifest.clone();
        let task = tokio::task::spawn_blocking(move || {
            let writer = ChunkWriter {
                tx: tx.clone(),
                buffer: Vec::with_capacity(limits.chunk_bytes()),
                chunk: limits.chunk_bytes(),
                stats: Arc::clone(&producer_stats),
            };
            let result = encode(&snapshot, limits, &flag, writer).and_then(|actual| {
                if actual == expected {
                    Ok(())
                } else {
                    Err(error("pinned stream metadata mismatch"))
                }
            });
            if let Err(e) = result {
                let _sent = tx.blocking_send(Err(e));
            }
            if let Some(mut checkpoint) = Arc::into_inner(snapshot) {
                while let Some(entity) = checkpoint.entities.pop() {
                    let _observed = observe(&flag, Stage::Cleanup, 8192);
                    drop(entity);
                    producer_stats.cleaned.fetch_add(1, Ordering::Relaxed);
                }
                while let Some(receipt) = checkpoint.dedup.pop() {
                    let _observed = observe(&flag, Stage::Cleanup, 8192);
                    drop(receipt);
                    producer_stats.cleaned.fetch_add(1, Ordering::Relaxed);
                }
            }
            producer_stats.stopped.store(true, Ordering::Release);
        });
        Ok(Self {
            limits,
            manifest,
            rx,
            cancelled,
            task: Some(task),
            stats,
        })
    }
    /// Shared high-water instrumentation, valid after producer completion.
    pub fn stats(&self) -> Arc<StreamStats> {
        Arc::clone(&self.stats)
    }
}
#[async_trait::async_trait]
impl CheckpointSource for EncodedCheckpoint {
    fn limits(&self) -> CheckpointLimits {
        self.limits
    }
    fn manifest(&self) -> &StreamManifest {
        &self.manifest
    }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
        match self.rx.recv().await {
            Some(Ok(bytes)) => {
                self.stats
                    .consumer
                    .fetch_max(bytes.len(), Ordering::Relaxed);
                Ok(Some(bytes))
            }
            Some(Err(e)) => {
                self.cancel().await;
                Err(e)
            }
            None => {
                if let Some(task) = self.task.as_mut() {
                    let result = task.await;
                    self.task.take();
                    result.map_err(error)?;
                }
                if self.cancelled.load(Ordering::Acquire) {
                    return Err(error("checkpoint cancelled"));
                }
                Ok(None)
            }
        }
    }
    async fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.rx.close();
        while self.rx.try_recv().is_ok() {}
        // A decoder select may drop this wait; keep ownership for its retry.
        if let Some(task) = self.task.as_mut() {
            let _joined = task.await;
            self.task.take();
        }
    }
}
impl Drop for EncodedCheckpoint {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.rx.close();
    }
}

mod decode_worker;
pub use decode_worker::{DecodeJob, decode};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
fn generation_bytes(cp: &Checkpoint) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(
        cp,
        CheckpointLimits::new(16384, 67_108_864).unwrap(),
        &AtomicBool::new(false),
        &mut bytes,
    )
    .unwrap();
    bytes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    include!("../../../../tests/checkpoint_resource_allocator.rs");

    #[test]
    fn resource_real_producer_maximum_identity_capacity() {
        let mut cp = fixture();
        for _ in 0..ENTITIES {
            cp.entities.push(entity(&cp, Vec::new()));
        }
        let (peak, stack) = tracking::measure(|| {
            cp.stream_manifest(limits(), &AtomicBool::new(false))
                .unwrap();
        });
        println!(
            "real producer 65536 requested transient peak={peak}, allocator-stack sample={stack}"
        );
        assert!(peak < 300_000);
        cp.entities[ENTITIES - 1] = cp.entities[0].clone();
        assert!(
            cp.stream_manifest(limits(), &AtomicBool::new(false))
                .is_err()
        );
    }

    #[tokio::test]
    async fn resource_eof_duplicate_and_interrupted_cleanup() {
        let mut cp = fixture();
        for _ in 0..64 {
            cp.entities.push(entity(&cp, vec![1; 16]));
        }
        let bytes = generation_bytes(&cp);
        let mut source = Bytes::new(bytes.clone());
        source.manifest.digest[0] ^= 1;
        let stats = Arc::new(StreamStats::default());
        assert!(
            decode(
                source,
                limits(),
                Arc::new(AtomicBool::new(false)),
                stats.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(
            stats.cleaned_records(),
            64,
            "EOF mismatch retains cleanup owner"
        );

        let record = String::from_utf8(generation_bytes(&Checkpoint::new(
            cp.instance_id,
            cp.revision,
            vec![cp.entities[0].clone()],
            cp.timestamp,
        )))
        .unwrap();
        let record = record
            .split("\"entities\":[")
            .nth(1)
            .unwrap()
            .split("],\"dedup\":")
            .next()
            .unwrap();
        let text = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("],\"dedup\":", &format!(",{record}],\"dedup\":"));
        let stats = Arc::new(StreamStats::default());
        assert!(
            decode(
                Bytes::new(text.into_bytes()),
                limits(),
                Arc::new(AtomicBool::new(false)),
                stats.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(
            stats.cleaned_records(),
            65,
            "duplicate rejection drains all unpublished entities"
        );

        let stats = Arc::new(StreamStats::default());
        let mut job = DecodeJob::start(
            Bytes::new(bytes),
            limits(),
            Arc::new(AtomicBool::new(false)),
            stats.clone(),
        );
        while !stats.stopped() {
            tokio::task::yield_now().await;
        }
        // Poll exactly once: cleanup removes a record then yields. Dropping
        // this wait must leave the remaining verified output in the job.
        let mut cancel = Box::pin(job.cancel());
        std::future::poll_fn(|cx| {
            assert!(cancel.as_mut().poll(cx).is_pending());
            if stats.cleaned_records() > 0 {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
        drop(cancel);
        assert!(stats.cleaned_records() > 0 && stats.cleaned_records() < 64);
        job.cancel().await;
        assert_eq!(stats.cleaned_records(), 64);
        let (peaks, calls) = orbisync_application::checkpoint_record::canonical::work::evidence();
        println!(
            "runtime stage reservations parser/emit/copy/hash/domain/index/cleanup: {peaks:?}; observations={calls:?}"
        );
        assert!(peaks.iter().all(|&n| n > 0 && n <= 16384));
    }

    fn limits() -> CheckpointLimits {
        CheckpointLimits::new(16_384, 64 * 1024 * 1024).unwrap()
    }
    fn fixture() -> Checkpoint {
        Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(i64::MAX as u64),
            Vec::new(),
            Timestamp::from_unix_millis(1000).unwrap(),
        )
    }
    fn entity(cp: &Checkpoint, payload: Vec<u8>) -> Entity {
        Entity::from_persisted(
            EntityId::generate(),
            cp.instance_id,
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::custom("x").unwrap(),
            Revision::from_u64(1),
            cp.timestamp,
            cp.timestamp,
            payload
                .chunks(4096)
                .enumerate()
                .map(|(i, v)| (format!("test.binary{i}"), v.to_vec()))
                .collect(),
        )
        .unwrap()
    }
    // Exact size construction stores domain fixtures only, not a JSON payload.
    fn sized(target: usize) -> Checkpoint {
        let mut cp = fixture();
        let block = vec![255; 8 * 4096];
        let record = entity(&cp, block.clone());
        let record_len = count_record(
            &borrowed_entity(&record, &AtomicBool::new(false)).unwrap(),
            ENTITY,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut size = cp
            .stream_manifest(limits(), &AtomicBool::new(false))
            .unwrap()
            .serialized_bytes as usize;
        while target - size > record_len + 1024 {
            size += record_len + usize::from(!cp.entities.is_empty());
            cp.entities.push(entity(&cp, block.clone()));
        }
        let separator = usize::from(!cp.entities.is_empty());
        let mut low: usize = 0;
        let mut high = 16 * 4096;
        while low < high {
            let mid = (low + high).div_ceil(2);
            let candidate = entity(&cp, vec![255; mid]);
            let n = count_record(
                &borrowed_entity(&candidate, &AtomicBool::new(false)).unwrap(),
                ENTITY,
                &AtomicBool::new(false),
            )
            .unwrap();
            if size + separator + n <= target {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let candidate = entity(&cp, vec![255; low]);
        let n = count_record(
            &borrowed_entity(&candidate, &AtomicBool::new(false)).unwrap(),
            ENTITY,
            &AtomicBool::new(false),
        )
        .unwrap();
        let tag = "x".repeat(target - size - separator - n + 1);
        let tail = Entity::from_persisted(
            candidate.id(),
            cp.instance_id,
            EntityKind::Object,
            None,
            None,
            VisibilityPolicy::custom(tag).unwrap(),
            Revision::from_u64(1),
            cp.timestamp,
            cp.timestamp,
            candidate.components().clone(),
        )
        .unwrap();
        cp.entities.push(tail);
        cp
    }
    async fn roundtrip(cp: Checkpoint, expected: usize) {
        let cp = Arc::new(cp);
        let manifest = cp
            .stream_manifest(limits(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(manifest.serialized_bytes, expected as u64);
        let source = EncodedCheckpoint::new(Arc::clone(&cp), limits(), manifest).unwrap();
        let producer = source.stats();
        let reader = Arc::new(StreamStats::default());
        // Reader C differs; selected manifest governs chunk lengths.
        let restored = decode(
            source,
            CheckpointLimits::new(1_048_576, 67_108_864).unwrap(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&reader),
        )
        .await
        .unwrap();
        assert_eq!(*cp, restored);
        assert!(producer.stopped());
        assert!(producer.peaks()[..3].iter().all(|v| *v <= 16_384));
        assert!(reader.peaks()[2] <= 16_384);
        assert!(reader.peaks()[3] <= RECEIPT);
    }
    #[tokio::test]
    async fn exact_chunk_boundaries_and_legacy_equivalence() {
        for size in [16_383, 16_384, 16_385] {
            let cp = sized(size);
            let legacy = cp.to_json_bytes().unwrap();
            let manifest = cp
                .stream_manifest(limits(), &AtomicBool::new(false))
                .unwrap();
            assert_eq!(
                manifest.digest,
                <[u8; 32]>::from(Sha256::digest(generation_bytes(&cp)))
            );
            assert_eq!(Checkpoint::from_json_bytes(&legacy).unwrap(), cp);
            roundtrip(cp, size).await;
        }
    }
    #[tokio::test]
    async fn old_generation_profile_is_rejected_while_legacy_reader_is_unchanged() {
        let cp = fixture();
        let legacy = cp.to_json_bytes().unwrap();
        assert_eq!(Checkpoint::from_json_bytes(&legacy).unwrap(), cp);
        rejects(Bytes::new(legacy), "reconcile/export").await;
        assert!(Checkpoint::from_json_bytes(&generation_bytes(&cp)).is_err());
    }
    #[tokio::test]
    async fn shared_codec5_golden_restores_and_reemits_exact_bytes() {
        let entity = include_str!("../../../../test-vectors/checkpoint-codec5-entity.json");
        let data = format!("{{\"format_version\":5,\"instance_id\":\"01900000-0000-7000-8000-000000000002\",\"revision\":1,\"timestamp\":\"2024-01-01T00:00:00Z\",\"entities\":[{entity}],\"dedup\":[]}}").into_bytes();
        let restored = decode(
            Bytes::new(data.clone()),
            limits(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(StreamStats::default()),
        )
        .await
        .unwrap();
        assert_eq!(generation_bytes(&restored), data);
        assert_eq!(
            restored.entities[0].components()["test.bytes"],
            [0, 255, 123, 34]
        );
    }
    #[tokio::test]
    async fn supported_large_sizes_once() {
        for size in [8 * 1024 * 1024 + 1, 16 * 1024 * 1024 + 1, 64 * 1024 * 1024] {
            roundtrip(sized(size), size).await;
        }
        let cp = sized(64 * 1024 * 1024 + 1);
        assert!(
            cp.stream_manifest(limits(), &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("total limit")
        );
    }
    #[tokio::test]
    async fn backpressure_cancel_and_lowered_total_preserve_selection() {
        let cp = Arc::new(sized(2 * 1024 * 1024 + 1));
        let manifest = cp
            .stream_manifest(limits(), &AtomicBool::new(false))
            .unwrap();
        let legacy = cp.to_json_bytes().unwrap();
        assert!(
            Checkpoint::from_legacy_json_with_limits(
                &legacy,
                CheckpointLimits::new(16_384, 2 * 1024 * 1024).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            Checkpoint::from_legacy_json_with_limits(&legacy, limits()).unwrap(),
            *cp
        );
        let mut source =
            EncodedCheckpoint::new(Arc::clone(&cp), limits(), manifest.clone()).unwrap();
        let stats = source.stats();
        assert!(source.next_chunk().await.unwrap().is_some());
        source.cancel().await;
        assert!(stats.stopped());
        assert!(source.next_chunk().await.is_err());
        let source = EncodedCheckpoint::new(Arc::clone(&cp), limits(), manifest.clone()).unwrap();
        let stats = source.stats();
        assert_eq!(source.manifest(), &manifest);
        assert!(
            decode(
                source,
                CheckpointLimits::new(16_384, 2 * 1024 * 1024).unwrap(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(StreamStats::default())
            )
            .await
            .is_err()
        );
        assert!(stats.stopped());
        roundtrip((*cp).clone(), manifest.serialized_bytes as usize).await;
    }
    struct Bytes {
        data: Vec<u8>,
        offset: usize,
        manifest: StreamManifest,
        cancelled: Arc<AtomicBool>,
    }
    impl Bytes {
        fn new(data: Vec<u8>) -> Self {
            Self {
                manifest: StreamManifest {
                    serialized_bytes: data.len() as u64,
                    chunk_bytes: 16_384,
                    chunk_count: (data.len() as u64).div_ceil(16_384),
                    digest: Sha256::digest(&data).into(),
                },
                data,
                offset: 0,
                cancelled: Arc::new(AtomicBool::new(false)),
            }
        }
    }
    #[async_trait::async_trait]
    impl CheckpointSource for Bytes {
        fn limits(&self) -> CheckpointLimits {
            limits()
        }
        fn manifest(&self) -> &StreamManifest {
            &self.manifest
        }
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
            if self.offset == self.data.len() {
                return Ok(None);
            }
            let end = (self.offset + 16_384).min(self.data.len());
            let bytes = self.data[self.offset..end].to_vec();
            self.offset = end;
            Ok(Some(bytes))
        }
        async fn cancel(&mut self) {
            self.cancelled.store(true, Ordering::Release);
        }
    }
    async fn rejects(source: Bytes, contains: &str) {
        let stopped = Arc::clone(&source.cancelled);
        let err = decode(
            source,
            limits(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(StreamStats::default()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(contains), "{err}");
        assert!(stopped.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn malformed_truncated_integrity_depth_token_record_and_numeric_bounds() {
        let cp = sized(16_385);
        let bytes = generation_bytes(&cp);
        let mut source = Bytes::new(bytes.clone());
        source.data.pop();
        rejects(source, "EOF").await;
        let mut source = Bytes::new(bytes.clone());
        source.manifest.digest[0] ^= 1;
        rejects(source, "integrity").await;
        let mut malformed = bytes.clone();
        malformed[0] = b'[';
        rejects(Bytes::new(malformed), "canonical").await;
        let text = String::from_utf8(bytes.clone()).unwrap();
        let excessive_revision = text.replacen(&i64::MAX.to_string(), &u64::MAX.to_string(), 1);
        rejects(Bytes::new(excessive_revision.into_bytes()), "BIGINT").await;
        let prefix = text.split("\"entities\":[").next().unwrap().to_owned() + "\"entities\":[";
        rejects(
            Bytes::new(
                format!(
                    "{prefix}{{\"unknown\":{}0{} }}]}}",
                    "[".repeat(32),
                    "]".repeat(32)
                )
                .into_bytes(),
            ),
            "canonical",
        )
        .await;
        // Large token in receipt has a 2 MiB record budget, so token wins.
        let empty = generation_bytes(&fixture());
        let start = String::from_utf8(empty).unwrap().replace(
            "\"dedup\":[]",
            &format!("\"dedup\":[{{\"unknown\":\"{}\"}}]", "a".repeat(ENTITY)),
        );
        rejects(Bytes::new(start.into_bytes()), "canonical").await;
        rejects(
            Bytes::new(
                format!("{prefix}{{\"unknown\":[{}0]}}]}}", "0,".repeat(ENTITY / 2)).into_bytes(),
            ),
            "canonical",
        )
        .await;
        let source = Bytes::new(bytes);
        let stopped = Arc::clone(&source.cancelled);
        assert!(
            decode(
                source,
                limits(),
                Arc::new(AtomicBool::new(true)),
                Arc::new(StreamStats::default())
            )
            .await
            .is_err()
        );
        assert!(stopped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn receipts_escape_binary_and_logical_constraints() {
        let mut cp = fixture();
        let id = CommandId::generate().to_string();
        cp.dedup.push(CheckpointDedupEntry {
            command_id: id.clone(),
            message_id: id,
            fingerprint: vec![255; 32],
            created_at_millis: 1000,
            expires_at_millis: 1000 + MAX_DEDUP_TTL_MILLIS,
            result: CheckpointDedupResult::Rejected {
                code: "CAPACITY".into(),
                detail: "\"\\\n\t\u{0000}日本".repeat(500),
            },
        });
        let id = CommandId::generate().to_string();
        cp.dedup.push(CheckpointDedupEntry {
            command_id: id.clone(),
            message_id: id,
            fingerprint: vec![0; 32],
            created_at_millis: 1000,
            expires_at_millis: 1000 + MAX_DEDUP_TTL_MILLIS,
            result: CheckpointDedupResult::Applied {
                response_payload: vec![255; MAX_DEDUP_RESPONSE_BYTES],
            },
        });
        let bytes = generation_bytes(&cp);
        let manifest = cp
            .stream_manifest(limits(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(manifest.digest, <[u8; 32]>::from(Sha256::digest(&bytes)));
        roundtrip(cp.clone(), bytes.len()).await;
        cp.dedup.push(cp.dedup[0].clone());
        assert!(
            cp.stream_manifest(limits(), &AtomicBool::new(false))
                .is_err()
        );
        cp.dedup.pop();
        cp.dedup[1].command_id = "bad-id".into();
        let malformed = serde_json::to_vec(&StoredCheckpoint {
            format_version: 5,
            instance_id: cp.instance_id.to_string(),
            revision: cp.revision.as_u64(),
            timestamp: cp.timestamp.to_rfc3339().unwrap(),
            entities: Vec::new(),
            dedup: cp.dedup.iter().map(stored_dedup_entry).collect(),
        })
        .unwrap();
        rejects(Bytes::new(malformed), "canonical").await;
    }

    #[tokio::test]
    async fn maximum_chunk_single_receipt_hash_work_is_bounded() {
        let mut cp = fixture();
        let id = CommandId::generate().to_string();
        cp.dedup.push(CheckpointDedupEntry {
            command_id: id.clone(),
            message_id: id,
            fingerprint: vec![255; 32],
            created_at_millis: 1000,
            expires_at_millis: 1000 + MAX_DEDUP_TTL_MILLIS,
            result: CheckpointDedupResult::Applied {
                response_payload: vec![255; MAX_DEDUP_RESPONSE_BYTES],
            },
        });
        let limits = CheckpointLimits::new(1_048_576, 2_097_152).unwrap();
        let manifest = cp.stream_manifest(limits, &AtomicBool::new(false)).unwrap();
        assert!(manifest.serialized_bytes > manifest.chunk_bytes);
        let total = manifest.serialized_bytes as usize;
        let cp = Arc::new(cp);
        let source = EncodedCheckpoint::new(Arc::clone(&cp), limits, manifest).unwrap();
        let stats = Arc::new(StreamStats::default());
        let restored = decode(
            source,
            limits,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&stats),
        )
        .await
        .unwrap();
        assert_eq!(restored, *cp);
        assert_eq!(stats.hash_work(), [32, total]);
        assert_eq!(stats.peaks()[2], 1_048_576);
        assert!(stats.stopped());
    }

    #[tokio::test]
    async fn cancellation_during_fetch_stops_before_hash_and_joins_source() {
        struct CancelOnFetch {
            source: Bytes,
            flag: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl CheckpointSource for CancelOnFetch {
            fn limits(&self) -> CheckpointLimits {
                self.source.limits()
            }
            fn manifest(&self) -> &StreamManifest {
                self.source.manifest()
            }
            async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
                let result = self.source.next_chunk().await;
                self.flag.store(true, Ordering::Release);
                result
            }
            async fn cancel(&mut self) {
                self.source.cancel().await;
            }
        }
        let source = Bytes::new(generation_bytes(&fixture()));
        let stopped = Arc::clone(&source.cancelled);
        let flag = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StreamStats::default());
        let mut job = DecodeJob::start(
            CancelOnFetch {
                source,
                flag: Arc::clone(&flag),
            },
            limits(),
            flag,
            Arc::clone(&stats),
        );
        assert!(
            job.finish()
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert_eq!(stats.hash_work(), [0, 0]);
        assert!(stats.stopped());
        assert!(stopped.load(Ordering::Acquire));
        job.cancel().await;
    }

    #[tokio::test]
    async fn decoder_cancel_joins_pending_cursor_before_returning() {
        struct Pending {
            manifest: StreamManifest,
            stopped: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl CheckpointSource for Pending {
            fn limits(&self) -> CheckpointLimits {
                limits()
            }
            fn manifest(&self) -> &StreamManifest {
                &self.manifest
            }
            async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
                std::future::pending().await
            }
            async fn cancel(&mut self) {
                self.stopped.store(true, Ordering::Release);
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StreamStats::default());
        let mut job = DecodeJob::start(
            Pending {
                manifest: fixture()
                    .stream_manifest(limits(), &AtomicBool::new(false))
                    .unwrap(),
                stopped: Arc::clone(&stopped),
            },
            limits(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&stats),
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), job.finish())
                .await
                .is_err()
        );
        job.cancel().await;
        assert!(stopped.load(Ordering::Acquire));
        assert!(stats.stopped());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod independent_review {
    use super::*;

    use std::time::Duration;

    // Park producer completion to make the scheduling window deterministic.
    // All cancellation and decoder code below is the actual production code.
    #[tokio::test]
    async fn cancellation_during_source_error_must_join_producer() {
        let limits = CheckpointLimits::default();
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(1),
            Vec::new(),
            Timestamp::from_unix_millis(1000).unwrap(),
        );
        let manifest = cp.stream_manifest(limits, &AtomicBool::new(false)).unwrap();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(Err(error("injected producer error"))).unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let task = {
            let release = release.clone();
            let stopped = stopped.clone();
            tokio::spawn(async move {
                release.notified().await;
                stopped.store(true, Ordering::Release);
            })
        };
        let source = EncodedCheckpoint {
            limits,
            manifest,
            rx,
            cancelled: Arc::new(AtomicBool::new(false)),
            task: Some(task),
            stats: Arc::new(StreamStats::default()),
        };
        let mut job = DecodeJob::start(
            source,
            limits,
            Arc::new(AtomicBool::new(false)),
            Arc::new(StreamStats::default()),
        );
        // Source error has entered cancel(): receiver is closed and join pending.
        tokio::time::timeout(Duration::from_secs(2), tx.closed())
            .await
            .unwrap();
        let returned_before_producer =
            tokio::time::timeout(Duration::from_millis(150), job.cancel())
                .await
                .is_ok();
        let stopped_before_release = stopped.load(Ordering::Acquire);
        let repeated_returned_before_producer =
            tokio::time::timeout(Duration::from_millis(50), job.cancel())
                .await
                .is_ok();
        // Release injected worker even if the regression assertion fails.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !stopped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Join the decoder too, including when a cancellation wait was dropped.
        tokio::time::timeout(Duration::from_secs(2), job.cancel())
            .await
            .unwrap();
        job.cancel().await;
        println!(
            "cancel_returned_before_release={returned_before_producer}, producer_stopped_before_release={stopped_before_release}"
        );
        assert!(
            !returned_before_producer || stopped_before_release,
            "DecodeJob::cancel returned while the encoded producer was still alive"
        );
        assert!(
            !repeated_returned_before_producer || stopped_before_release,
            "repeated DecodeJob::cancel lost the pending decoder join"
        );
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod independent_integrity_review {
    use super::*;

    struct Source {
        manifest: StreamManifest,
        chunks: std::collections::VecDeque<Vec<u8>>,
        late_error: bool,
        cancelled: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl CheckpointSource for Source {
        fn limits(&self) -> CheckpointLimits {
            CheckpointLimits::default()
        }
        fn manifest(&self) -> &StreamManifest {
            &self.manifest
        }
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
            if let Some(bytes) = self.chunks.pop_front() {
                return Ok(Some(bytes));
            }
            if self.late_error {
                return Err(error("late producer failure"));
            }
            Ok(None)
        }
        async fn cancel(&mut self) {
            self.cancelled.store(true, Ordering::Release);
        }
    }
    #[tokio::test]
    async fn actual_coverage_and_post_payload_errors_fail_closed() {
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(1),
            Vec::new(),
            Timestamp::from_unix_millis(1000).unwrap(),
        );
        let bytes = generation_bytes(&cp);
        for case in 0..7 {
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut source = Source {
                manifest: cp
                    .stream_manifest(CheckpointLimits::default(), &AtomicBool::new(false))
                    .unwrap(),
                chunks: [bytes.clone()].into(),
                late_error: false,
                cancelled: cancelled.clone(),
            };
            match case {
                0 => {
                    source.chunks.clear();
                }
                1 => {
                    source.chunks.push_back(vec![b' ']);
                }
                2 => {
                    source.chunks[0].push(b' ');
                }
                3 => {
                    source.manifest.serialized_bytes += 1;
                }
                4 => {
                    source.manifest.chunk_count += 1;
                }
                5 => {
                    source.manifest.digest[0] ^= 1;
                }
                6 => {
                    source.late_error = true;
                }
                _ => unreachable!(),
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                decode(
                    source,
                    CheckpointLimits::default(),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(StreamStats::default()),
                ),
            )
            .await
            .unwrap();
            assert!(result.is_err(), "case {case} published invalid state");
            assert!(
                cancelled.load(Ordering::Acquire),
                "case {case} did not clean up source"
            );
            println!("integrity case {case}: rejected and source cancelled");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod cancellation_ownership_tests {
    use super::*;

    use std::{future::Future, task::Poll, time::Duration};

    // Poll exactly once and drop the pending wait, without scheduling assumptions.
    async fn interrupt(wait: impl Future) -> bool {
        let mut wait = std::pin::pin!(wait);
        std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx).is_pending())).await
    }

    #[tokio::test]
    async fn interrupted_eof_and_cancel_retain_producer_join() {
        let limits = CheckpointLimits::default();
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(1),
            Vec::new(),
            Timestamp::from_unix_millis(1000).unwrap(),
        );
        let manifest = cp.stream_manifest(limits, &AtomicBool::new(false)).unwrap();
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let release = Arc::new(tokio::sync::Notify::new());
        let producer_release = Arc::clone(&release);
        let task = tokio::spawn(async move {
            producer_release.notified().await;
        });
        let mut source = EncodedCheckpoint {
            limits,
            manifest,
            rx,
            cancelled: Arc::new(AtomicBool::new(false)),
            task: Some(task),
            stats: Arc::new(StreamStats::default()),
        };
        let eof_pending = interrupt(source.next_chunk()).await;
        let eof_retained = source.task.is_some();
        let cancel_pending = interrupt(source.cancel()).await;
        let cancel_retained = source.task.is_some();
        let repeated_pending = interrupt(source.cancel()).await;
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), source.cancel())
            .await
            .unwrap();
        source.cancel().await;
        assert!(eof_pending && cancel_pending && repeated_pending);
        assert!(eof_retained && cancel_retained);
        assert!(source.task.is_none());
        assert!(source.next_chunk().await.is_err());
    }

    #[tokio::test]
    async fn drop_closes_full_queue_and_signals_producer() {
        let limits = CheckpointLimits::default();
        let cp = Checkpoint::new(
            InstanceId::generate(),
            Revision::from_u64(1),
            Vec::new(),
            Timestamp::from_unix_millis(1000).unwrap(),
        );
        let manifest = cp.stream_manifest(limits, &AtomicBool::new(false)).unwrap();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(Ok(vec![0])).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let task = tokio::task::spawn_blocking(move || {
            let closed = tx.blocking_send(Ok(vec![1])).is_err();
            (closed, flag.load(Ordering::Acquire))
        });
        // Retain the test producer join externally: Drop only signals cleanup,
        // and production permit owners must explicitly await cancel instead.
        let source = EncodedCheckpoint {
            limits,
            manifest,
            rx,
            cancelled,
            task: None,
            stats: Arc::new(StreamStats::default()),
        };
        drop(source);
        let (closed, cancelled) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(closed && cancelled);
    }
}
