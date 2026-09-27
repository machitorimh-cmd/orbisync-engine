# ADR-027: Public entity ownership transfer

- Status: Accepted

## Context

`InstanceCommand::TransferOwnership` and the
`entity.ownership_transferred` extension event already exist, but no public
transport can request a transfer. Publishing the command without deciding its
authorization, target validation, idempotency, delivery and audit semantics
would let clients invent owner state or create entities whose owner cannot use
them.

## Decision

The canonical public request is the reliable WebSocket `EntityCommand` with:

- `operation = "transfer_ownership"`;
- the current entity revision in `expected_revision`; and
- exactly one argument, `new_owner_id`, containing a canonical UUIDv7
  `UserId` string.

No REST mutation is added. Entity mutations remain ordered by the instance
actor, and this operation inherits the existing instance-scoped `command_id`
deduplication contract: the same ID and payload replays the retained result,
while the same ID with another payload fails with `COMMAND_ID_CONFLICT`.

Authorization and validation are server authoritative:

- the current owner may transfer an owned entity when they have
  `entity.update.own`;
- a caller with `entity.update.any` may transfer an entity owned by another
  user;
- ownerless entities remain server-only and cannot be claimed through this
  public command;
- the target must have a live presence in the same instance when the actor
  applies the command, otherwise it fails with `TARGET_NOT_PRESENT`;
- clearing ownership is not exposed publicly; and
- transferring to the current owner fails with `OWNER_UNCHANGED` and does not
  advance a revision.

The actor applies the optimistic revision check and, on success, advances the
entity revision. Its batched persistence effect updates the durable owner and
inserts the successful `entity.ownership_transferred` administrative audit
record in the same PostgreSQL transaction. The existing extension event is
also emitted for webhook delivery. The reliable result is filtered using the
post-transfer entity view, but the previous and new owners always receive the
transfer result so an `owner_only` hand-off cannot hide its acknowledgement
from either party.

## Consequences

Offline assignment is deliberately unsupported in v1. It can be added later
as a separate administrative REST use case with its own permission and target
account lifecycle rules, without weakening the realtime command. Clients must
retain the original `command_id` when retrying and refresh state after a
revision conflict.
