# ADR-010: Dependency and toolchain policy

- Status: Accepted
- Date: 2026-07-31

## Context

MSRV、依存更新bot、許容license、security advisory対応期限を実装開始前に決める必要がある。

## Recommendation

serverの`Cargo.lock`をcommitし、MSRVはstable releaseの直近2世代を候補とする。RenovateまたはDependabotの一方だけを使用し、major updateは手動review、critical advisoryは48時間以内にtriageする。採用license allowlistと例外承認者は別表で管理する。

## Alternatives

latest stableのみのsupportは保守が単純だが利用者のupgrade猶予がない。無期限MSRVは依存更新を阻害する。

## Decision trigger

最初のRust workspaceとCI toolchainを追加する変更でAcceptedへ移す。

This trigger is satisfied by Milestone 0, which adds the first Rust workspace
and CI toolchain.

## Milestone 0 decision record

- Rust 1.95 is the MSRV and pinned CI toolchain.
- Dependency licenses are denied by default and allowed explicitly in
  `deny.toml`. `CDLA-Permissive-2.0` is allowed because it is a permissive,
  notice-preserving data license used by the Mozilla root-certificate data in
  `webpki-roots`; it does not impose reciprocal source-code terms.
- Workspace path dependencies may omit versions; wildcard version requirements
  from registries remain denied.
- Dependabot is the sole dependency update bot. Minor and patch Cargo updates
  are grouped; major updates require individual review.
- Critical advisories are triaged within 48 hours.
