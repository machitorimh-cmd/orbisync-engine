use super::*;
use orbisync_application::checkpoint_record::canonical::work::{Stage, observe};
use orbisync_application::checkpoint_record::canonical::{
    VERSION, WORK_QUANTUM,
    reader::{Progress, Record, RecordDecoder, RecordKind},
};

// Error/cancellation cleanup visits each retained record separately. u8 buffer
// deallocation does not scan its contents; allocator latency is not a CPU-work
// deadline. The existing DecodeJob still owns and joins the whole cleanup.
pub(super) struct Restoring {
    instance_id: InstanceId,
    revision: Revision,
    timestamp: Timestamp,
    entities: Vec<Entity>,
    dedup: Vec<CheckpointDedupEntry>,
    cancelled: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
    slots: usize,
    heap: usize,
}
// Both mandatory closed records exceed 128 bytes even with empty payloads:
// the two 36-byte UUID values plus required field names/framing alone do so.
// This conservative lower bound only tightens reservation, never acceptance.
const MIN_RECORD_BYTES: usize = 128;

fn entity_heap(entity: &Entity) -> usize {
    let components = entity.components();
    // HashMap capacity -> upper bucket count, 48-byte key/value slots plus
    // controls/alignment rounded conservatively to 64 bytes per bucket.
    let table = if components.capacity() == 0 {
        0
    } else {
        components.capacity().next_power_of_two() * 64
    };
    table
        + components
            .iter()
            .map(|(key, value)| key.capacity() + value.capacity())
            .sum::<usize>()
        + match entity.visibility() {
            VisibilityPolicy::RoleRestricted { roles } => 48 + roles.len() * 256,
            VisibilityPolicy::Explicit { users } => 48 + users.len() * 256,
            VisibilityPolicy::Custom { tag } => 32 + tag.as_str().len(),
            _ => 0,
        }
}
fn receipt_heap(entry: &CheckpointDedupEntry) -> usize {
    entry.command_id.capacity()
        + entry.message_id.capacity()
        + entry.fingerprint.capacity()
        + match &entry.result {
            CheckpointDedupResult::Applied { response_payload } => response_payload.capacity(),
            CheckpointDedupResult::Rejected { code, detail } => code.capacity() + detail.capacity(),
        }
}
impl Restoring {
    fn charge(&mut self, slots: usize, heap: usize) {
        let added = slots + heap;
        self.slots += slots;
        self.heap += heap;
        let slots = self
            .stats
            .assembly_slots
            .fetch_add(slots, Ordering::Relaxed)
            + slots;
        let heap = self.stats.assembly_heap.fetch_add(heap, Ordering::Relaxed) + heap;
        self.stats
            .assembly_slots_peak
            .fetch_max(slots, Ordering::Relaxed);
        self.stats
            .assembly_heap_peak
            .fetch_max(heap, Ordering::Relaxed);
        let live = self.stats.assembly_live.fetch_add(added, Ordering::Relaxed) + added;
        self.stats.assembly_peak.fetch_max(live, Ordering::Relaxed);
    }
    fn uncharge_heap(&mut self, heap: usize) {
        self.heap -= heap;
        self.stats.assembly_heap.fetch_sub(heap, Ordering::Relaxed);
        self.stats.assembly_live.fetch_sub(heap, Ordering::Relaxed);
    }
    pub(super) fn release(mut self) -> Checkpoint {
        // All charge remains live until the verified vectors change owner.
        let checkpoint = Checkpoint {
            instance_id: self.instance_id,
            revision: self.revision,
            timestamp: self.timestamp,
            entities: std::mem::take(&mut self.entities),
            dedup: std::mem::take(&mut self.dedup),
        };
        self.stats
            .assembly_slots
            .fetch_sub(self.slots, Ordering::Relaxed);
        self.stats
            .assembly_heap
            .fetch_sub(self.heap, Ordering::Relaxed);
        self.stats
            .assembly_live
            .fetch_sub(self.slots + self.heap, Ordering::Relaxed);
        self.slots = 0;
        self.heap = 0;
        checkpoint
    }
    pub(super) fn cleanup_step(&mut self) -> bool {
        let _observed = observe(&self.cancelled, Stage::Cleanup, 8192);
        if let Some(entity) = self.entities.pop() {
            let heap = entity_heap(&entity);
            drop(entity);
            self.uncharge_heap(heap);
            self.stats.cleaned.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if let Some(receipt) = self.dedup.pop() {
            let heap = receipt_heap(&receipt);
            drop(receipt);
            self.uncharge_heap(heap);
            self.stats.cleaned.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}
impl Drop for Restoring {
    fn drop(&mut self) {
        while !self.cleanup_step() {}
        // Record cleanup alone does not release Vec backing capacity.
        self.entities = Vec::new();
        self.dedup = Vec::new();
        self.stats
            .assembly_slots
            .fetch_sub(self.slots, Ordering::Relaxed);
        self.stats
            .assembly_live
            .fetch_sub(self.slots, Ordering::Relaxed);
        self.stats.assemblies.fetch_sub(1, Ordering::Relaxed);
    }
}

fn byte(input: &mut impl Read) -> Result<u8, ApplicationError> {
    let mut byte = [0];
    input
        .read_exact(&mut byte)
        .map_err(|e| error(format!("checkpoint EOF/read failure: {e}")))?;
    Ok(byte[0])
}
fn literal(input: &mut impl Read, expected: &[u8]) -> Result<(), ApplicationError> {
    for &expected in expected {
        if byte(input)? != expected {
            return Err(error("noncanonical generation framing"));
        }
    }
    Ok(())
}
fn record<S: CheckpointSource>(
    input: &mut Input<S>,
    kind: RecordKind,
) -> Result<Record, ApplicationError> {
    let mut decoder = RecordDecoder::new(kind);
    let mut next = Some(b'{');
    loop {
        let bytes = next.take().map(|b| [b]);
        let step = decoder
            .step(
                bytes.as_ref().map_or(&[], |b| b.as_slice()),
                WORK_QUANTUM - 4096,
                &input.cancelled,
            )
            .map_err(error)?;
        match step.progress {
            Progress::Ready { record, .. } => {
                let stats = decoder.stats();
                input.stats.canonical_scratch.fetch_max(
                    stats.scratch_capacity_bound + input.index_bytes,
                    Ordering::Relaxed,
                );
                input
                    .stats
                    .canonical_work
                    .fetch_max(stats.max_step_work, Ordering::Relaxed);
                return Ok(record);
            }
            Progress::Input => next = Some(byte(input)?),
            Progress::Yield => {}
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format_version: u32,
    instance_id: String,
    revision: u64,
    timestamp: String,
}
pub(super) fn parse<S: CheckpointSource>(
    input: &mut Input<S>,
) -> Result<Restoring, ApplicationError> {
    // Header is fixed-width metadata, not a record. Bound it before serde.
    const MARKER: &[u8] = b",\"entities\":[";
    let mut prefix = Vec::with_capacity(256);
    loop {
        if prefix.len() == 256 {
            return Err(error("generation metadata limit"));
        }
        prefix.push(byte(input)?);
        if prefix.ends_with(MARKER) {
            break;
        }
    }
    prefix.truncate(prefix.len() - MARKER.len());
    prefix.push(b'}');
    let header: Header = serde_json::from_slice(&prefix)
        .map_err(|e| error(format!("noncanonical generation metadata: {e}")))?;
    if header.format_version != VERSION {
        return Err(error(
            "incompatible generation codec; preserve source and reconcile/export explicitly",
        ));
    }
    if header.revision > i64::MAX as u64 {
        return Err(error("generation revision exceeds BIGINT"));
    }
    if serde_json::to_vec(&header).map_err(error)? != prefix {
        return Err(error("noncanonical generation metadata"));
    }
    let instance_id = InstanceId::parse(&header.instance_id).map_err(error)?;
    let timestamp = Timestamp::parse_rfc3339(&header.timestamp).map_err(error)?;
    if instance_id.to_string() != header.instance_id
        || timestamp.to_rfc3339().map_err(error)? != header.timestamp
    {
        return Err(error("noncanonical generation identity/time"));
    }
    let revision = Revision::from_u64(header.revision);
    let millis = timestamp.to_unix_millis().map_err(error)?;
    let mut restored = Restoring {
        instance_id,
        revision,
        timestamp,
        entities: Vec::new(),
        dedup: Vec::new(),
        cancelled: Arc::clone(&input.cancelled),
        stats: Arc::clone(&input.stats),
        slots: 0,
        heap: 0,
    };
    let active = input.stats.assemblies.fetch_add(1, Ordering::Relaxed) + 1;
    input
        .stats
        .assemblies_peak
        .fetch_max(active, Ordering::Relaxed);
    let mut seen =
        orbisync_application::checkpoint_record::canonical::index::PositionIndex::new(ENTITIES);
    input.index_bytes = seen.capacity_bytes();
    let mut next = byte(input)?;
    if next != b']' {
        loop {
            if next != b'{' || restored.entities.len() == ENTITIES {
                return Err(error("entity framing/count"));
            }
            let Record::Entity(entity) = record(input, RecordKind::Entity)? else {
                return Err(error("entity schema"));
            };
            observe(&input.cancelled, Stage::Domain, 4096).map_err(error)?;
            if entity.instance_id() != instance_id || entity.revision() > revision {
                return Err(error("entity identity/revision mismatch"));
            }
            observe(&input.cancelled, Stage::Index, 4096).map_err(error)?;

            if restored.entities.is_empty() {
                // Reserve the bounded retained-state capacity once, without
                // initializing or moving entity bytes inside a later step.
                restored.entities.reserve_exact(
                    ENTITIES.min(input.manifest.serialized_bytes as usize / MIN_RECORD_BYTES),
                );
                restored.charge(restored.entities.capacity() * size_of::<Entity>(), 0);
            }
            restored.charge(0, entity_heap(&entity));
            restored.entities.push(entity);
            seen.push((restored.entities.len() - 1) as u32, |position| {
                restored.entities[position as usize].id().as_uuid()
            })
            .map_err(error)?;
            next = byte(input)?;
            if next == b']' {
                break;
            }
            if next != b',' {
                return Err(error("entity separator"));
            }
            next = byte(input)?;
        }
    }
    loop {
        if input.cancelled.load(Ordering::Acquire) {
            return Err(error("checkpoint cancelled"));
        }
        observe(&input.cancelled, Stage::Index, 4096).map_err(error)?;
        if seen
            .drain_step(|position| restored.entities[position as usize].id().as_uuid())
            .map_err(error)?
        {
            break;
        }
    }
    drop(seen);
    input.index_bytes = 0;
    literal(input, b",\"dedup\":[")?;
    input.record_limit.store(RECEIPT, Ordering::Relaxed);

    let mut seen = orbisync_application::checkpoint_record::canonical::index::IdentityIndex::new(
        MAX_DEDUP_RECORDS,
    );
    input.index_bytes = seen.capacity_bytes();
    let mut logical = 0;
    next = byte(input)?;
    if next != b']' {
        loop {
            if next != b'{' || restored.dedup.len() == MAX_DEDUP_RECORDS {
                return Err(error("receipt framing/count"));
            }
            let Record::Receipt(receipt) = record(input, RecordKind::Receipt)? else {
                return Err(error("receipt schema"));
            };
            observe(&input.cancelled, Stage::Domain, 8192).map_err(error)?;
            let entry = restore_dedup_entry(
                StoredDedupEntry {
                    command_id: receipt.command_id,
                    fingerprint: receipt.fingerprint,
                    created_at_millis: receipt.created_at_millis,
                    expires_at_millis: receipt.expires_at_millis,
                    message_id: receipt.message_id,
                    result: receipt.result,
                },
                millis,
            )
            .map_err(error)?;
            let size = dedup_entry_size(&entry);
            logical += size;
            if size > MAX_DEDUP_RECORD_BYTES || logical > MAX_DEDUP_AGGREGATE_BYTES {
                return Err(error("receipt logical bound or duplicate"));
            }
            observe(&input.cancelled, Stage::Index, 4096).map_err(error)?;
            seen.push(
                CommandId::parse(&entry.command_id)
                    .map_err(error)?
                    .as_uuid(),
            )
            .map_err(error)?;
            if restored.dedup.is_empty() {
                restored.dedup.reserve_exact(
                    MAX_DEDUP_RECORDS
                        .min(input.manifest.serialized_bytes as usize / MIN_RECORD_BYTES),
                );
                restored.charge(
                    restored.dedup.capacity() * size_of::<CheckpointDedupEntry>(),
                    0,
                );
            }
            restored.charge(0, receipt_heap(&entry));
            restored.dedup.push(entry);
            next = byte(input)?;
            if next == b']' {
                break;
            }
            if next != b',' {
                return Err(error("receipt separator"));
            }
            next = byte(input)?;
        }
    }
    loop {
        if input.cancelled.load(Ordering::Acquire) {
            return Err(error("checkpoint cancelled"));
        }
        observe(&input.cancelled, Stage::Index, 4096).map_err(error)?;
        if seen.drain_step().map_err(error)? {
            break;
        }
    }
    drop(seen);
    literal(input, b"}")?;
    let mut trailing = [0];
    if input.read(&mut trailing).map_err(error)? != 0 {
        return Err(error("trailing generation bytes"));
    }
    // The cleanup owner must cover final coverage/digest verification too.
    if input.total != input.manifest.serialized_bytes
        || input.chunks != input.manifest.chunk_count
        || <[u8; 32]>::from(input.hash.clone().finalize()) != input.manifest.digest
        || input.cancelled.load(Ordering::Acquire)
    {
        return Err(error("checkpoint EOF/integrity mismatch"));
    }
    Ok(restored)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod resource_tests {
    use super::*;
    use crate::checkpoint::stream::tests::tracking;

    struct Source {
        data: Option<Vec<u8>>,
        manifest: StreamManifest,
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
            Ok(self.data.take())
        }
        async fn cancel(&mut self) {}
    }
    fn input(
        bytes: &[u8],
        runtime: &tokio::runtime::Runtime,
        stats: &Arc<StreamStats>,
        corrupt: bool,
    ) -> Input<Source> {
        let mut manifest = StreamManifest {
            serialized_bytes: bytes.len() as u64,
            chunk_bytes: 262144,
            chunk_count: 1,
            digest: Sha256::digest(bytes).into(),
        };
        if corrupt {
            manifest.digest[0] ^= 1;
        }
        Input {
            source: Source {
                data: Some(bytes.to_vec()),
                manifest: manifest.clone(),
            },
            runtime: runtime.handle().clone(),
            manifest,
            buffer: Vec::new(),
            offset: 0,
            total: 0,
            chunks: 0,
            hash: Sha256::new(),
            lexical: Lexical::default(),
            cancelled: Arc::new(AtomicBool::new(false)),
            stats: stats.clone(),
            record_limit: Arc::new(AtomicUsize::new(ENTITY)),
            record_bytes: 0,
            index_bytes: 0,
        }
    }
    #[test]
    fn r3_modest_assembly_overlap_transfer_and_failed_eof_cleanup() {
        println!(
            "R3 concrete control sizes: Input<Source>={} Restoring={} StreamStats={} RecordDecoder={} (native frames/runtime allocations separate)",
            size_of::<Input<Source>>(),
            size_of::<Restoring>(),
            size_of::<StreamStats>(),
            size_of::<RecordDecoder>()
        );
        let instance = InstanceId::generate();
        let timestamp = Timestamp::from_unix_millis(1000).unwrap();
        let entities = (0..40)
            .map(|_| {
                Entity::from_persisted(
                    EntityId::generate(),
                    instance,
                    EntityKind::Object,
                    None,
                    None,
                    VisibilityPolicy::Global,
                    Revision::from_u64(1),
                    timestamp,
                    timestamp,
                    [("test.payload".to_owned(), vec![255; 1024])].into(),
                )
                .unwrap()
            })
            .collect();
        let checkpoint = Checkpoint::new(instance, Revision::from_u64(1), entities, timestamp);
        let bytes = generation_bytes(&checkpoint);
        assert!(bytes.len() > 131072 && bytes.len() < 262144);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stats = Arc::new(StreamStats::default());
        let slots = (bytes.len() / MIN_RECORD_BYTES).min(ENTITIES) * size_of::<Entity>();
        let (peak, stack) = tracking::measure(|| {
            let first = parse(&mut input(&bytes, &runtime, &stats, false)).unwrap();
            let single = stats.assembly_resources();
            assert_eq!(single[0], slots);
            assert!(single[1] >= 40 * 1024);
            assert_eq!(single[3], single[0] + single[1]);
            let second = parse(&mut input(&bytes, &runtime, &stats, false)).unwrap();
            let both = stats.assembly_resources();
            assert_eq!(both[4], 2);
            assert_eq!(both[3], 2 * single[3]);
            drop(first);
            assert_eq!(stats.assembly_resources()[3], single[3]);
            let published = second.release();
            assert_eq!(published, checkpoint);
            assert_eq!(stats.assembly_resources()[3..5], [0, 0]);
        });
        println!(
            "R3 two assembly owners: encoded={} entity_size={} receipt_slot_size={} slot_capacity_each={} resources={:?} requested_peak={} native_sample={}",
            bytes.len(),
            size_of::<Entity>(),
            size_of::<CheckpointDedupEntry>(),
            slots,
            stats.assembly_resources(),
            peak,
            stack
        );
        let stats = Arc::new(StreamStats::default());
        let (peak, stack) = tracking::measure(|| {
            assert!(parse(&mut input(&bytes, &runtime, &stats, true)).is_err());
            assert_eq!(stats.cleaned_records(), 40);
            assert_eq!(stats.assembly_resources()[3..5], [0, 0]);
        });
        println!(
            "R3 failed EOF: resources={:?} requested_peak={} native_sample={}",
            stats.assembly_resources(),
            peak,
            stack
        );
        assert!(slots < 1_048_576, "avoid old 18,350,080-byte reservation");
    }
    #[test]
    fn resource_real_restore_owner_requested_overlap() {
        let golden = include_str!("../../../../../../test-vectors/checkpoint-codec5-entity.json");
        let bytes = format!("{{\"format_version\":5,\"instance_id\":\"01900000-0000-7000-8000-000000000002\",\"revision\":1,\"timestamp\":\"2024-01-01T00:00:00Z\",\"entities\":[{golden}],\"dedup\":[]}}").into_bytes();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stats = Arc::new(StreamStats::default());
        for corrupt in [false, true] {
            let (peak, stack) = tracking::measure(|| {
                let mut manifest = StreamManifest {
                    serialized_bytes: bytes.len() as u64,
                    chunk_bytes: 16384,
                    chunk_count: 1,
                    digest: Sha256::digest(&bytes).into(),
                };
                if corrupt {
                    manifest.digest[0] ^= 1;
                }
                let mut input = Input {
                    source: Source {
                        data: Some(bytes.clone()),
                        manifest: manifest.clone(),
                    },
                    runtime: runtime.handle().clone(),
                    manifest,
                    buffer: Vec::new(),
                    offset: 0,
                    total: 0,
                    chunks: 0,
                    hash: Sha256::new(),
                    lexical: Lexical::default(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    stats: stats.clone(),
                    record_limit: Arc::new(AtomicUsize::new(ENTITY)),
                    record_bytes: 0,
                    index_bytes: 0,
                };
                let result = parse(&mut input);
                assert_eq!(result.is_err(), corrupt);
                drop(result);
            });
            // This fixture includes retained Vec capacity and input as well;
            // nothing is subtracted to make this small-record owner pass.
            println!(
                "real restore corrupt={corrupt}: requested peak including retained state/input={peak}, allocator-stack sample={stack}, index+reader={:?}",
                stats.canonical_resources()
            );
            assert!(peak < 2_097_152);
            assert!(stats.canonical_resources()[0] > 262_144);
        }
    }
}
