//! Test-only feasibility probe, NOT a JSON parser or production decoder.
//! The lexer owns no heap storage and accepts discontiguous input windows.
//! Grammar, duplicate keys and numeric conversion stay with the serde oracle.
//! Those oracle operations are deliberately outside the step-work guarantee.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};

const QUANTUM: usize = 16_384;
// Conservative data-work charge per input transition: one source-byte read,
// at most four scalar-buffer writes or a <=4-byte UTF-8 check. Metadata updates
// take constant work. Delivered decoded bytes cost two units (read + output).
const INPUT_COST: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Symbol(u8),
    StringStart,
    StringByte(u8),
    StringEnd,
    Atom { start: usize, end: usize },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Progress {
    Event(Event),
    Budget,
    Input,
    End,
}
#[derive(Debug)]
struct Step {
    consumed: usize,
    work: usize,
    progress: Progress,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Error {
    Cancelled,
    String,
    Truncated,
    Budget,
}
#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Ground,
    Atom(usize),
    String,
    Escape,
    Hex {
        value: u16,
        digits: u8,
        high: u16,
    },
    HighSlash(u16),
    HighU(u16),
    Utf8 {
        filled: u8,
        width: u8,
    },
}
#[derive(Default)]
struct Lexer {
    mode: Mode,
    offset: usize,
    bytes: [u8; 4],
    emitted: u8,
    ready: u8,
    failed: Option<Error>,
}
fn delimiter(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\n' | b'\r' | b'\t' | b'{' | b'}' | b'[' | b']' | b':' | b',' | b'"'
    )
}
impl Lexer {
    fn step(
        &mut self,
        input: &[u8],
        eof: bool,
        budget: usize,
        cancel: &AtomicBool,
    ) -> Result<Step, Error> {
        if cancel.load(Ordering::Acquire) {
            self.failed = Some(Error::Cancelled);
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        if budget < INPUT_COST {
            return Err(Error::Budget);
        }
        let mut step = Step {
            consumed: 0,
            work: 0,
            progress: Progress::Budget,
        };
        let limit = budget.min(QUANTUM);
        loop {
            if cancel.load(Ordering::Acquire) {
                self.failed = Some(Error::Cancelled);
                return Err(Error::Cancelled);
            }
            if self.emitted < self.ready {
                if limit - step.work < 2 {
                    return Ok(step);
                }
                let byte = self.bytes[self.emitted as usize];
                self.emitted += 1;
                step.work += 2;
                step.progress = Progress::Event(Event::StringByte(byte));
                return Ok(step);
            }
            if limit - step.work < INPUT_COST {
                return Ok(step);
            }
            if step.consumed == input.len() {
                step.progress = if !eof {
                    Progress::Input
                } else {
                    step.work += 1;
                    match self.mode {
                        Mode::Ground => Progress::End,
                        Mode::Atom(start) => {
                            self.mode = Mode::Ground;
                            Progress::Event(Event::Atom {
                                start,
                                end: self.offset,
                            })
                        }
                        _ => {
                            self.failed = Some(Error::Truncated);
                            return Err(Error::Truncated);
                        }
                    }
                };
                return Ok(step);
            }
            let byte = input[step.consumed];
            step.work += INPUT_COST;
            if let Mode::Atom(start) = self.mode
                && delimiter(byte)
            {
                self.mode = Mode::Ground;
                step.progress = Progress::Event(Event::Atom {
                    start,
                    end: self.offset,
                });
                return Ok(step); // delimiter is inspected again on the next step
            }
            step.consumed += 1;
            self.offset += 1;
            match self.byte(byte) {
                Ok(Some(event)) => {
                    step.progress = Progress::Event(event);
                    return Ok(step);
                }
                Ok(None) => {}
                Err(error) => {
                    self.failed = Some(error);
                    return Err(error);
                }
            }
        }
    }

    fn queue(&mut self, scalar: char) {
        self.ready = scalar.encode_utf8(&mut self.bytes).len() as u8;
        self.emitted = 0;
        self.mode = Mode::String;
    }

    fn byte(&mut self, b: u8) -> Result<Option<Event>, Error> {
        match self.mode {
            Mode::Ground => match b {
                b' ' | b'\n' | b'\r' | b'\t' => {}
                b'{' | b'}' | b'[' | b']' | b':' | b',' => return Ok(Some(Event::Symbol(b))),
                b'"' => {
                    self.mode = Mode::String;
                    return Ok(Some(Event::StringStart));
                }
                _ => self.mode = Mode::Atom(self.offset - 1),
            },
            Mode::Atom(_) => {}
            Mode::String => match b {
                b'"' => {
                    self.mode = Mode::Ground;
                    return Ok(Some(Event::StringEnd));
                }
                b'\\' => self.mode = Mode::Escape,
                0..=31 => return Err(Error::String),
                32..=127 => self.queue(char::from(b)),
                _ => {
                    let width = match b {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(Error::String),
                    };
                    self.bytes[0] = b;
                    self.mode = Mode::Utf8 { filled: 1, width };
                }
            },
            Mode::Escape => match b {
                b'"' | b'\\' | b'/' => self.queue(char::from(b)),
                b'b' => self.queue('\x08'),
                b'f' => self.queue('\x0c'),
                b'n' => self.queue('\n'),
                b'r' => self.queue('\r'),
                b't' => self.queue('\t'),
                b'u' => {
                    self.mode = Mode::Hex {
                        value: 0,
                        digits: 0,
                        high: 0,
                    }
                }
                _ => return Err(Error::String),
            },
            Mode::Hex {
                value,
                digits,
                high,
            } => {
                let digit = char::from(b).to_digit(16).ok_or(Error::String)? as u16;
                let value = (value << 4) | digit;
                if digits != 3 {
                    self.mode = Mode::Hex {
                        value,
                        digits: digits + 1,
                        high,
                    };
                } else if high != 0 {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return Err(Error::String);
                    }
                    let scalar =
                        0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(value) - 0xdc00;
                    self.queue(char::from_u32(scalar).ok_or(Error::String)?);
                } else if (0xd800..=0xdbff).contains(&value) {
                    self.mode = Mode::HighSlash(value);
                } else {
                    self.queue(char::from_u32(u32::from(value)).ok_or(Error::String)?);
                }
            }
            Mode::HighSlash(high) => {
                if b != b'\\' {
                    return Err(Error::String);
                }
                self.mode = Mode::HighU(high);
            }
            Mode::HighU(high) => {
                if b != b'u' {
                    return Err(Error::String);
                }
                self.mode = Mode::Hex {
                    value: 0,
                    digits: 0,
                    high,
                };
            }
            Mode::Utf8 { filled, width } => {
                self.bytes[filled as usize] = b;
                if filled + 1 == width {
                    std::str::from_utf8(&self.bytes[..width as usize])
                        .map_err(|_| Error::String)?;
                    self.ready = width;
                    self.emitted = 0;
                    self.mode = Mode::String;
                } else {
                    self.mode = Mode::Utf8 {
                        filled: filled + 1,
                        width,
                    };
                }
            }
        }
        Ok(None)
    }
}

// Test-only grammar/duplicate/canonicalization oracle, matching storage Strict.
// Its AST and fixture buffers are NOT part of the lexer's measured state.
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use sha2::{Digest, Sha256};
struct Strict;
impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Strict {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unique-key JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(value) = seq.next_element_seed(Strict)? {
            out.push(value);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(A::Error::custom("duplicate key"));
            }
            out.insert(key, map.next_value_seed(Strict)?);
        }
        Ok(Value::Object(out))
    }
}
fn canonical(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut de = serde_json::Deserializer::from_slice(input);
    let value = Strict.deserialize(&mut de).map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

fn through_steps(input: &[u8], window: usize, budget: usize) -> Result<Vec<u8>, String> {
    let mut lexer = Lexer::default();
    let cancel = AtomicBool::new(false);
    let mut cursor = 0;
    let mut end = window.min(input.len());
    let mut output = Vec::new();
    let mut string = Vec::new();
    let mut total_work = 0;
    loop {
        let step = lexer
            .step(&input[cursor..end], end == input.len(), budget, &cancel)
            .map_err(|e| format!("{e:?}"))?;
        assert!(step.work <= budget.min(QUANTUM));
        total_work += step.work;
        cursor += step.consumed;
        match step.progress {
            Progress::Input => {
                assert_eq!(cursor, end);
                end = (end + window).min(input.len());
            }
            Progress::Budget => assert!(step.work > 0),
            Progress::End => {
                assert_eq!(cursor, input.len());
                break;
            }
            Progress::Event(event) => match event {
                Event::Symbol(byte) => output.push(byte),
                Event::StringStart => string.clear(),
                Event::StringByte(byte) => string.push(byte),
                Event::StringEnd => {
                    let text = std::str::from_utf8(&string).map_err(|e| e.to_string())?;
                    serde_json::to_writer(&mut output, text).map_err(|e| e.to_string())?;
                    output.push(b' '); // never merge originally separate tokens
                }
                Event::Atom { start, end } => {
                    // Preserve spelling until the existing codec converts once.
                    // Parsing/serializing/reparsing here could add f64 rounding.
                    // Final oracle conversion is synchronous and NOT covered
                    // by the lexer budget, especially for long numeric tokens.
                    output.extend_from_slice(&input[start..end]);
                    output.push(b' ');
                }
            },
        }
    }
    // Each source byte is inspected at most twice (an atom's delimiter),
    // charged eight units per inspection; decoded string output is <= input
    // length and charged two units per byte. EOF costs at most two units.
    assert!(total_work <= 18 * input.len() + 2);
    canonical(&output)
}

#[test]
fn lexer_probe_matches_storage_oracle_for_numeric_unicode_opaque_and_duplicates() {
    let cases = [
        r#"{"u":18446744073709551615,"i":-9223372036854775808,"z":-0,"f":1e0}"#,
        r#"[18446744073709551616,-9223372036854775809,1.7976931348623157e308,5e-324,1e-400,0e999999999999999999999]"#,
        r#"{"opaque":{"z":[null,true,false,1.234567890123456789],"a":{"b":"\u0000\b\f\n\r\t\/\\\""}}}"#,
        r#"{"\u0061":"日本é😀","pair":"\ud83d\ude00","low":"\udfff"}"#,
        r#"{"\u0061":"日本é😀","pair":"\ud83d\ude00","bmp":"\ud7ff\ue000\uffff"}"#,
        r#"{"a":1,"\u0061":2}"#,
        r#"{"é":1,"\u00e9":2}"#,
        r#"{"😀":1,"\ud83d\ude00":2}"#,
        r#"{"opaque":[{"x":1,"x":2}]}"#,
        r#"[1e400,0]"#,
        r#"[1e-]"#,
        r#"[00]"#,
        r#"[-]"#,
        r#"[.1]"#,
        r#"[1.]"#,
        r#"[+1]"#,
        r#"[NaN]"#,
        r#"[truefalse]"#,
        r#"{"x":"\ud800"}"#,
        r#"{"x":"\ud800\u0041"}"#,
        r#"{"x":"\ud800\ud800"}"#,
        r#"{"x":"\u12xz"}"#,
        r#"{"x":"\q"}"#,
        r#"{"x":"unterminated}"#,
        r#"{"x":1 2}"#,
        r#"["x" "y"]"#,
        r#"[1,]"#,
        r#"{"x":true,}"#,
        r#"[1}"#,
        r#"{"x" true}"#,
        "",
        " ",
        "true false",
    ];
    for input in cases {
        let old = canonical(input.as_bytes());
        for (window, budget) in [(1, 8), (3, 9), (17, 31), (65_536, QUANTUM)] {
            let new = through_steps(input.as_bytes(), window, budget);
            assert_eq!(
                new.is_ok(),
                old.is_ok(),
                "{input}, window={window}, budget={budget}"
            );
            if let (Ok(old), Ok(new)) = (&old, new) {
                assert_eq!(new, *old, "{input}");
                assert_eq!(Sha256::digest(&new), Sha256::digest(old));
            }
        }
    }
}

#[test]
fn lexer_probe_rejects_bad_utf8_at_every_byte_boundary() {
    for bad in [
        &[0xc0, 0xaf][..],
        &[0xed, 0xa0, 0x80],
        &[0xf4, 0x90, 0x80, 0x80],
        &[0x80],
        &[0xe2, 0x82],
        &[0xff],
        &[0],
        &[31],
    ] {
        let mut input = b"{\"opaque\":\"".to_vec();
        input.extend(bad);
        input.extend(b"\"}");
        assert!(canonical(&input).is_err());
        for window in 1..=5 {
            assert!(through_steps(&input, window, 8).is_err());
        }
    }
}

#[test]
fn lexer_probe_budget_pause_resume_and_sticky_cancellation() {
    let input = format!(
        r#"{{"opaque":"{}","number":0.{}1}}"#,
        "日\\ud83d\\ude00\\n".repeat(8192),
        "0".repeat(65_536)
    );
    assert_eq!(
        through_steps(input.as_bytes(), 137, 31).unwrap(),
        canonical(input.as_bytes()).unwrap()
    );
    // Budget exhaustion inside a long numeric token, before any semantic conversion.
    let number = "9".repeat(65_536);
    let mut lexer = Lexer::default();
    let cancel = AtomicBool::new(false);
    let first = lexer
        .step(number.as_bytes(), true, usize::MAX, &cancel)
        .unwrap();
    assert_eq!(first.progress, Progress::Budget);
    assert_eq!(first.work, QUANTUM);
    assert_eq!(first.consumed, QUANTUM / INPUT_COST);
    let offset = lexer.offset;
    // Move the suspended state: there is no running task or borrowed input window.
    let mut resumed = lexer;
    assert_eq!(
        resumed.step(&[], false, 0, &cancel).unwrap_err(),
        Error::Budget
    );
    assert_eq!(resumed.offset, offset);
    cancel.store(true, Ordering::Release);
    assert_eq!(
        resumed
            .step(&number.as_bytes()[offset..], true, QUANTUM, &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Release);
    assert_eq!(
        resumed
            .step(&number.as_bytes()[offset..], true, QUANTUM, &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(resumed.offset, offset);
    assert!(size_of::<Lexer>() <= 128);
    println!(
        "lexer state={} bytes; step result={} bytes; no lexer heap; fixture/oracle allocations excluded",
        size_of::<Lexer>(),
        size_of::<Step>()
    );
}

#[test]
fn lexer_probe_unicode_scalar_edges_and_numeric_spellings() {
    for scalar in (0..=127).chain([0x7ff, 0x800, 0xd7ff, 0xe000, 0xffff, 0x10000, 0x10ffff]) {
        let character = char::from_u32(scalar).unwrap().to_string();
        let escaped = if scalar <= 0xffff {
            format!(r#""\u{scalar:04x}""#)
        } else {
            let high = 0xd800 + ((scalar - 0x10000) >> 10);
            let low = 0xdc00 + ((scalar - 0x10000) & 1023);
            format!(r#""\u{high:04x}\u{low:04x}""#)
        };
        let expected = serde_json::to_vec(&character).unwrap();
        assert_eq!(through_steps(escaped.as_bytes(), 1, 8).unwrap(), expected);
        assert_eq!(through_steps(&expected, 1, 8).unwrap(), expected);
    }
    for mantissa in [
        "0",
        "-0",
        "1",
        "-1",
        "18446744073709551615",
        "18446744073709551616",
        "1.23456789012345678901234567890",
    ] {
        for exponent in [
            "",
            "e0",
            "E+0",
            "e308",
            "e309",
            "e-324",
            "e-999999999999999999999",
        ] {
            let input = format!(r#"{{"opaque":{mantissa}{exponent}}}"#);
            let old = canonical(input.as_bytes());
            let new = through_steps(input.as_bytes(), 2, 15);
            assert_eq!(old.is_ok(), new.is_ok(), "{input}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old, new, "{input}");
            }
        }
    }
}

#[test]
fn lexer_probe_cancels_partial_escape_utf8_and_queued_output() {
    for prefix in [
        &b"\"\\u12"[..],
        &b"\"\\ud83d"[..],
        &b"\"\\ud83d\\"[..],
        &b"\"\xf0\x9f"[..],
        &b"\"\\ud83d\\ude00"[..],
    ] {
        let mut lexer = Lexer::default();
        let cancel = AtomicBool::new(false);
        let mut offset = 0;
        while offset != prefix.len() {
            let step = lexer.step(&prefix[offset..], false, 8, &cancel).unwrap();
            offset += step.consumed;
        }
        assert_eq!(lexer.offset, prefix.len());
        cancel.store(true, Ordering::Release);
        assert_eq!(
            lexer.step(b"ignored", true, QUANTUM, &cancel).unwrap_err(),
            Error::Cancelled
        );
        cancel.store(false, Ordering::Release);
        assert_eq!(
            lexer.step(b"ignored", true, QUANTUM, &cancel).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(lexer.offset, prefix.len());
    }
}
