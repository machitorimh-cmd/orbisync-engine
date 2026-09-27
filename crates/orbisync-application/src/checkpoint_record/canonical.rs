//! Scalar primitives for the closed generation profile. Legacy serde visitors
//! deliberately do not call these stricter readers.
//!
//! This module does not validate an envelope or authorize a record digest.
//! Its fixed scalar storage and explicit progress are building blocks for the
//! schema-specific record machine, not an end-to-end resource certificate.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

pub mod emit;
pub mod index;
pub mod reader;
pub mod work;

/// Separately versioned closed generation profile; not a legacy codec alias.
pub const VERSION: u32 = 5;
/// Maximum cooperative data-work quantum.
pub const WORK_QUANTUM: usize = 16_384;
/// Scalar storage bound. Integers require at most 20 bytes. A finite binary32
/// value needs at most nine significant decimal digits; even fixed notation
/// from the smallest subnormal through the largest finite value needs at most
/// 1 sign + 2 punctuation/leading bytes + 45 fractional places + 9 digits = 57.
/// Scientific notation needs fewer bytes (sign + 9 digits + dot + e + sign +
/// two exponent digits = 15). The 57-byte bound therefore covers either form;
/// an extra byte is not required because these slices are not NUL terminated.
pub const SCALAR_BYTES: usize = 57;

/// A scalar spelling outside the closed profile, or terminal cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarError {
    /// Invalid, noncanonical, nonfinite, or oversized scalar.
    Invalid,
    /// Cancellation is terminal for the current cursor.
    Cancelled,
}
impl std::fmt::Display for ScalarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid canonical checkpoint scalar",
            Self::Cancelled => "checkpoint cancelled",
        })
    }
}
impl std::error::Error for ScalarError {}

/// Only the numeric types present in the engine envelope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// Unsigned revision or byte.
    Unsigned(u64),
    /// Signed original receipt time.
    Signed(i64),
    /// Finite transform/visibility scalar, preserving signed zero.
    Float(f32),
}

/// Stack-owned canonical spelling, with no reallocating string or number tree.
#[derive(Debug, Clone)]
pub struct Scalar {
    bytes: [u8; SCALAR_BYTES],
    len: usize,
}
impl Scalar {
    /// Format through the pinned serde scalar implementation only.
    pub fn number(value: Number) -> Result<Self, ScalarError> {
        let mut out = Self {
            bytes: [0; SCALAR_BYTES],
            len: 0,
        };
        let result = match value {
            Number::Unsigned(v) => serde_json::to_writer(&mut out, &v),
            Number::Signed(v) => serde_json::to_writer(&mut out, &v),
            Number::Float(v) if v.is_finite() => serde_json::to_writer(&mut out, &v),
            Number::Float(_) => return Err(ScalarError::Invalid),
        };
        result.map_err(|_| ScalarError::Invalid)?;
        Ok(out)
    }
    /// Encoded scalar, without surrounding separators.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl Write for Scalar {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let end = self
            .len
            .checked_add(input.len())
            .filter(|end| *end <= SCALAR_BYTES)
            .ok_or_else(|| io::Error::other("canonical scalar capacity"))?;
        self.bytes[self.len..end].copy_from_slice(input);
        self.len = end;
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Numeric field type selected by the closed schema, never inferred from input.
#[derive(Debug, Clone, Copy)]
pub enum NumberKind {
    /// Decimal u64.
    Unsigned,
    /// Decimal i64.
    Signed,
    /// Finite binary32 with canonical serde spelling.
    Float,
}

/// Fixed-capacity numeric cursor. Delimiters belong to the enclosing schema.
#[derive(Debug)]
pub struct NumberReader {
    scalar: Scalar,
    failed: Option<ScalarError>,
}
impl Default for NumberReader {
    fn default() -> Self {
        Self {
            scalar: Scalar {
                bytes: [0; SCALAR_BYTES],
                len: 0,
            },
            failed: None,
        }
    }
}
impl NumberReader {
    /// Consume one numeric byte. Callers charge this bounded transition and
    /// scalar conversion against the same enclosing record work budget.
    pub fn push(&mut self, byte: u8, cancelled: &AtomicBool) -> Result<(), ScalarError> {
        if cancelled.load(Ordering::Acquire) {
            self.failed = Some(ScalarError::Cancelled);
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            || self.scalar.len == SCALAR_BYTES
        {
            self.failed = Some(ScalarError::Invalid);
            return Err(ScalarError::Invalid);
        }
        self.scalar.bytes[self.scalar.len] = byte;
        self.scalar.len += 1;
        Ok(())
    }
    /// Decode and compare with the producer spelling. serde sees at most 57
    /// bytes, never an arbitrary-length number. No digest is published here.
    pub fn finish(self, kind: NumberKind, cancelled: &AtomicBool) -> Result<Number, ScalarError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(ScalarError::Cancelled);
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        let bytes = self.scalar.as_bytes();
        let value = match kind {
            NumberKind::Unsigned => {
                Number::Unsigned(serde_json::from_slice(bytes).map_err(|_| ScalarError::Invalid)?)
            }
            NumberKind::Signed => {
                Number::Signed(serde_json::from_slice(bytes).map_err(|_| ScalarError::Invalid)?)
            }
            NumberKind::Float => {
                Number::Float(serde_json::from_slice(bytes).map_err(|_| ScalarError::Invalid)?)
            }
        };
        if Scalar::number(value)?.as_bytes() != bytes {
            return Err(ScalarError::Invalid);
        }
        Ok(value)
    }
}

/// Result of one bounded string emission step.
#[derive(Debug, Clone, Copy)]
pub struct EmitStep {
    /// Output bytes initialized by the step.
    pub written: usize,
    /// Charged source reads, control/scalar writes and output-copy work.
    pub work: usize,
    /// Closing quote was emitted.
    pub done: bool,
}

/// Resumable canonical JSON string emission. UTF-8 is already guaranteed by
/// `str`; scanning/escaping is bytewise, without a whole-string preprocessing
/// scan or allocation. The caller retains the original immutable string.
#[derive(Debug)]
pub struct StringEmitter<'a> {
    input: &'a str,
    offset: usize,
    pending: [u8; 6],
    pending_len: usize,
    pending_offset: usize,
    phase: u8,
    cancelled: bool,
}
impl<'a> StringEmitter<'a> {
    /// Pin the original immutable scalar for this cursor's lifetime.
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            pending: [0; 6],
            pending_len: 0,
            pending_offset: 0,
            phase: 0,
            cancelled: false,
        }
    }
    /// Emit into a caller-owned bounded output window. Charge hashing and downstream copies separately
    /// before they run; this function never claims to cover a consumer's work.
    pub fn step(
        &mut self,
        output: &mut [u8],
        budget: usize,
        cancelled: &AtomicBool,
    ) -> Result<EmitStep, ScalarError> {
        if cancelled.load(Ordering::Acquire) {
            self.cancelled = true;
        }
        if self.cancelled {
            return Err(ScalarError::Cancelled);
        }
        let mut written = 0;
        let mut work = 0;
        let budget = budget.min(WORK_QUANTUM);
        while written < output.len() {
            if self.pending_offset < self.pending_len {
                // Read pending byte and initialize destination byte.
                if work + 2 > budget {
                    break;
                }
                output[written] = self.pending[self.pending_offset];
                self.pending_offset += 1;
                written += 1;
                work += 2;
                continue;
            }
            if self.phase == 3 {
                break;
            }
            // One input read and at most six pending-byte writes; round up
            // for finite control transitions. Never hide a bulk scan here.
            if work + 8 > budget {
                break;
            }
            work += 8;
            self.pending_offset = 0;
            self.pending_len = 1;
            if self.phase == 0 {
                self.pending[0] = b'"';
                self.phase = 1;
            } else if self.offset == self.input.len() {
                self.pending[0] = b'"';
                self.phase = 3;
            } else {
                let byte = self.input.as_bytes()[self.offset];
                self.offset += 1;
                let escape = match byte {
                    b'"' => Some(b'"'),
                    b'\\' => Some(b'\\'),
                    b'\x08' => Some(b'b'),
                    b'\x0c' => Some(b'f'),
                    b'\n' => Some(b'n'),
                    b'\r' => Some(b'r'),
                    b'\t' => Some(b't'),
                    _ => None,
                };
                if let Some(escaped) = escape {
                    self.pending[..2].copy_from_slice(&[b'\\', escaped]);
                    self.pending_len = 2;
                } else if byte < 0x20 {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.pending = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(byte >> 4) as usize],
                        HEX[(byte & 15) as usize],
                    ];
                    self.pending_len = 6;
                } else {
                    self.pending[0] = byte;
                }
            }
        }
        Ok(EmitStep {
            written,
            work,
            done: self.phase == 3 && self.pending_offset == self.pending_len,
        })
    }
}

/// Incremental canonical string reader. The closed envelope's largest decoded
/// string is an 8192-byte receipt detail; arbitrary payload bytes use arrays.
/// No serde string scratch, whole-token copy or reallocation is used.
#[derive(Debug)]
pub struct StringReader {
    value: String,
    limit: usize,
    mode: StringMode,
    utf8: [u8; 4],
    failed: Option<ScalarError>,
}
#[derive(Debug, Clone, Copy)]
enum StringMode {
    Quote,
    Text,
    Escape,
    Hex { digits: u8, value: u8 },
    Utf8 { width: usize, filled: usize },
    Done,
}
impl StringReader {
    /// Allocate once to the schema field's bound. Allocation size is exposed
    /// by `scratch_bytes`; callers must account for simultaneous cursors.
    pub fn new(limit: usize) -> Result<Self, ScalarError> {
        if limit > super::DETAIL_BYTES {
            return Err(ScalarError::Invalid);
        }
        Ok(Self {
            value: String::with_capacity(limit),
            limit,
            mode: StringMode::Quote,
            utf8: [0; 4],
            failed: None,
        })
    }
    /// Object plus owned allocation capacity, excluding allocator bookkeeping
    /// and caller stack. This is not total record scratch.
    pub fn scratch_bytes(&self) -> usize {
        size_of::<Self>() + self.value.capacity()
    }
    fn append(&mut self, character: char) -> Result<(), ScalarError> {
        if self.value.len() + character.len_utf8() > self.limit {
            return Err(ScalarError::Invalid);
        }
        self.value.push(character);
        self.mode = StringMode::Text;
        Ok(())
    }
    /// Consume one byte, including opening/closing quotes. Each transition
    /// reads one byte, initializes at most four decoded bytes and validates
    /// at most one four-byte UTF-8 scalar. Enclosing work accounting must
    /// reserve 32 data-work units before calling. `true` means closing quote.
    pub fn push(&mut self, byte: u8, cancelled: &AtomicBool) -> Result<bool, ScalarError> {
        if cancelled.load(Ordering::Acquire) {
            self.failed = Some(ScalarError::Cancelled);
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.transition(byte);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn transition(&mut self, byte: u8) -> Result<bool, ScalarError> {
        match self.mode {
            StringMode::Quote if byte == b'"' => self.mode = StringMode::Text,
            StringMode::Text => match byte {
                b'"' => self.mode = StringMode::Done,
                b'\\' => self.mode = StringMode::Escape,
                0x20..=0x7f => self.append(char::from(byte))?,
                0xc2..=0xf4 => {
                    self.utf8[0] = byte;
                    let width = if byte < 0xe0 {
                        2
                    } else if byte < 0xf0 {
                        3
                    } else {
                        4
                    };
                    self.mode = StringMode::Utf8 { width, filled: 1 };
                }
                _ => return Err(ScalarError::Invalid),
            },
            StringMode::Escape => match byte {
                b'"' | b'\\' => self.append(char::from(byte))?,
                b'b' => self.append('\x08')?,
                b'f' => self.append('\x0c')?,
                b'n' => self.append('\n')?,
                b'r' => self.append('\r')?,
                b't' => self.append('\t')?,
                b'u' => {
                    self.mode = StringMode::Hex {
                        digits: 0,
                        value: 0,
                    }
                }
                _ => return Err(ScalarError::Invalid),
            },
            StringMode::Hex { digits, value } => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(ScalarError::Invalid),
                };
                if digits < 2 && digit != 0 {
                    return Err(ScalarError::Invalid);
                }
                let value = value * 16 + digit;
                if digits == 3 {
                    // serde emits Unicode escapes only for controls lacking a
                    // short escape; everything else must be raw UTF-8.
                    if value >= 0x20 || matches!(value, 8 | 9 | 10 | 12 | 13) {
                        return Err(ScalarError::Invalid);
                    }
                    self.append(char::from(value))?;
                } else {
                    self.mode = StringMode::Hex {
                        digits: digits + 1,
                        value,
                    };
                }
            }
            StringMode::Utf8 { width, filled } => {
                if byte & 0xc0 != 0x80 {
                    return Err(ScalarError::Invalid);
                }
                self.utf8[filled] = byte;
                if filled + 1 == width {
                    let character = std::str::from_utf8(&self.utf8[..width])
                        .map_err(|_| ScalarError::Invalid)?
                        .chars()
                        .next()
                        .ok_or(ScalarError::Invalid)?;
                    self.append(character)?;
                } else {
                    self.mode = StringMode::Utf8 {
                        width,
                        filled: filled + 1,
                    };
                }
            }
            _ => return Err(ScalarError::Invalid),
        }
        Ok(matches!(self.mode, StringMode::Done))
    }
    /// Transfer the successfully validated string allocation without a copy.
    /// Truncation and previous cancellation remain errors.
    pub fn finish(self, cancelled: &AtomicBool) -> Result<String, ScalarError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(ScalarError::Cancelled);
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(self.mode, StringMode::Done) {
            return Err(ScalarError::Invalid);
        }
        Ok(self.value)
    }
}

#[cfg(test)]
mod tests;
