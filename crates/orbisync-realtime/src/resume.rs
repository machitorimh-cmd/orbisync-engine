//! Resume token validation and resync decision.
//!
//! Implements `docs/design/mobile-resume-interest-backpressure.md` §3
//! (Resume Token) and §4 (Resync) and the normative decision table in
//! `contracts/realtime-resume-token-policy.json` and
//! `docs/design/realtime-protocol-and-connection.md` §6.1.
//!
//! `realtime-protocol-and-connection.md` §6.1 defines:
//! - `canonical_field = ResumeSession.resume_token`
//! - `client_hello_role = optional_binding_hint`
//! - `comparison = constant_time`
//!   and the five exhaustive cases:
//!   `client_hello_only`, `resume_session_only`, `both_equal`, `both_mismatch`,
//!   `neither`.
//!
//! `mobile-resume-interest-backpressure.md` §4.3 defines resync branching on
//! `instance revision` versus the retained history window.
//!
//! Dependency rule (`repo-crate-conventions.md` §3.2): this module depends only
//! on `domain` (via [`orbisync_domain::Revision`]) and local `state`; it does
//! not depend on `world-runtime` or `interest`.

use orbisync_domain::Revision;

use crate::state::ConnectionState;

// ---------------------------------------------------------------------------
// Constant-time token comparison
// ---------------------------------------------------------------------------

/// Constant-time equality for resume tokens.
///
/// The contract (`contracts/realtime-resume-token-policy.json`) requires
/// `comparison = constant_time` when both `ClientHello.resume_token` and
/// `ResumeSession.resume_token` are present. This function compares the raw
/// bytes without short-circuiting on the first differing byte, so the timing
/// does not leak the position of a mismatch.
///
/// Length is not hidden: a length mismatch returns `false` immediately. Resume
/// tokens are opaque random strings of fixed entropy (MRIB-02), so length
/// alone is not a secret. The byte loop itself remains constant-time for
/// equal-length inputs.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    constant_time_eq_bytes(a.as_bytes(), b.as_bytes())
}

/// Constant-time equality for byte slices.
#[must_use]
pub fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Returns `true` when the optional token is considered present.
///
/// The proto field `resume_token` defaults to `""`; an empty string is treated
/// as absent, matching the contract's `client_hello_present` /
/// `resume_session_present` booleans.
#[must_use]
pub fn is_token_present(token: Option<&str>) -> bool {
    token.is_some_and(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Resume decision table (§6.1)
// ---------------------------------------------------------------------------

/// The five exhaustive cases from `realtime-resume-token-policy.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResumeCase {
    /// `client_hello_only`: hint present, no `ResumeSession` request.
    ClientHelloOnly,
    /// `resume_session_only`: no hint, `ResumeSession` present.
    ResumeSessionOnly,
    /// `both_equal`: both present and equal (constant-time).
    BothEqual,
    /// `both_mismatch`: both present but differ.
    BothMismatch,
    /// `neither`: neither present.
    Neither,
}

impl ResumeCase {
    /// Contract spelling (`case` field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClientHelloOnly => "client_hello_only",
            Self::ResumeSessionOnly => "resume_session_only",
            Self::BothEqual => "both_equal",
            Self::BothMismatch => "both_mismatch",
            Self::Neither => "neither",
        }
    }
}

/// High-level resume decision derived from the case table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResumeDecision {
    /// Do not enter `resuming` (`next_state = ready`).
    DoNotStartResume,
    /// Accept the resume request (`next_state = resuming`).
    AcceptResumeRequest,
    /// Reject the resume request with `resume_token_mismatch`
    /// (`next_state = ready`).
    RejectResume,
}

impl ResumeDecision {
    /// Contract spelling (`decision` field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DoNotStartResume => "do_not_start_resume",
            Self::AcceptResumeRequest => "accept_resume_request",
            Self::RejectResume => "reject_resume",
        }
    }
}

/// Full outcome of the resume token decision table, mirroring the contract
/// fields `case`, `decision`, `next_state`, `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeOutcome {
    /// Which of the five cases matched.
    pub case: ResumeCase,
    /// Derived decision.
    pub decision: ResumeDecision,
    /// Next connection state (`ready` or `resuming`).
    pub next_state: ConnectionState,
    /// Error code when `decision == RejectResume`.
    pub error_code: Option<&'static str>,
}

/// Canonical error code for the `both_mismatch` case.
pub const RESUME_TOKEN_MISMATCH: &str = "resume_token_mismatch";

/// Evaluates the resume token decision table.
///
/// `client_hello_token` is `ClientHello.resume_token` (hint, optional).
/// `resume_session_token` is `ResumeSession.resume_token` (normative).
/// Empty strings are treated as absent.
///
/// Comparison of the two tokens when both are present uses
/// [`constant_time_eq`].
#[must_use]
pub fn decide_resume(
    client_hello_token: Option<&str>,
    resume_session_token: Option<&str>,
) -> ResumeOutcome {
    let ch_present = is_token_present(client_hello_token);
    let rs_present = is_token_present(resume_session_token);

    match (ch_present, rs_present) {
        (true, false) => ResumeOutcome {
            case: ResumeCase::ClientHelloOnly,
            decision: ResumeDecision::DoNotStartResume,
            next_state: ConnectionState::Ready,
            error_code: None,
        },
        (false, true) => ResumeOutcome {
            case: ResumeCase::ResumeSessionOnly,
            decision: ResumeDecision::AcceptResumeRequest,
            next_state: ConnectionState::Resuming,
            error_code: None,
        },
        (false, false) => ResumeOutcome {
            case: ResumeCase::Neither,
            decision: ResumeDecision::DoNotStartResume,
            next_state: ConnectionState::Ready,
            error_code: None,
        },
        (true, true) => {
            // Both present: constant-time comparison decides equal vs mismatch.
            let a = client_hello_token.unwrap_or("");
            let b = resume_session_token.unwrap_or("");
            let equal = constant_time_eq(a, b);
            if equal {
                ResumeOutcome {
                    case: ResumeCase::BothEqual,
                    decision: ResumeDecision::AcceptResumeRequest,
                    next_state: ConnectionState::Resuming,
                    error_code: None,
                }
            } else {
                ResumeOutcome {
                    case: ResumeCase::BothMismatch,
                    decision: ResumeDecision::RejectResume,
                    next_state: ConnectionState::Ready,
                    error_code: Some(RESUME_TOKEN_MISMATCH),
                }
            }
        }
    }
}

/// Convenience wrapper that works directly on owned strings (proto fields).
#[must_use]
pub fn decide_resume_from_strings(
    client_hello_token: &str,
    resume_session_token: &str,
) -> ResumeOutcome {
    let ch = if client_hello_token.is_empty() {
        None
    } else {
        Some(client_hello_token)
    };
    let rs = if resume_session_token.is_empty() {
        None
    } else {
        Some(resume_session_token)
    };
    decide_resume(ch, rs)
}

// ---------------------------------------------------------------------------
// Resync / replay decision (§4)
// ---------------------------------------------------------------------------

/// Reason for a `ResyncRequired` outcome (`ResyncRequired.reason_code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResyncReason {
    /// `last_revision + 1 < oldest_retained` — history gap.
    Gap,
    /// `last_revision > current_revision` — client ahead of server.
    FutureRevision,
    /// No retained history available.
    HistoryUnavailable,
}

impl ResyncReason {
    /// Wire reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gap => "revision_gap",
            Self::FutureRevision => "future_revision",
            Self::HistoryUnavailable => "history_unavailable",
        }
    }
}

/// Retained history window for an instance.
///
/// `current` is the current instance revision. `oldest_retained` is the
/// smallest revision still available for replay (`None` means no history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryWindow {
    /// Current instance revision (`instance_runtime` authoritative).
    pub current: Revision,
    /// Oldest retained revision inclusive, or `None`.
    pub oldest_retained: Option<Revision>,
}

impl HistoryWindow {
    /// Creates a window with known bounds.
    #[must_use]
    pub const fn new(current: Revision, oldest_retained: Option<Revision>) -> Self {
        Self {
            current,
            oldest_retained,
        }
    }
}

/// Outcome of the §4.3 branching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncOutcome {
    /// History contains `(last_revision, current]` — replay it.
    /// `from` is `last_revision + 1` (first revision to replay, inclusive).
    Replay {
        /// First revision to replay (inclusive).
        from: Revision,
        /// Current revision (end of replay, inclusive).
        current: Revision,
    },
    /// History missing or gap — client must resync via Snapshot.
    ResyncRequired {
        /// Why resync is required.
        reason: ResyncReason,
        /// Current revision to include in `ResyncRequired`.
        current: Revision,
    },
}

/// Decides between replay and resync per §4.3.
///
/// Rules (mirroring `mobile-resume-interest-backpressure.md` §4.3):
/// - `oldest_retained == None` → `ResyncRequired(history_unavailable)`
/// - `last_applied > current` → `ResyncRequired(future_revision)`
/// - `last_applied + 1 < oldest_retained` → `ResyncRequired(gap)`
/// - otherwise → `Replay { from = last_applied + 1, current }`
///
/// `last_applied` is `ResumeSession.last_applied_revision`. The check uses
/// `last + 1 < oldest` (not `last < oldest`) so a client that is exactly one
/// revision behind the window still replays (`1200` with window `1201..1230`
/// replays `1201..1230`, matching §4.1 example).
#[must_use]
pub fn decide_resync(last_applied: Revision, window: HistoryWindow) -> ResyncOutcome {
    let Some(oldest) = window.oldest_retained else {
        return ResyncOutcome::ResyncRequired {
            reason: ResyncReason::HistoryUnavailable,
            current: window.current,
        };
    };

    if last_applied > window.current {
        return ResyncOutcome::ResyncRequired {
            reason: ResyncReason::FutureRevision,
            current: window.current,
        };
    }

    // `last + 1 < oldest` means gap. Use checked_add to handle u64::MAX.
    let next = last_applied.as_u64().checked_add(1);
    match next {
        None => {
            // last == u64::MAX and not > current (so current == MAX) implies
            // the client is at the tip with no next revision to replay.
            // Treat as replay with no work rather than gap.
            // If oldest is somehow > last+1 (impossible due to overflow), gap.
            // Since next overflowed, last == MAX == current, so replay empty.
            ResyncOutcome::Replay {
                from: last_applied,
                current: window.current,
            }
        }
        Some(next_u64) => {
            if next_u64 < oldest.as_u64() {
                ResyncOutcome::ResyncRequired {
                    reason: ResyncReason::Gap,
                    current: window.current,
                }
            } else {
                ResyncOutcome::Replay {
                    from: Revision::from_u64(next_u64),
                    current: window.current,
                }
            }
        }
    }
}

/// Convenience `u64` overload for callers working directly with proto fields.
#[must_use]
pub fn decide_resync_u64(
    last_applied: u64,
    current: u64,
    oldest_retained: Option<u64>,
) -> ResyncOutcome {
    decide_resync(
        Revision::from_u64(last_applied),
        HistoryWindow::new(
            Revision::from_u64(current),
            oldest_retained.map(Revision::from_u64),
        ),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::state::ConnectionState;

    // -- constant-time eq ----------------------------------------------------

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq_bytes(b"token123", b"token123"));
    }

    #[test]
    fn test_constant_time_eq_mismatch_same_length() {
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("token1", "token2"));
    }

    #[test]
    fn test_constant_time_eq_different_length() {
        assert!(!constant_time_eq("short", "longer"));
        assert!(!constant_time_eq("a", ""));
    }

    #[test]
    fn test_is_token_present() {
        assert!(!is_token_present(None));
        assert!(!is_token_present(Some("")));
        assert!(is_token_present(Some("x")));
    }

    // -- 5 decision cases (contract) -----------------------------------------

    #[test]
    fn test_case_neither() {
        let out = decide_resume(None, None);
        assert_eq!(out.case, ResumeCase::Neither);
        assert_eq!(out.decision, ResumeDecision::DoNotStartResume);
        assert_eq!(out.next_state, ConnectionState::Ready);
        assert_eq!(out.error_code, None);
        assert_eq!(out.case.as_str(), "neither");
        assert_eq!(out.decision.as_str(), "do_not_start_resume");
    }

    #[test]
    fn test_case_client_hello_only() {
        let out = decide_resume(Some("hint"), None);
        assert_eq!(out.case, ResumeCase::ClientHelloOnly);
        assert_eq!(out.decision, ResumeDecision::DoNotStartResume);
        assert_eq!(out.next_state, ConnectionState::Ready);
        assert_eq!(out.error_code, None);
    }

    #[test]
    fn test_case_client_hello_only_empty_resume_is_not_present() {
        // Proto default "" counts as absent.
        let out = decide_resume(Some("hint"), Some(""));
        assert_eq!(out.case, ResumeCase::ClientHelloOnly);
    }

    #[test]
    fn test_case_resume_session_only() {
        let out = decide_resume(None, Some("tok"));
        assert_eq!(out.case, ResumeCase::ResumeSessionOnly);
        assert_eq!(out.decision, ResumeDecision::AcceptResumeRequest);
        assert_eq!(out.next_state, ConnectionState::Resuming);
        assert_eq!(out.error_code, None);
    }

    #[test]
    fn test_case_both_equal() {
        let token = "opaque-random-token-xyz";
        let out = decide_resume(Some(token), Some(token));
        assert_eq!(out.case, ResumeCase::BothEqual);
        assert_eq!(out.decision, ResumeDecision::AcceptResumeRequest);
        assert_eq!(out.next_state, ConnectionState::Resuming);
        assert_eq!(out.error_code, None);
    }

    #[test]
    fn test_case_both_mismatch() {
        let out = decide_resume(Some("token-a"), Some("token-b"));
        assert_eq!(out.case, ResumeCase::BothMismatch);
        assert_eq!(out.decision, ResumeDecision::RejectResume);
        assert_eq!(out.next_state, ConnectionState::Ready);
        assert_eq!(out.error_code, Some(RESUME_TOKEN_MISMATCH));
        assert_eq!(out.error_code, Some("resume_token_mismatch"));
    }

    #[test]
    fn test_case_both_mismatch_different_lengths() {
        let out = decide_resume(Some("short"), Some("longer-token"));
        assert_eq!(out.case, ResumeCase::BothMismatch);
        assert_eq!(out.decision, ResumeDecision::RejectResume);
    }

    #[test]
    fn test_decide_from_strings() {
        // Both empty => neither
        assert_eq!(decide_resume_from_strings("", "").case, ResumeCase::Neither);
        // Both same => both_equal
        assert_eq!(
            decide_resume_from_strings("t", "t").case,
            ResumeCase::BothEqual
        );
        // Mismatch
        assert_eq!(
            decide_resume_from_strings("a", "b").case,
            ResumeCase::BothMismatch
        );
    }

    // -- resync --------------------------------------------------------------

    #[test]
    fn test_resync_within_retained_range_replay() {
        // last=1220, current=1230, oldest=1201 => replay 1221..1230
        let out = decide_resync_u64(1220, 1230, Some(1201));
        match out {
            ResyncOutcome::Replay { from, current } => {
                assert_eq!(from.as_u64(), 1221);
                assert_eq!(current.as_u64(), 1230);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_exactly_at_oldest_boundary_replay() {
        // Example from §4.1: last=1200, retains 1201..1230 => replay
        let out = decide_resync_u64(1200, 1230, Some(1201));
        match out {
            ResyncOutcome::Replay { from, current } => {
                assert_eq!(from.as_u64(), 1201);
                assert_eq!(current.as_u64(), 1230);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_gap_beyond_retained_requires_resync() {
        // last=1000, oldest=1201 => gap
        let out = decide_resync_u64(1000, 1230, Some(1201));
        match out {
            ResyncOutcome::ResyncRequired { reason, current } => {
                assert_eq!(reason, ResyncReason::Gap);
                assert_eq!(reason.as_str(), "revision_gap");
                assert_eq!(current.as_u64(), 1230);
            }
            other => panic!("expected ResyncRequired, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_future_revision_requires_resync() {
        // last > current
        let out = decide_resync_u64(1240, 1230, Some(1201));
        match out {
            ResyncOutcome::ResyncRequired { reason, .. } => {
                assert_eq!(reason, ResyncReason::FutureRevision);
            }
            other => panic!("expected ResyncRequired, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_no_history_requires_resync() {
        let out = decide_resync_u64(1200, 1230, None);
        match out {
            ResyncOutcome::ResyncRequired { reason, .. } => {
                assert_eq!(reason, ResyncReason::HistoryUnavailable);
            }
            other => panic!("expected ResyncRequired, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_up_to_date_replay_empty() {
        // last == current => replay with from = current+1 (empty range) still Replay
        // Some implementations treat this as no-op replay rather than resync.
        let out = decide_resync_u64(1230, 1230, Some(1201));
        match out {
            ResyncOutcome::Replay { from, current } => {
                assert_eq!(from.as_u64(), 1231);
                assert_eq!(current.as_u64(), 1230);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn test_resync_gap_one_behind_window() {
        // last=1199, oldest=1201 => next=1200 <1201 => gap
        let out = decide_resync_u64(1199, 1230, Some(1201));
        assert!(matches!(
            out,
            ResyncOutcome::ResyncRequired {
                reason: ResyncReason::Gap,
                ..
            }
        ));
    }

    #[test]
    fn test_resync_typed_api() {
        let window = HistoryWindow::new(Revision::from_u64(1230), Some(Revision::from_u64(1201)));
        let out = decide_resync(Revision::from_u64(1220), window);
        assert!(matches!(out, ResyncOutcome::Replay { .. }));
    }
}
