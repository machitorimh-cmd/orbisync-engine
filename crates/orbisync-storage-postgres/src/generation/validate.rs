//! Closed codec-5 framing and shared record validation.
use super::*;
use orbisync_application::checkpoint_record::{
    ReceiptResult,
    canonical::{
        VERSION,
        index::IdentityIndex,
        reader::{Progress, Record, RecordDecoder, RecordKind},
        work::{Stage, observe},
    },
};
use orbisync_domain::Timestamp;
use std::sync::atomic::AtomicBool;

pub(super) struct Receipt {
    pub digest: [u8; 32],
    pub created: i64,
    pub expires: i64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod resource_tests {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/checkpoint_resource_allocator.rs"
    ));

    #[test]
    fn resource_sqlx_binding_boundary_is_not_cooperative() {
        // No connection/query/SQL execution: exercise only the pinned driver's
        // public BYTEA encoder to preserve the unresolved R3 boundary evidence.
        let bytes = vec![0u8; 65536];
        let before = orbisync_application::checkpoint_record::canonical::work::evidence();
        let mut buffer = sqlx::postgres::PgArgumentBuffer::default();
        let _encoded =
            <&[u8] as sqlx::Encode<Postgres>>::encode_by_ref(&bytes.as_slice(), &mut buffer)
                .unwrap();
        assert_eq!(buffer.len(), bytes.len());
        assert_eq!(
            before,
            orbisync_application::checkpoint_record::canonical::work::evidence()
        );
        println!(
            "R3 residual: SQLx BYTEA encode copied {} bytes synchronously with zero codec work observations (no DB used)",
            buffer.len()
        );
    }

    #[test]
    fn resource_real_validator_maximum_indices_and_constant_drop() {
        println!(
            "R3 concrete Validator control bytes={} (backing capacities and native stack separate)",
            size_of::<Validator>()
        );
        use orbisync_domain::{Entity, EntityId, EntityKind, Revision, UserId, VisibilityPolicy};
        let instance =
            InstanceId::new(Uuid::from_u128(0x01900000_0000_7000_8000_000000000001)).unwrap();
        let timestamp = Timestamp::from_unix_millis(1000).unwrap();
        let manifest = StreamManifest {
            serialized_bytes: 64 * 1024 * 1024,
            chunk_bytes: 16384,
            chunk_count: 4096,
            digest: [0; 32],
        };
        let attempt = GenerationAttempt::from_manifest(
            instance,
            0,
            orbisync_application::checkpoint_admission::WriterToken {
                epoch: 1,
                boot: Uuid::now_v7(),
            },
            1000,
            VERSION,
            CheckpointLimits::new(16384, 64 * 1024 * 1024).unwrap(),
            &manifest,
        )
        .unwrap();
        let (peak, sampled_stack) = tracking::measure(|| {
            // A producer/restorer's position index may coexist with this
            // adapter in one generation pipeline; include its full capacity.
            let _runtime_index =
                orbisync_application::checkpoint_record::canonical::index::PositionIndex::new(
                    65536,
                );
            let mut validator = Validator::new(&attempt);
            validator.revision = 1;
            for n in 0..65536u128 {
                let uuid = Uuid::from_u128(0x01900000_0000_7000_8000_000000000000 | n);
                let entity = Entity::from_persisted(
                    EntityId::new(uuid).unwrap(),
                    instance,
                    EntityKind::Object,
                    Some(UserId::new(uuid).unwrap()),
                    None,
                    VisibilityPolicy::Global,
                    Revision::from_u64(1),
                    timestamp,
                    timestamp,
                    Default::default(),
                )
                .unwrap();
                validator.accept(&Record::Entity(entity), [0; 32]).unwrap();
                // The real adapter checks and clears each small owner batch.
                assert!(validator.owners.len() <= 32);
                validator.owners.clear();
            }
            validator.phase = 1;
            validator.draining = true;
            // Interrupt a cooperative drain wait; the validator still owns
            // the packed array. No cleanup task/handle has been detached.
            let mut wait = Box::pin(validator.feed_async(&[]));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            drop(wait);
            assert!(validator.draining);
            let mut steps = 1;
            while validator.draining {
                validator.step(&[]).unwrap();
                steps += 1;
            }
            assert_eq!(steps, 65537);
            for n in 0..4096u128 {
                let id = Uuid::from_u128(0x01900000_0000_7000_8000_000000000000 | n).to_string();
                validator
                    .accept(
                        &Record::Receipt(
                            orbisync_application::checkpoint_record::canonical::reader::Receipt {
                                command_id: id.clone(),
                                message_id: id,
                                fingerprint: vec![0; 32],
                                created_at_millis: 1000,
                                expires_at_millis: 86_401_000,
                                result: ReceiptResult::Applied {
                                    response_payload: vec![1],
                                },
                            },
                        ),
                        [0; 32],
                    )
                    .unwrap();
            }
            // All three long-lived backing arrays contain no Drop elements.
            assert!(!std::mem::needs_drop::<Uuid>());
            assert!(!std::mem::needs_drop::<(Uuid, Receipt)>());
            drop(validator);
        });
        println!(
            "real validator max identities requested peak={peak}, allocator-stack sample={sampled_stack}"
        );
        // Leave room for the maximum receipt decoder and native stack separately.
        assert!(peak < 1_800_000);
    }
}
pub(super) struct Validator {
    attempt: GenerationAttempt,
    phase: u8,
    scratch: Vec<u8>,
    decoder: Option<RecordDecoder>,
    pending: Option<(Record, [u8; 32])>,
    cleanup: Option<Record>,
    separator: bool,
    after_comma: bool,
    pub revision: i64,
    pub state: Sha256,
    // Checked by the adapter after each <=512-byte feed, in the same tx.
    pub owners: Vec<Uuid>,
    identities: IdentityIndex,
    entity_count: usize,
    draining: bool,
    pub receipts: Vec<(Uuid, Receipt)>,
    logical: usize,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format_version: u32,
    instance_id: String,
    revision: i64,
    timestamp: String,
}
impl Validator {
    pub fn new(attempt: &GenerationAttempt) -> Self {
        Self {
            attempt: attempt.clone(),
            phase: 0,
            scratch: Vec::with_capacity(256),
            decoder: None,
            pending: None,
            cleanup: None,
            separator: false,
            after_comma: false,
            revision: 0,
            state: Sha256::new(),
            owners: Vec::with_capacity(32),
            identities: IdentityIndex::new(65536),
            entity_count: 0,
            draining: false,
            receipts: Vec::with_capacity(4096),
            logical: 0,
        }
    }
    fn step(&mut self, input: &[u8]) -> Result<usize, ApplicationError> {
        if let Some(record) = self.cleanup.take() {
            let _observed = observe(&AtomicBool::new(false), Stage::Cleanup, 8192);
            drop(record);
            return Ok(0);
        }
        if let Some((record, digest)) = self.pending.take() {
            observe(&AtomicBool::new(false), Stage::Domain, 16384).map_err(|_| invalid())?;
            let result = self.accept(&record, digest);
            self.cleanup = Some(record);
            result?;
            self.separator = true;
            return Ok(0);
        }
        if self.draining {
            observe(&AtomicBool::new(false), Stage::Index, 4096).map_err(|_| invalid())?;
            if self.identities.drain_step().map_err(|_| invalid())? {
                // Release before reserving the next phase: assignment alone
                // would temporarily retain both allocations.
                self.identities = IdentityIndex::default();
                self.identities = IdentityIndex::new(if self.phase == 1 { 4096 } else { 0 });
                self.draining = false;
                self.phase += 1;
                self.separator = false;
            }
            return Ok(0);
        }
        if self.attempt.codec != VERSION {
            return Err(ApplicationError::port_failure(
                "incompatible generation codec; preserve source and reconcile/export explicitly",
            ));
        }
        if let Some(decoder) = self.decoder.as_mut() {
            let step = decoder
                .step(input, 8192, &AtomicBool::new(false))
                .map_err(|_| invalid())?;
            if let Progress::Ready { record, digest } = step.progress {
                self.pending = Some((record, digest));
                self.decoder = None;
            }
            return Ok(step.consumed);
        }
        let Some(&byte) = input.first() else {
            return Ok(0);
        };
        observe(&AtomicBool::new(false), Stage::Copy, 8192).map_err(|_| invalid())?;
        match self.phase {
            0 => {
                if self.scratch.len() == 256 {
                    return Err(invalid());
                }
                self.scratch.push(byte);
                const MARKER: &[u8] = b",\"entities\":[";
                if self.scratch.ends_with(MARKER) {
                    self.scratch.truncate(self.scratch.len() - MARKER.len());
                    self.scratch.push(b'}');
                    let header: Header =
                        serde_json::from_slice(&self.scratch).map_err(|_| invalid())?;
                    let timestamp =
                        Timestamp::parse_rfc3339(&header.timestamp).map_err(|_| invalid())?;
                    if header.format_version != VERSION
                        || header.instance_id != self.attempt.instance_id.to_string()
                        || header.revision < 0
                        || timestamp.to_unix_millis().map_err(|_| invalid())?
                            != self.attempt.completed_at_millis
                        || timestamp.to_rfc3339().map_err(|_| invalid())? != header.timestamp
                        || serde_json::to_vec(&header).map_err(|_| invalid())? != self.scratch
                    {
                        return Err(invalid());
                    }
                    self.revision = header.revision;
                    self.scratch.clear();
                    self.phase = 1;
                }
            }
            1 | 3 => {
                if byte == b']' && !self.after_comma {
                    self.draining = true;
                } else if self.separator {
                    if byte != b',' {
                        return Err(invalid());
                    }
                    self.separator = false;
                    self.after_comma = true;
                } else {
                    if byte != b'{' {
                        return Err(invalid());
                    }
                    self.after_comma = false;
                    self.decoder = Some(RecordDecoder::new(if self.phase == 1 {
                        RecordKind::Entity
                    } else {
                        RecordKind::Receipt
                    }));
                    return Ok(0);
                }
            }
            2 => {
                const MARKER: &[u8] = b",\"dedup\":[";
                self.scratch.push(byte);
                if !MARKER.starts_with(&self.scratch) {
                    return Err(invalid());
                }
                if self.scratch == MARKER {
                    self.scratch.clear();
                    self.phase = 3;
                }
            }
            4 if byte == b'}' => self.phase = 5,
            _ => return Err(invalid()),
        }
        Ok(1)
    }
    pub async fn feed_async(&mut self, input: &[u8]) -> Result<(), ApplicationError> {
        let mut offset = 0;
        while offset < input.len()
            || self.draining
            || self.pending.is_some()
            || self.cleanup.is_some()
        {
            let result = self.step(&input[offset..]);
            tokio::task::yield_now().await;
            offset += result?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn feed(&mut self, input: &[u8]) -> Result<(), ApplicationError> {
        let mut offset = 0;
        while offset < input.len()
            || self.draining
            || self.pending.is_some()
            || self.cleanup.is_some()
        {
            offset += self.step(&input[offset..])?;
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<(), ApplicationError> {
        if self.phase != 5 || self.decoder.is_some() {
            return Err(invalid());
        }
        Ok(())
    }
    fn accept(&mut self, record: &Record, digest: [u8; 32]) -> Result<(), ApplicationError> {
        match record {
            Record::Entity(entity) => {
                if entity.instance_id() != self.attempt.instance_id
                    || entity.revision().as_u64() > self.revision as u64
                    || self.entity_count == 65536
                {
                    return Err(invalid());
                }
                self.identities
                    .push(entity.id().as_uuid())
                    .map_err(|_| invalid())?;
                self.entity_count += 1;
                if let Some(owner) = entity.owner()
                    && !self.owners.contains(&owner.as_uuid())
                {
                    if self.owners.len() == 32 {
                        return Err(invalid());
                    }
                    self.owners.push(owner.as_uuid());
                }
                self.state.update(digest);
            }
            Record::Receipt(receipt) => {
                let command = orbisync_domain::CommandId::parse(&receipt.command_id)
                    .map_err(|_| invalid())?;
                if receipt.created_at_millis > self.attempt.completed_at_millis {
                    return Err(invalid());
                }
                let size = 104
                    + match &receipt.result {
                        ReceiptResult::Applied { response_payload } => response_payload.len(),
                        ReceiptResult::Rejected { code, detail } => code.len() + detail.len(),
                    };
                self.logical += size;
                if size > 524288 || self.logical > 8388608 || self.receipts.len() == 4096 {
                    return Err(invalid());
                }
                self.identities
                    .push(command.as_uuid())
                    .map_err(|_| invalid())?;
                self.receipts.push((
                    command.as_uuid(),
                    Receipt {
                        digest,
                        created: receipt.created_at_millis,
                        expires: receipt.expires_at_millis,
                    },
                ));
            }
        }
        Ok(())
    }
}
