# ADR-020: Namespace-separated bootstrap roles

- Status: Accepted
- Date: 2026-08-29
- Decision Owners: avistoria

## Context

AA-04 separates administration permissions from world and entity permissions.
The original bootstrap role granted both `admin.*` and world permissions,
which made the bootstrap path an exception to that boundary.

## Decision

1. Bootstrap creates two roles: `Administrator` contains the existing
   `admin.*` permissions, and `World Administrator` contains
   `world.instance.create`, `entity.spawn`, `entity.update.own`, and
   `entity.update.any`.
2. The bootstrap user receives both role assignments in the same identity
   transaction as the user, credential, role, permission, and audit inserts.
3. Permission names are restricted to the supported `admin`, `world`,
   `entity`, and `moderation` namespaces. A role may not mix `admin.*` with a
   world-side namespace.

## Rationale

The role names preserve the existing `Administrator` label for compatibility
with operators and make the second role's world scope explicit. Keeping the
two role IDs separate makes the namespace boundary visible in both storage
and authorization while preserving the bootstrap user's effective access.

## Consequences

- Bootstrap authorization continues to provide all existing administrative
  and world capabilities through the union of two roles.
- New roles cannot accidentally combine management API permissions with
  world-instance permissions.
- Existing persisted roles are read as stored; the construction boundary
  prevents new mixed roles without requiring a destructive migration.
