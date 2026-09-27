# ADR-023: Animation and presence payloads

- Status: Accepted

## Context

The design specification defines the names of the core standard components but
does not define payload schemas for `core.animation` and `core.presence`.

## Decision

Following the orchestrator decision for F-5, `core.animation` contains a clip
name (1–64 ASCII lowercase/digit/dot/hyphen/underscore bytes), playback time in
seconds, finite playback speed, and a loop flag. `core.presence` contains one
of `online`, `away`, or `busy`, plus an i64 Unix timestamp in milliseconds.
Both are transient state, use the transform/velocity state-delta delivery path,
and are excluded from checkpoints. Animation intentionally has no blending or
layers; presence intentionally has no gaze or speech state.

This decision is an implementation of the currently unspecified schema and
must be revisited when the formal specification defines these payloads.
