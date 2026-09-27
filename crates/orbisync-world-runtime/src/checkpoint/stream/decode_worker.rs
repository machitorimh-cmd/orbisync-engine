use super::*;
mod canonical;
use std::io::Read;

struct Input<S> {
    source: S,
    runtime: tokio::runtime::Handle,
    manifest: StreamManifest,
    buffer: Vec<u8>,
    offset: usize,
    total: u64,
    chunks: u64,
    hash: Sha256,
    lexical: Lexical,
    cancelled: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
    record_limit: Arc<AtomicUsize>,
    record_bytes: usize,
    index_bytes: usize,
}
impl<S: CheckpointSource> Read for Input<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::other("checkpoint cancelled"));
        }
        if out.is_empty() {
            return Ok(0);
        }
        if self.offset == self.buffer.len() {
            self.buffer = Vec::new();
            self.offset = 0;
            let chunk = self.runtime.block_on(async {
                tokio::select! {
                    result = self.source.next_chunk() => result.map_err(|e| io::Error::other(e.to_string())),
                    () = async {
                        loop {
                            if self.cancelled.load(Ordering::Acquire) { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    } => Err(io::Error::other("checkpoint cancelled")),
                }
            })?;
            let Some(bytes) = chunk else {
                return Ok(0);
            };
            self.chunks += 1;
            if self.chunks > self.manifest.chunk_count {
                return Err(io::Error::other("chunk coverage mismatch"));
            }
            let expected = if self.chunks == self.manifest.chunk_count {
                self.manifest.serialized_bytes - (self.chunks - 1) * self.manifest.chunk_bytes
            } else {
                self.manifest.chunk_bytes
            };
            if bytes.len() as u64 != expected {
                return Err(io::Error::other("chunk length mismatch"));
            }
            self.total += bytes.len() as u64;
            for part in bytes.chunks(32) {
                observe(&self.cancelled, Stage::Hash, 8192).map_err(io::Error::other)?;
                self.hash.update(part);
                self.stats
                    .hash_unit
                    .fetch_max(part.len(), Ordering::Relaxed);
                self.stats.hashed.fetch_add(part.len(), Ordering::Relaxed);
            }
            self.stats
                .consumer
                .fetch_max(bytes.len(), Ordering::Relaxed);
            self.buffer = bytes;
        }
        let n = out.len().min(self.buffer.len() - self.offset).min(512);
        observe(&self.cancelled, Stage::Copy, 8192).map_err(io::Error::other)?;
        for (dst, &b) in out[..n]
            .iter_mut()
            .zip(&self.buffer[self.offset..self.offset + n])
        {
            // Top object -> array -> record object. Count original bytes, not
            // a re-encoding; ignored fields cannot bypass depth/token/record caps.
            if self.lexical.depth == 2 && !self.lexical.string && b == b'{' {
                self.record_bytes = 0;
            }
            let in_record = self.lexical.depth >= 3;
            self.lexical.byte(b)?;
            if in_record || self.lexical.depth >= 3 {
                self.record_bytes += 1;
                if self.record_bytes > self.record_limit.load(Ordering::Relaxed) {
                    return Err(io::Error::other("encoded record limit"));
                }
                self.stats
                    .record
                    .fetch_max(self.record_bytes, Ordering::Relaxed);
            }
            *dst = b;
        }
        self.offset += n;
        Ok(n)
    }
}

/// Owned decoder job. Coordinators retain this and their permits across
/// timeout/select; call cancel to join both decoder and source before release.
pub struct DecodeJob {
    cancelled: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<Result<canonical::Restoring, ApplicationError>>>,
    cleanup: Option<canonical::Restoring>,
}
impl DecodeJob {
    /// Start an ordered cursor or encoded source. No record byte buffer is
    /// collected: the shared canonical reader transfers validated records into unpublished state.
    pub fn start<S: CheckpointSource + 'static>(
        source: S,
        limits: CheckpointLimits,
        cancelled: Arc<AtomicBool>,
        stats: Arc<StreamStats>,
    ) -> Self {
        let runtime = tokio::runtime::Handle::current();
        let manifest = source.manifest().clone();
        let flag = Arc::clone(&cancelled);
        let task = tokio::task::spawn_blocking(move || {
            let record_limit = Arc::new(AtomicUsize::new(ENTITY));
            let mut input = Input {
                source,
                runtime,
                manifest,
                buffer: Vec::new(),
                offset: 0,
                total: 0,
                chunks: 0,
                hash: Sha256::new(),
                lexical: Lexical::default(),
                cancelled: flag,
                stats,
                record_limit: Arc::clone(&record_limit),
                record_bytes: 0,
                index_bytes: 0,
            };
            let result = (|| {
                limits.validate_manifest(&input.manifest)?;
                canonical::parse(&mut input)
            })();
            if result.is_err() {
                input.runtime.block_on(input.source.cancel());
            }
            input.stats.stopped.store(true, Ordering::Release);
            result
        });
        Self {
            cancelled,
            task: Some(task),
            cleanup: None,
        }
    }
    /// Wait for verified state. Cancelling this wait retains the join handle.
    pub async fn finish(&mut self) -> Result<Checkpoint, ApplicationError> {
        let task = self
            .task
            .as_mut()
            .ok_or_else(|| error("decoder already joined"))?;
        let result = task.await.map_err(error);
        self.task.take();
        result?.map(canonical::Restoring::release)
    }
    /// Cancel and join decoder/source; only then may coordinator release permits.
    /// Dropping this wait retains ownership; call cancel again to finish cleanup.
    pub async fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(task) = self.task.as_mut() {
            if let Ok(Ok(restored)) = task.await {
                self.cleanup = Some(restored);
            }
            self.task.take();
        }
        while let Some(restored) = self.cleanup.as_mut() {
            if restored.cleanup_step() {
                self.cleanup.take();
                break;
            }
            tokio::task::yield_now().await;
        }
    }
}
impl Drop for DecodeJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Convenience owned decode. Coordinators needing timeout/cancellation must use
/// DecodeJob and join cancellation before releasing work/transaction permits.
pub async fn decode<S: CheckpointSource + 'static>(
    source: S,
    limits: CheckpointLimits,
    cancelled: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
) -> Result<Checkpoint, ApplicationError> {
    DecodeJob::start(source, limits, cancelled, stats)
        .finish()
        .await
}
