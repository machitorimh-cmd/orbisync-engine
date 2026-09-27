//! Shared generation codec policy and adapter metadata. Legacy adapters retain
//! their explicit 8 MiB contract. No configuration enables generation writes.
use crate::ApplicationError;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum byte work between checkpoint cancellation observations.
pub const CHECKPOINT_WORK_BYTES: usize = 16 * 1024;

/// Visit bytes without copying them, observing cancellation before each work
/// unit and after the final unit. The visitor must not retain or combine units
/// into a larger synchronous operation. This bounds only the work in `visit`,
/// not a caller's parsing, allocation, validation or destruction work.
pub fn visit_checkpoint_bytes(
    bytes: &[u8],
    cancelled: &AtomicBool,
    mut visit: impl FnMut(&[u8]),
) -> Result<(), ApplicationError> {
    for part in bytes.chunks(CHECKPOINT_WORK_BYTES) {
        if cancelled.load(Ordering::Acquire) {
            return Err(ApplicationError::port_failure("checkpoint cancelled"));
        }
        visit(part);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(ApplicationError::port_failure("checkpoint cancelled"));
    }
    Ok(())
}

/// Validated immutable policy shared by admission, codec and future storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointLimits {
    chunk: usize,
    total: usize,
}
impl Default for CheckpointLimits {
    fn default() -> Self {
        Self {
            chunk: 262_144,
            total: 8_388_608,
        }
    }
}
impl CheckpointLimits {
    /// Validate configuration before constructing any generation component.
    pub fn new(chunk: u64, total: u64) -> Result<Self, ApplicationError> {
        if !(16_384..=1_048_576).contains(&chunk)
            || !(2_097_152..=67_108_864).contains(&total)
            || chunk > total
            || total
                .checked_sub(1)
                .and_then(|v| v.checked_div(chunk))
                .and_then(|v| v.checked_add(1))
                .is_none_or(|v| v > 4096)
        {
            return Err(ApplicationError::port_failure("invalid checkpoint limits"));
        }
        Ok(Self {
            chunk: usize::try_from(chunk)
                .map_err(|e| ApplicationError::port_failure(e.to_string()))?,
            total: usize::try_from(total)
                .map_err(|e| ApplicationError::port_failure(e.to_string()))?,
        })
    }
    /// Maximum emitted chunk size.
    pub fn chunk_bytes(self) -> usize {
        self.chunk
    }
    /// Maximum complete uncompressed JSON bytes, including receipts/metadata.
    pub fn max_serialized_bytes(self) -> usize {
        self.total
    }
    /// Validate a selected manifest using its own C, never current write C.
    pub fn validate_manifest(self, manifest: &StreamManifest) -> Result<(), ApplicationError> {
        let c = manifest.chunk_bytes;
        let t = manifest.serialized_bytes;
        if !(16_384..=1_048_576).contains(&c)
            || t == 0
            || t > self.total as u64
            || manifest.chunk_count != 1 + (t - 1) / c
            || manifest.chunk_count > 4096
        {
            return Err(ApplicationError::port_failure(
                "selected checkpoint exceeds policy or has invalid manifest",
            ));
        }
        Ok(())
    }
}

/// Immutable byte metadata for a pinned stream. The adapter also binds this to
/// GenerationAttempt identity and checks per-chunk hashes/contiguous indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamManifest {
    /// Complete JSON byte length.
    pub serialized_bytes: u64,
    /// Chunk size used by this generation, independent of later configuration.
    pub chunk_bytes: u64,
    /// Exact contiguous chunk count.
    pub chunk_count: u64,
    /// Incremental SHA-256 of complete JSON bytes.
    pub digest: [u8; 32],
}

/// One ordered, bounded source. Adapters must fetch one chunk per cursor page,
/// enforce metadata/actual lengths before allocating, and never collect chunks.
#[async_trait::async_trait]
pub trait CheckpointSource: Send {
    /// Validated policy retained for this job.
    fn limits(&self) -> CheckpointLimits;
    /// Immutable selected manifest.
    fn manifest(&self) -> &StreamManifest;
    /// Pull one chunk, with backpressure. EOF is required for success.
    /// This wait may be dropped by decoder cancellation. Retain producer and
    /// cursor cleanup ownership in the source so a subsequent cancel can join.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError>;
    /// Close queue, signal cancellation, and join producer before releasing permit.
    /// An interrupted wait must retain cleanup ownership for a repeated cancel;
    /// callers must retain the source and await cleanup before releasing permits.
    async fn cancel(&mut self);
}

#[async_trait::async_trait]
impl CheckpointSource for Box<dyn CheckpointSource> {
    fn limits(&self) -> CheckpointLimits {
        (**self).limits()
    }
    fn manifest(&self) -> &StreamManifest {
        (**self).manifest()
    }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
        (**self).next_chunk().await
    }
    async fn cancel(&mut self) {
        (**self).cancel().await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn checkpoint_hash_work_preserves_digest_and_observes_mid_hash_cancellation() {
        // Maximum configured C; input lives in the caller, not helper scratch.
        let bytes = vec![0xa5; 1_048_576];
        let cancelled = AtomicBool::new(false);
        let mut hash = Sha256::new();
        let mut largest = 0;
        visit_checkpoint_bytes(&bytes, &cancelled, |part| {
            largest = largest.max(part.len());
            hash.update(part);
        })
        .unwrap();
        assert_eq!(largest, CHECKPOINT_WORK_BYTES);
        assert_eq!(hash.finalize(), Sha256::digest(&bytes));

        let mut visited = 0;
        let mut hash = Sha256::new();
        assert!(
            visit_checkpoint_bytes(&bytes, &cancelled, |part| {
                hash.update(part);
                visited += part.len();
                cancelled.store(true, Ordering::Release);
            })
            .is_err()
        );
        assert_eq!(visited, CHECKPOINT_WORK_BYTES);
        assert_eq!(hash.finalize(), Sha256::digest(&bytes[..visited]));
    }

    #[test]
    fn checkpoint_work_observes_cancellation_at_entry_and_final_unit() {
        let cancelled = AtomicBool::new(true);
        for bytes in [&[][..], &[1][..]] {
            let mut visited = false;
            assert!(visit_checkpoint_bytes(bytes, &cancelled, |_| visited = true).is_err());
            assert!(!visited);
        }
        for len in [1, CHECKPOINT_WORK_BYTES - 1, CHECKPOINT_WORK_BYTES] {
            cancelled.store(false, Ordering::Release);
            let bytes = vec![0; len];
            assert!(
                visit_checkpoint_bytes(&bytes, &cancelled, |_| {
                    cancelled.store(true, Ordering::Release);
                })
                .is_err()
            );
        }
    }

    #[test]
    fn checked_policy_and_manifest_ceilings() {
        for (c, t) in [
            (0, 0),
            (u64::MAX, u64::MAX),
            (16_383, 2_097_152),
            (1_048_577, 67_108_864),
            (16_384, 67_108_865),
            (16_384, 2_097_151),
        ] {
            assert!(CheckpointLimits::new(c, t).is_err());
        }
        let limits = CheckpointLimits::new(1_048_576, 67_108_864).unwrap();
        let mut m = StreamManifest {
            serialized_bytes: 67_108_864,
            chunk_bytes: 16_384,
            chunk_count: 4096,
            digest: [0; 32],
        };
        assert!(limits.validate_manifest(&m).is_ok());
        m.serialized_bytes += 1;
        assert!(limits.validate_manifest(&m).is_err());
        m.serialized_bytes = 1;
        m.chunk_count = 1;
        assert!(limits.validate_manifest(&m).is_ok()); // C can exceed actual total
        m.chunk_bytes = 0;
        assert!(limits.validate_manifest(&m).is_err());
        m.chunk_bytes = 16_384;
        m.serialized_bytes = u64::MAX;
        assert!(limits.validate_manifest(&m).is_err());
    }
}
