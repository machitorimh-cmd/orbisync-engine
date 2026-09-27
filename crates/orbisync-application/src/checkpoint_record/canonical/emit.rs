//! Shared record emission and admission sizing. Each sink call is at most 512
//! bytes and is preceded by cancellation observation. The sink must separately
//! account for its own copies/hashing and must not collect a whole checkpoint.

use super::{Number, Scalar, ScalarError, StringEmitter};
use orbisync_domain::{Entity, Transform, Vec3, VisibilityPolicy};
use std::{
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
};

/// Borrowed receipt result; opaque responses are never cloned for emission.
pub enum ResultRef<'a> {
    /// Successful encoded response.
    Applied(&'a [u8]),
    /// Deterministic rejection code and detail.
    Rejected {
        /// Stable error code.
        code: &'a str,
        /// Original detail.
        detail: &'a str,
    },
}
/// Borrowed original receipt fields.
pub struct ReceiptRef<'a> {
    /// Command UUID.
    pub command_id: &'a str,
    /// Original payload fingerprint.
    pub fingerprint: &'a [u8],
    /// Original creation time.
    pub created_at_millis: i64,
    /// Original expiration time.
    pub expires_at_millis: i64,
    /// Replay message identity.
    pub message_id: &'a str,
    /// Original response.
    pub result: ResultRef<'a>,
}
fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
struct Emitter<'a, W> {
    sink: W,
    cancelled: &'a AtomicBool,
    bytes: usize,
    limit: usize,
}
impl<W: Write> Emitter<'_, W> {
    fn check(&self) -> io::Result<()> {
        super::work::observe(self.cancelled, super::work::Stage::Emit, 8192).map_err(error)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(error(ScalarError::Cancelled));
        }
        Ok(())
    }
    fn raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        for part in bytes.chunks(512) {
            self.check()?;
            if part.len() > self.limit.saturating_sub(self.bytes) {
                return Err(error("canonical record limit"));
            }
            self.sink.write_all(part)?;
            self.bytes += part.len();
        }
        Ok(())
    }
    fn text(&mut self, text: &str) -> io::Result<()> {
        let mut emitter = StringEmitter::new(text);
        let mut output = [0; 512];
        loop {
            let step = emitter
                .step(&mut output, 8192, self.cancelled)
                .map_err(error)?;
            self.raw(&output[..step.written])?;
            if step.done {
                return Ok(());
            }
        }
    }
    fn number(&mut self, number: Number) -> io::Result<()> {
        self.check()?;
        self.raw(Scalar::number(number).map_err(error)?.as_bytes())
    }
    fn bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.raw(b"[")?;
        for (i, &byte) in bytes.iter().enumerate() {
            if i != 0 {
                self.raw(b",")?;
            }
            self.number(Number::Unsigned(u64::from(byte)))?;
        }
        self.raw(b"]")
    }
    fn vector(&mut self, vector: Vec3) -> io::Result<()> {
        self.raw(b"{\"x\":")?;
        self.number(Number::Float(vector.x()))?;
        self.raw(b",\"y\":")?;
        self.number(Number::Float(vector.y()))?;
        self.raw(b",\"z\":")?;
        self.number(Number::Float(vector.z()))?;
        self.raw(b"}")
    }
    fn transform(&mut self, transform: Option<Transform>) -> io::Result<()> {
        let Some(transform) = transform else {
            return self.raw(b"null");
        };
        self.raw(b"{\"position\":")?;
        self.vector(transform.position())?;
        self.raw(b",\"rotation\":{\"x\":")?;
        self.number(Number::Float(transform.rotation().x()))?;
        self.raw(b",\"y\":")?;
        self.number(Number::Float(transform.rotation().y()))?;
        self.raw(b",\"z\":")?;
        self.number(Number::Float(transform.rotation().z()))?;
        self.raw(b",\"w\":")?;
        self.number(Number::Float(transform.rotation().w()))?;
        self.raw(b"},\"scale\":")?;
        self.vector(transform.scale())?;
        self.raw(b"}")
    }
    fn visibility(&mut self, visibility: &VisibilityPolicy) -> io::Result<()> {
        self.raw(b"{\"type\":")?;
        match visibility {
            VisibilityPolicy::Global => self.text("global")?,
            VisibilityPolicy::OwnerOnly => self.text("owner_only")?,
            VisibilityPolicy::Spatial { radius } => {
                self.text("spatial")?;
                self.raw(b",\"radius\":")?;
                self.number(Number::Float(*radius))?;
            }
            VisibilityPolicy::Custom { tag } => {
                self.text("custom")?;
                self.raw(b",\"tag\":")?;
                self.text(tag.as_str())?;
            }
            VisibilityPolicy::RoleRestricted { roles } => {
                self.text("role_restricted")?;
                self.raw(b",\"roles\":[")?;
                for (i, id) in roles.iter().enumerate() {
                    if i != 0 {
                        self.raw(b",")?;
                    }
                    self.check()?;
                    self.text(&id.to_string())?;
                }
                self.raw(b"]")?;
            }
            VisibilityPolicy::Explicit { users } => {
                self.text("explicit")?;
                self.raw(b",\"users\":[")?;
                for (i, id) in users.iter().enumerate() {
                    if i != 0 {
                        self.raw(b",")?;
                    }
                    self.check()?;
                    self.text(&id.to_string())?;
                }
                self.raw(b"]")?;
            }
        }
        self.raw(b"}")
    }
}

/// Emit one entity using the closed profile. Returns the exact byte count;
/// callers can use `io::sink()` for admission sizing with identical encoding.
pub fn entity<W: Write>(entity: &Entity, sink: W, cancelled: &AtomicBool) -> io::Result<usize> {
    let mut out = Emitter {
        sink,
        cancelled,
        bytes: 0,
        limit: 1_048_576,
    };
    out.check()?;
    out.raw(b"{\"id\":")?;
    out.text(&entity.id().to_string())?;
    out.raw(b",\"instance_id\":")?;
    out.text(&entity.instance_id().to_string())?;
    out.raw(b",\"kind\":")?;
    out.text(entity.kind().as_str())?;
    out.raw(b",\"owner\":")?;
    if let Some(owner) = entity.owner() {
        out.text(&owner.to_string())?;
    } else {
        out.raw(b"null")?;
    }
    out.raw(b",\"transform\":")?;
    out.transform(entity.transform())?;
    out.raw(b",\"visibility\":")?;
    out.visibility(entity.visibility())?;
    out.raw(b",\"revision\":")?;
    out.number(Number::Unsigned(entity.revision().as_u64()))?;
    out.raw(b",\"created_at\":")?;
    out.text(&entity.created_at().to_rfc3339().map_err(error)?)?;
    out.raw(b",\"updated_at\":")?;
    out.text(&entity.updated_at().to_rfc3339().map_err(error)?)?;
    out.raw(b",\"components\":{")?;
    if entity.components().len() > 16 {
        return Err(error("component count"));
    }
    // At most sixteen 128-byte keys. Sorting borrows payloads and never scans
    // or copies them. Fixed storage avoids Vec growth while constructing it.
    let mut components: [Option<(&str, &[u8])>; 16] = [None; 16];
    for (slot, (key, bytes)) in components.iter_mut().zip(entity.components()) {
        if key.len() > 128 || bytes.len() > 4096 {
            return Err(error("component limit"));
        }
        *slot = Some((key.as_str(), bytes.as_slice()));
    }
    let count = entity.components().len();
    let mut merged = [None; 16];
    let mut width = 1;
    while width < count {
        for start in (0..count).step_by(width * 2) {
            let middle = (start + width).min(count);
            let end = (middle + width).min(count);
            let (mut left, mut right) = (start, middle);
            for slot in &mut merged[start..end] {
                out.check()?;
                let take_left = right == end
                    || (left < middle
                        && components[left].map(|(key, _)| key)
                            <= components[right].map(|(key, _)| key));
                if take_left {
                    *slot = components[left];
                    left += 1;
                } else {
                    *slot = components[right];
                    right += 1;
                }
            }
        }
        out.check()?;
        components = merged; // Fixed sixteen-descriptor copy, never payloads.
        width *= 2;
    }
    out.check()?;
    for (index, entry) in components.into_iter().flatten().enumerate() {
        if index != 0 {
            out.raw(b",")?;
        }
        out.text(entry.0)?;
        out.raw(b":")?;
        out.bytes(entry.1)?;
    }
    out.raw(b"}}")?;
    Ok(out.bytes)
}

/// Emit an original receipt without cloning response bytes or interpreting them.
pub fn receipt<W: Write>(
    receipt: ReceiptRef<'_>,
    sink: W,
    cancelled: &AtomicBool,
) -> io::Result<usize> {
    let mut out = Emitter {
        sink,
        cancelled,
        bytes: 0,
        limit: 2_097_152,
    };
    out.raw(b"{\"command_id\":")?;
    out.text(receipt.command_id)?;
    out.raw(b",\"fingerprint\":")?;
    out.bytes(receipt.fingerprint)?;
    out.raw(b",\"created_at_millis\":")?;
    out.number(Number::Signed(receipt.created_at_millis))?;
    out.raw(b",\"expires_at_millis\":")?;
    out.number(Number::Signed(receipt.expires_at_millis))?;
    out.raw(b",\"message_id\":")?;
    out.text(receipt.message_id)?;
    out.raw(b",\"result\":{\"type\":")?;
    match receipt.result {
        ResultRef::Applied(bytes) => {
            out.text("applied")?;
            out.raw(b",\"response_payload\":")?;
            out.bytes(bytes)?;
        }
        ResultRef::Rejected { code, detail } => {
            out.text("rejected")?;
            out.raw(b",\"code\":")?;
            out.text(code)?;
            out.raw(b",\"detail\":")?;
            out.text(detail)?;
        }
    }
    out.raw(b"}}")?;
    Ok(out.bytes)
}
