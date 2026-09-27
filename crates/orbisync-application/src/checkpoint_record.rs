//! Streaming receipt wire fields. No serde tagged-enum Content tree is built.
//! These visitors bound retained fields, not the JSON reader's string scratch.
//! Cancellation and encoded token/depth limits remain the reader's responsibility.

#[cfg(test)]
mod lexer_probe;

pub mod canonical;

use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// Maximum retained response bytes in a receipt.
pub const RESPONSE_BYTES: usize = 262_144;
/// Maximum retained rejection code bytes.
pub const CODE_BYTES: usize = 128;
/// Maximum retained rejection detail bytes.
pub const DETAIL_BYTES: usize = 8_192;

/// Deserialize a bounded string without copying an oversized decoded token.
/// The caller must also bound the underlying JSON reader's token scratch.
pub fn bounded_string<'de, D: Deserializer<'de>, const MAX: usize>(
    de: D,
) -> Result<String, D::Error> {
    match Capture::Text(MAX).deserialize(de)? {
        Captured::Text(value) => Ok(value),
        _ => Err(D::Error::custom("invalid or oversized receipt string")),
    }
}

/// Deserialize a byte array without retaining more than `MAX` bytes.
/// No per-element JSON values are retained. `MAX` bounds logical length;
/// vector capacity and the reader's token scratch must be accounted separately.
pub fn bounded_bytes<'de, D: Deserializer<'de>, const MAX: usize>(
    de: D,
) -> Result<Vec<u8>, D::Error> {
    match Capture::Bytes(MAX).deserialize(de)? {
        Captured::Bytes(value) => Ok(value),
        _ => Err(D::Error::custom("invalid or oversized receipt bytes")),
    }
}

/// Protocol-neutral v4 receipt result, with streaming tagged deserialization.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReceiptResult {
    /// Opaque encoded response, retained directly as bytes.
    Applied {
        /// Encoded response retained for replay.
        response_payload: Vec<u8>,
    },
    /// Deterministic failure text.
    Rejected {
        /// Deterministic error code.
        code: String,
        /// Human-readable failure detail.
        detail: String,
    },
}

enum Captured {
    Invalid,
    Byte(u8),
    Bytes(Vec<u8>),
    Text(String),
}
#[derive(Clone, Copy)]
enum Capture {
    Byte,
    Bytes(usize),
    Text(usize),
}

// Unlike IgnoredAny, deserialize_any preserves the former Content visitor's
// validation of opaque numbers and strings (e.g. 1e400 and lone surrogates).
// Walk opaque values without retaining nodes, keys or strings.
struct Discard;
impl<'de> DeserializeSeed<'de> for Discard {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Discard {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JSON value")
    }
    fn visit_u64<E: Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(Discard)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_key_seed(Discard)?.is_some() {
            map.next_value_seed(Discard)?;
        }
        Ok(())
    }
}
impl<'de> DeserializeSeed<'de> for Capture {
    type Value = Captured;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<Captured, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Capture {
    type Value = Captured;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded receipt field")
    }
    fn visit_u64<E: Error>(self, value: u64) -> Result<Captured, E> {
        Ok(match (self, u8::try_from(value)) {
            (Self::Byte, Ok(value)) => Captured::Byte(value),
            _ => Captured::Invalid,
        })
    }
    fn visit_i64<E: Error>(self, value: i64) -> Result<Captured, E> {
        Ok(match (self, u8::try_from(value)) {
            (Self::Byte, Ok(value)) => Captured::Byte(value),
            _ => Captured::Invalid,
        })
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<Captured, E> {
        Ok(Captured::Invalid)
    }
    fn visit_bool<E: Error>(self, _: bool) -> Result<Captured, E> {
        Ok(Captured::Invalid)
    }
    fn visit_unit<E: Error>(self) -> Result<Captured, E> {
        Ok(Captured::Invalid)
    }
    fn visit_str<E: Error>(self, value: &str) -> Result<Captured, E> {
        Ok(match self {
            Self::Text(limit) if value.len() <= limit => Captured::Text(value.to_owned()),
            _ => Captured::Invalid,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Captured, A::Error> {
        let Self::Bytes(limit) = self else {
            while seq.next_element_seed(Discard)?.is_some() {}
            return Ok(Captured::Invalid);
        };
        let mut bytes = Vec::new();
        let mut valid = true;
        while let Some(value) = seq.next_element_seed(Self::Byte)? {
            if valid {
                if let Captured::Byte(value) = value
                    && bytes.len() < limit
                {
                    // Only bytes are retained, never a per-element AST. The
                    // capacity high-water mark is checked by the boundary tests.
                    bytes.push(value);
                } else {
                    valid = false;
                    bytes = Vec::new();
                }
            }
        }
        Ok(if valid {
            Captured::Bytes(bytes)
        } else {
            Captured::Invalid
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Captured, A::Error> {
        while map.next_key_seed(Discard)?.is_some() {
            map.next_value_seed(Discard)?;
        }
        Ok(Captured::Invalid)
    }
}

enum Key {
    Type,
    Payload,
    Code,
    Detail,
    Other,
}
impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = Key;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("receipt result key")
            }
            fn visit_str<E: Error>(self, key: &str) -> Result<Key, E> {
                Ok(match key {
                    "type" => Key::Type,
                    "response_payload" => Key::Payload,
                    "code" => Key::Code,
                    "detail" => Key::Detail,
                    _ => Key::Other,
                })
            }
        }
        de.deserialize_identifier(KeyVisitor)
    }
}
#[derive(Default)]
struct Field {
    value: Option<Captured>,
    duplicate: bool,
}
impl Field {
    fn read<'de, A: MapAccess<'de>>(
        &mut self,
        map: &mut A,
        capture: Capture,
    ) -> Result<(), A::Error> {
        if self.value.is_some() {
            self.duplicate = true;
            map.next_value_seed(Discard)?;
        } else {
            self.value = Some(map.next_value_seed(capture)?);
        }
        Ok(())
    }
    fn take<E: Error>(self) -> Result<Captured, E> {
        if self.duplicate {
            return Err(E::custom("duplicate receipt result field"));
        }
        self.value
            .ok_or_else(|| E::custom("missing receipt result field"))
    }
    fn text<E: Error>(self) -> Result<String, E> {
        match self.take()? {
            Captured::Text(value) => Ok(value),
            _ => Err(E::custom("invalid receipt result text")),
        }
    }
}
impl<'de> Deserialize<'de> for ReceiptResult {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct ResultVisitor;
        impl<'de> Visitor<'de> for ResultVisitor {
            type Value = ReceiptResult;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("receipt result object")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                // Preserve serde's legacy positional representation as well as
                // the object representation emitted by the v4 serializer.
                let tag = Field {
                    value: seq.next_element_seed(Capture::Text(8))?,
                    duplicate: false,
                };
                let result = match tag.text::<A::Error>()?.as_str() {
                    "applied" => match seq.next_element_seed(Capture::Bytes(RESPONSE_BYTES))? {
                        Some(Captured::Bytes(response_payload)) => {
                            ReceiptResult::Applied { response_payload }
                        }
                        _ => return Err(A::Error::custom("invalid receipt response bytes")),
                    },
                    "rejected" => {
                        let code = Field {
                            value: seq.next_element_seed(Capture::Text(CODE_BYTES))?,
                            duplicate: false,
                        };
                        let detail = Field {
                            value: seq.next_element_seed(Capture::Text(DETAIL_BYTES))?,
                            duplicate: false,
                        };
                        ReceiptResult::Rejected {
                            code: code.text()?,
                            detail: detail.text()?,
                        }
                    }
                    _ => return Err(A::Error::custom("invalid receipt result type")),
                };
                if seq.next_element_seed(Discard)?.is_some() {
                    return Err(A::Error::custom("extra receipt result field"));
                }
                Ok(result)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut tag = Field::default();
                let mut payload = Field::default();
                let mut code = Field::default();
                let mut detail = Field::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Type => tag.read(&mut map, Capture::Text(8))?,
                        Key::Payload => payload.read(&mut map, Capture::Bytes(RESPONSE_BYTES))?,
                        Key::Code => code.read(&mut map, Capture::Text(CODE_BYTES))?,
                        Key::Detail => detail.read(&mut map, Capture::Text(DETAIL_BYTES))?,
                        Key::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                match tag.text::<A::Error>()?.as_str() {
                    "applied" => match payload.take()? {
                        Captured::Bytes(response_payload) => {
                            Ok(ReceiptResult::Applied { response_payload })
                        }
                        _ => Err(A::Error::custom("invalid receipt response bytes")),
                    },
                    "rejected" => Ok(ReceiptResult::Rejected {
                        code: code.text()?,
                        detail: detail.text()?,
                    }),
                    _ => Err(A::Error::custom("invalid receipt result type")),
                }
            }
        }
        de.deserialize_any(ResultVisitor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // The previous wire decoder, used only as a compatibility oracle. Domain
    // size checks happened after this decoder; the new visitor applies those
    // existing ceilings while capturing fields.
    #[derive(Debug, Deserialize, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Previous {
        Applied { response_payload: Vec<u8> },
        Rejected { code: String, detail: String },
    }

    #[test]
    fn receipt_visitor_preserves_tag_order_escaping_opaque_and_u64() {
        for input in [
            r#"["applied",[0,255]]"#,
            r#"["rejected","X",""]"#,
            r#"["applied",[1],0]"#,
            r#"["applied"]"#,
            r#"["rejected","X"]"#,
            r#"["rejected","X","",null]"#,
            r#"[]"#,
            r#"{"type":0,"response_payload":[1]}"#,
            r#"{"type":1,"code":"X","detail":""}"#,
            r#"[0,[1]]"#,
            r#"{"type":"applied","response_payload":[0,127,255]}"#,
            r#"{"response_payload":[0,255],"type":"applied"}"#,
            r#"{"response_payload":[0],"opaque":{"b":[null,true,{"u":18446744073709551615}],"a":"\u65e5\n\\\""},"type":"applied"}"#,
            r#"{"type":"rejected","code":"FAIL","detail":"\u65e5\n\\\""}"#,
            r#"{"detail":"text","code":"FAIL","type":"rejected"}"#,
            r#"{"type":"rejected","response_payload":{"ignored":[256,-1,1.5]},"code":"X","detail":""}"#,
            r#"{"response_payload":null,"response_payload":[1],"type":"rejected","code":"X","detail":""}"#,
            r#"{"code":false,"code":{},"detail":[null],"type":"applied","response_payload":[1]}"#,
            r#"{"type":"applied","response_payload":[1],"opaque":1,"opaque":2}"#,
            r#"{"type":"applied","response_payload":[1,256]}"#,
            r#"{"type":"applied","response_payload":[1,1.5]}"#,
            r#"{"type":"applied","response_payload":[1,-1]}"#,
            r#"{"type":"applied","response_payload":[1,18446744073709551615]}"#,
            r#"{"type":"applied","response_payload":[1,true]}"#,
            r#"{"type":"applied","response_payload":[1,null]}"#,
            r#"{"type":"applied","response_payload":[1,{}]}"#,
            r#"{"type":"applied","response_payload":[1,[]]}"#,
            r#"{"type":"applied","response_payload":null}"#,
            r#"{"type":"rejected","code":false,"detail":""}"#,
            r#"{"type":"applied","type":"applied","response_payload":[1]}"#,
            r#"{"type":"applied","response_payload":[1],"response_payload":[2]}"#,
            r#"{"type":"rejected","code":"X","code":"Y","detail":""}"#,
            r#"{"type":"rejected","code":"X","detail":"","detail":""}"#,
            r#"{"type":null,"response_payload":[1]}"#,
            r#"{"type":"other","response_payload":[1]}"#,
            r#"{"response_payload":[1]}"#,
            r#"{"type":"applied","response_payload":[1],"opaque":{"bad":1e400}}"#,
            r#"{"type":"applied","response_payload":[1],"opaque":"\ud800"}"#,
            r#"{"type":"applied","response_payload":[1],"code":0,"code":[1e400]}"#,
            r#"{"type":"rejected","response_payload":[{"bad":1e400}],"code":"X","detail":""}"#,
        ] {
            let old = serde_json::from_str::<Previous>(input);
            let new = serde_json::from_reader::<_, ReceiptResult>(input.as_bytes());
            assert_eq!(old.is_ok(), new.is_ok(), "{input}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap()
                );
            }
        }
    }

    #[test]
    fn large_receipt_is_bytes_not_per_element_ast_and_caps_ignore_unused_fields() {
        let payload = vec![255_u8; RESPONSE_BYTES];
        let input = format!(
            r#"{{"response_payload":{},"type":"applied"}}"#,
            serde_json::to_string(&payload).unwrap()
        );
        let result: ReceiptResult = serde_json::from_reader(input.as_bytes()).unwrap();
        let response_payload = match result {
            ReceiptResult::Applied { response_payload } => response_payload,
            ReceiptResult::Rejected { .. } => Vec::new(),
        };
        assert_eq!(response_payload, payload);
        assert_eq!(response_payload.capacity(), RESPONSE_BYTES);
        for (tag, accepted) in [("applied", false), ("rejected", true)] {
            let input = format!(
                r#"{{"response_payload":{},"code":"X","detail":"","type":"{tag}"}}"#,
                serde_json::to_string(&vec![0_u8; RESPONSE_BYTES + 1]).unwrap()
            );
            assert_eq!(
                serde_json::from_reader::<_, ReceiptResult>(input.as_bytes()).is_ok(),
                accepted
            );
        }
        for (field, bound) in [("code", CODE_BYTES), ("detail", DETAIL_BYTES)] {
            let other = if field == "code" { "detail" } else { "code" };
            for len in [bound, bound + 1] {
                let input = format!(
                    r#"{{"type":"rejected","{field}":"{}","{other}":"X"}}"#,
                    "a".repeat(len)
                );
                assert_eq!(
                    serde_json::from_reader::<_, ReceiptResult>(input.as_bytes()).is_ok(),
                    len == bound
                );
            }
        }
    }

    #[test]
    fn opaque_large_record_and_reader_interruption_do_not_require_an_ast() {
        let input = format!(
            r#"{{"opaque":{{"nested":[{},"{}"]}},"response_payload":[0,255],"type":"applied"}}"#,
            serde_json::to_string(&vec![0_u8; RESPONSE_BYTES]).unwrap(),
            "\\u65e5\\n".repeat(32_768),
        );
        let decoded: ReceiptResult = serde_json::from_reader(input.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            r#"{"type":"applied","response_payload":[0,255]}"#
        );

        struct Interrupted<'a> {
            input: &'a [u8],
            remaining: usize,
        }
        impl std::io::Read for Interrupted<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("checkpoint cancelled"));
                }
                let n = self.remaining.min(out.len()).min(self.input.len());
                out[..n].copy_from_slice(&self.input[..n]);
                self.input = &self.input[n..];
                self.remaining -= n;
                Ok(n)
            }
        }
        // Interrupt within the opaque array and then within its escaped string.
        // The error must propagate through the actual streaming record visitor.
        for cutoff in [16_384, 600_000] {
            let mut reader = Interrupted {
                input: input.as_bytes(),
                remaining: cutoff,
            };
            let error = serde_json::from_reader::<_, ReceiptResult>(&mut reader).unwrap_err();
            assert!(error.to_string().contains("checkpoint cancelled"));
            assert_eq!(input.len() - reader.input.len(), cutoff);
        }
    }
}
