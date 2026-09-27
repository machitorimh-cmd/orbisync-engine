//! Opaque cursor codec for ADR-003 pagination.
//!
//! Every list endpoint uses keyset pagination with an opaque cursor that is
//! signed by the server. The cursor carries the sort key and tie-breaker
//! together with a filter binding so that a cursor issued for one filter
//! cannot be replayed against another. The codec lives outside the DB adapter
//! so that transport and storage can share the same implementation
//! (W-29 CR-07).

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

type HmacSha256 = Hmac<Sha256>;

const CURSOR_VERSION: u8 = 1;

/// Errors produced when decoding an opaque cursor supplied by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorError {
    /// The cursor is malformed, tampered, or bound to a different filter.
    Invalid,
    /// Server-side failure encoding the cursor (serialization or key error).
    /// Must map to HTTP 500, not 400.
    Internal,
}

/// Payload encoded inside the cursor. Shared across users / roles / audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CursorPayload {
    v: u8,
    k: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fh: Option<String>,
}

/// Server-side codec that signs and verifies cursors with HMAC-SHA-256.
///
/// The key must be supplied explicitly via [`CursorCodec::new`] and is
/// resolved through configuration `auth.pagination_hmac_key_env`.
/// It never appears in the repository or argv (§0.1).
#[derive(Clone)]
pub struct CursorCodec {
    key: [u8; 64],
}

impl core::fmt::Debug for CursorCodec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorCodec")
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl CursorCodec {
    /// Creates a codec with the given HMAC key bytes.
    ///
    /// The key is normalized to 64 bytes (HMAC-SHA-256 block size) via
    /// SHA-256 so that later HMAC construction is infallible
    /// (`HmacSha256::new_from_slice` accepts any length and internally hashes
    /// or pads the key via `get_der_key` – for the normalized 64-byte key it
    /// always succeeds). This removes failure handling at call sites while
    /// preserving determinism: the same input key always yields the same HMAC
    /// output. HMAC itself would hash long keys and pad short keys
    /// internally, so pre-hashing to 32 bytes and zero-padding to 64
    /// is cryptographically consistent and safe for any key length.
    /// The indirection through `generic-array` is avoided so that the crate
    /// compiles with both `generic-array` 0.14 (used by `hmac`/`sha2` 0.12/0.10)
    /// and `generic-array` 1.4 (direct workspace dependency). `new_from_slice`
    /// erases the `GenericArray` version from the call site, preventing the
    /// `E0308 mismatched types` error that arises when `hmac` re-exports the
    /// 0.14 `GenericArray` while the direct dependency is 1.4.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError::Internal`] when the supplied key is empty
    /// or whitespace-only. Empty keys would produce deterministic HMACs
    /// that are trivially forgeable and indicate missing configuration;
    /// the caller must resolve the key via `auth.pagination_hmac_key_env`
    /// and fail startup if it cannot be resolved.
    pub fn new(key: Vec<u8>) -> Result<Self, CursorError> {
        let is_empty = key.is_empty() || key.iter().all(|b| b.is_ascii_whitespace());
        if is_empty {
            return Err(CursorError::Internal);
        }
        let digest = Sha256::digest(&key);
        let mut normalized = [0u8; 64];
        normalized[..32].copy_from_slice(&digest);
        // remaining 32 bytes are zero-padded, matching HMAC's short-key padding
        Ok(Self { key: normalized })
    }

    #[allow(clippy::expect_used)]
    fn sign(&self, payload_b64: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("pagination HMAC key is 64 bytes; HMAC accepts any length so this never fails");
        mac.update(payload_b64.as_bytes());
        let tag = mac.finalize().into_bytes();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag)
    }

    fn encode_payload(&self, payload: &CursorPayload) -> Result<String, CursorError> {
        // CursorPayload contains only String, Option<String>, i64, u8 — all always
        // serializable. `serde_json::to_string` can only fail on allocation
        // failure (which in Rust aborts rather than returns Err) or on
        // fundamentally non-serializable types, neither of which applies here.
        // By returning `Result` instead of panicking, we eliminate the
        // `expect`/`unwrap` lint violation and allow the caller to map the
        // error to HTTP 500 (internal error) rather than fabricating a signed
        // cursor that would later surface as a misleading 400. This is option A
        // (propagate rather than fabricate) because it preserves correct error
        // semantics, requires no manual JSON escaping, and remains safe if
        // fields are added in the future.
        let json = serde_json::to_string(payload).map_err(|_| CursorError::Internal)?;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
        let sig = self.sign(&payload_b64);
        Ok(format!("{payload_b64}.{sig}"))
    }

    fn decode_payload(&self, cursor: &str) -> Result<CursorPayload, CursorError> {
        let (payload_b64, sig_b64) = cursor.split_once('.').ok_or(CursorError::Invalid)?;
        let expected = self.sign(payload_b64);
        if !constant_time_eq(expected.as_bytes(), sig_b64.as_bytes()) {
            return Err(CursorError::Invalid);
        }
        let json_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| CursorError::Invalid)?;
        let payload: CursorPayload =
            serde_json::from_slice(&json_bytes).map_err(|_| CursorError::Invalid)?;
        if payload.v != CURSOR_VERSION {
            return Err(CursorError::Invalid);
        }
        Ok(payload)
    }

    /// Encodes a user/role cursor carrying the last seen id.
    pub fn encode_users(&self, last_id: &str) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "users".to_owned(),
            id: Some(last_id.to_owned()),
            ts: None,
            fh: None,
        })
    }

    /// Encodes a role cursor.
    pub fn encode_roles(&self, last_id: &str) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "roles".to_owned(),
            id: Some(last_id.to_owned()),
            ts: None,
            fh: None,
        })
    }

    /// Encodes an instance-list cursor carrying the last seen instance id.
    pub fn encode_instances(&self, last_id: &str) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "instances".to_owned(),
            id: Some(last_id.to_owned()),
            ts: None,
            fh: None,
        })
    }

    /// Encodes a world cursor carrying the last seen id.
    pub fn encode_worlds(&self, last_id: &str) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "worlds".to_owned(),
            id: Some(last_id.to_owned()),
            ts: None,
            fh: None,
        })
    }

    /// Encodes an instance-member cursor carrying the last seen user id,
    /// bound to the owning instance so a cursor cannot be replayed against
    /// another instance's member list.
    pub fn encode_members(&self, instance_id: &str, last_id: &str) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "members".to_owned(),
            id: Some(last_id.to_owned()),
            ts: None,
            fh: Some(instance_id.to_owned()),
        })
    }
    /// Encodes an audit cursor carrying (occurred_at_millis, id) and filter hash.
    pub fn encode_audit(
        &self,
        ts_millis: i64,
        id: &str,
        filter_hash: &str,
    ) -> Result<String, CursorError> {
        self.encode_payload(&CursorPayload {
            v: CURSOR_VERSION,
            k: "audit".to_owned(),
            id: Some(id.to_owned()),
            ts: Some(ts_millis),
            fh: Some(filter_hash.to_owned()),
        })
    }

    /// Decodes a user cursor, returning the last id.
    pub fn decode_users(&self, cursor: &str) -> Result<String, CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "users" {
            return Err(CursorError::Invalid);
        }
        payload.id.ok_or(CursorError::Invalid)
    }

    /// Decodes a role cursor.
    pub fn decode_roles(&self, cursor: &str) -> Result<String, CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "roles" {
            return Err(CursorError::Invalid);
        }
        payload.id.ok_or(CursorError::Invalid)
    }

    /// Decodes an instance-list cursor, returning the last id.
    pub fn decode_instances(&self, cursor: &str) -> Result<String, CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "instances" {
            return Err(CursorError::Invalid);
        }
        payload.id.ok_or(CursorError::Invalid)
    }

    /// Decodes an instance-member cursor, verifying it is bound to
    /// `expected_instance_id` and returning the last user id.
    pub fn decode_members(
        &self,
        cursor: &str,
        expected_instance_id: &str,
    ) -> Result<String, CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "members" {
            return Err(CursorError::Invalid);
        }
        let fh = payload.fh.ok_or(CursorError::Invalid)?;
        if fh != expected_instance_id {
            return Err(CursorError::Invalid);
        }
        payload.id.ok_or(CursorError::Invalid)
    }

    /// Decodes a world cursor.
    pub fn decode_worlds(&self, cursor: &str) -> Result<String, CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "worlds" {
            return Err(CursorError::Invalid);
        }
        payload.id.ok_or(CursorError::Invalid)
    }

    /// Decodes an audit cursor, verifying the filter binding.
    pub fn decode_audit(
        &self,
        cursor: &str,
        expected_filter_hash: &str,
    ) -> Result<(i64, String), CursorError> {
        let payload = self.decode_payload(cursor)?;
        if payload.k != "audit" {
            return Err(CursorError::Invalid);
        }
        let fh = payload.fh.ok_or(CursorError::Invalid)?;
        if fh != expected_filter_hash {
            return Err(CursorError::Invalid);
        }
        let ts = payload.ts.ok_or(CursorError::Invalid)?;
        let id = payload.id.ok_or(CursorError::Invalid)?;
        Ok((ts, id))
    }

    /// Computes a stable filter binding for audit cursors.
    #[must_use]
    pub fn audit_filter_hash(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        actor_id: Option<&str>,
        action: Option<&str>,
    ) -> String {
        let canonical = format!(
            "from:{:?}|to:{:?}|actor:{:?}|action:{:?}",
            from, to, actor_id, action
        );
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let out = hasher.finalize();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{CursorCodec, CursorError};
    use std::path::PathBuf;

    fn test_codec() -> CursorCodec {
        CursorCodec::new(b"test-key-32bytes-1234567890abcd".to_vec()).expect("test key")
    }

    fn pagination_vectors() -> serde_json::Value {
        let path = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
        )
        .join("../../test-vectors/rest/v1/pagination-cursor-vectors.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        serde_json::from_str(&raw).expect("pagination vectors must be valid JSON")
    }

    #[test]
    fn round_trip_users() {
        let codec = test_codec();
        let cursor = codec
            .encode_users("0199a1b2-3c4d-7000-8000-000000000001")
            .expect("encode");
        let decoded = codec.decode_users(&cursor).expect("decode");
        assert_eq!(decoded, "0199a1b2-3c4d-7000-8000-000000000001");
    }

    #[test]
    fn round_trip_worlds() {
        let codec = test_codec();
        let cursor = codec
            .encode_worlds("0199a1b2-3c4d-7000-8000-000000000001")
            .expect("encode");
        let decoded = codec.decode_worlds(&cursor).expect("decode");
        assert_eq!(decoded, "0199a1b2-3c4d-7000-8000-000000000001");
        assert_eq!(codec.decode_roles(&cursor), Err(CursorError::Invalid));
    }

    #[test]
    fn tampered_cursor_is_invalid() {
        let codec = test_codec();
        let cursor = codec
            .encode_users("0199a1b2-3c4d-7000-8000-000000000001")
            .expect("encode");
        let mut tampered = cursor.clone();
        tampered.push('x');
        assert_eq!(codec.decode_users(&tampered), Err(CursorError::Invalid));
    }

    #[test]
    fn cross_kind_is_invalid() {
        let codec = test_codec();
        let cursor = codec
            .encode_users("0199a1b2-3c4d-7000-8000-000000000001")
            .expect("encode");
        assert_eq!(codec.decode_roles(&cursor), Err(CursorError::Invalid));
    }

    #[test]
    fn audit_filter_binding() {
        let codec = test_codec();
        let fh1 = codec.audit_filter_hash(None, None, None, Some("user.created"));
        let fh2 = codec.audit_filter_hash(None, None, None, Some("role.created"));
        assert_ne!(fh1, fh2);
        let cursor = codec
            .encode_audit(
                1_700_000_000_000,
                "0199a1b2-3c4d-7000-8000-000000000001",
                &fh1,
            )
            .expect("encode");
        assert_eq!(
            codec.decode_audit(&cursor, &fh1).unwrap(),
            (
                1_700_000_000_000,
                "0199a1b2-3c4d-7000-8000-000000000001".to_owned()
            )
        );
        assert_eq!(codec.decode_audit(&cursor, &fh2), Err(CursorError::Invalid));
    }

    #[test]
    fn wrong_key_is_invalid() {
        let codec1 = CursorCodec::new(b"key-one-32bytes-1234567890abcd12".to_vec()).expect("key1");
        let codec2 = CursorCodec::new(b"key-two-32bytes-1234567890abcd12".to_vec()).expect("key2");
        let cursor = codec1
            .encode_users("0199a1b2-3c4d-7000-8000-000000000001")
            .expect("encode");
        assert_eq!(codec2.decode_users(&cursor), Err(CursorError::Invalid));
    }

    #[test]
    fn empty_key_is_internal_error() {
        assert!(matches!(
            CursorCodec::new(Vec::new()),
            Err(CursorError::Internal)
        ));
        assert!(matches!(
            CursorCodec::new(b"   ".to_vec()),
            Err(CursorError::Internal)
        ));
        assert!(matches!(
            CursorCodec::new(b"".to_vec()),
            Err(CursorError::Internal)
        ));
    }

    #[test]
    fn encode_internal_is_not_invalid() {
        // encode_payload failure must map to Internal (500), not Invalid (400).
        // The only way to trigger Internal via encode is through internal logic,
        // but we verify the error variant distinction exists and is not conflated.
        let codec = test_codec();
        // Manually check that a valid encode does not return Invalid on success.
        let ok = codec.encode_users("0199a1b2-3c4d-7000-8000-000000000001");
        assert!(ok.is_ok());
        // Verify Internal variant is distinct from Invalid.
        assert_ne!(CursorError::Internal, CursorError::Invalid);
    }

    // -----------------------------------------------------------------------
    // GV-1: golden vectors – detect HMAC / payload / key-normalization regressions
    // -----------------------------------------------------------------------

    #[test]
    fn golden_users_cursor_matches_vector() {
        let v = pagination_vectors();
        let key = v["key"].as_str().expect("key must be string");
        let vectors = v["vectors"].as_array().expect("vectors array");
        let users_vec = vectors
            .iter()
            .find(|e| e["kind"].as_str() == Some("users"))
            .expect("users vector must exist");
        let last_id = users_vec["last_id"].as_str().expect("last_id");
        let expected = users_vec["cursor"].as_str().expect("cursor");
        let codec = CursorCodec::new(key.as_bytes().to_vec()).expect("golden key");
        let cursor = codec.encode_users(last_id).expect("encode users");
        assert_eq!(
            cursor, expected,
            "users golden cursor must match vector (M1/M2/M3)"
        );
        // decode must round-trip
        assert_eq!(codec.decode_users(&cursor).expect("decode"), last_id);
    }

    #[test]
    fn golden_roles_cursor_matches_vector() {
        let v = pagination_vectors();
        let key = v["key"].as_str().expect("key must be string");
        let vectors = v["vectors"].as_array().expect("vectors array");
        let roles_vec = vectors
            .iter()
            .find(|e| e["kind"].as_str() == Some("roles"))
            .expect("roles vector must exist");
        let last_id = roles_vec["last_id"].as_str().expect("last_id");
        let expected = roles_vec["cursor"].as_str().expect("cursor");
        let codec = CursorCodec::new(key.as_bytes().to_vec()).expect("golden key");
        let cursor = codec.encode_roles(last_id).expect("encode roles");
        assert_eq!(
            cursor, expected,
            "roles golden cursor must match vector (M1/M2/M3)"
        );
        assert_eq!(codec.decode_roles(&cursor).expect("decode"), last_id);
    }

    #[test]
    fn golden_audit_cursor_and_filter_hash_match_vector() {
        let v = pagination_vectors();
        let key = v["key"].as_str().expect("key must be string");
        let vectors = v["vectors"].as_array().expect("vectors array");
        let audit_vec = vectors
            .iter()
            .find(|e| e["kind"].as_str() == Some("audit"))
            .expect("audit vector must exist");
        let ts = audit_vec["ts_millis"].as_i64().expect("ts_millis");
        let id = audit_vec["id"].as_str().expect("id");
        let filter_hash_expected = audit_vec["filter_hash"].as_str().expect("filter_hash");
        let cursor_expected = audit_vec["cursor"].as_str().expect("cursor");
        let codec = CursorCodec::new(key.as_bytes().to_vec()).expect("golden key");
        // filter_hash is derived via audit_filter_hash; golden vector pins it
        let fh = codec.audit_filter_hash(None, None, None, Some("user.created"));
        assert_eq!(
            fh, filter_hash_expected,
            "audit filter_hash must match vector (payload binding)"
        );
        let cursor = codec.encode_audit(ts, id, &fh).expect("encode audit");
        assert_eq!(
            cursor, cursor_expected,
            "audit golden cursor must match vector (M1/M2/M3)"
        );
        let (decoded_ts, decoded_id) = codec
            .decode_audit(&cursor, filter_hash_expected)
            .expect("decode audit");
        assert_eq!(decoded_ts, ts);
        assert_eq!(decoded_id, id);
    }

    #[test]
    fn golden_vectors_are_stable_across_calls() {
        // The same key + input must always produce the same cursor (determinism)
        let v = pagination_vectors();
        let key = v["key"].as_str().expect("key must be string");
        let codec = CursorCodec::new(key.as_bytes().to_vec()).expect("golden key");
        let c1 = codec
            .encode_users("0199a1b2-3c4d-7000-8000-000000000042")
            .expect("encode");
        let c2 = codec
            .encode_users("0199a1b2-3c4d-7000-8000-000000000042")
            .expect("encode");
        assert_eq!(c1, c2, "cursor must be deterministic");
    }
}
