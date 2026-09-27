# ADR-013: 生成コード方針と受入条件

- Status: Accepted
- Date: 2026-08-03
- Decision Owners: avistoria

## Context

ADR-004 は Protocol Buffers の生成コードを repository に commit せず、Rust は build 時に `OUT_DIR`、SDK 向け生成物は ignore された作業用 directory へ出力すると決定した。一方、`repo-crate-conventions.md` §4.3 と §8.2 の条件 6・7 は commit 済み生成物との再生成差分を前提としており、追跡対象の生成コードが存在しない現在の方式には適用できない。

CI には `buf lint`、base branch の `proto/` に対する `buf breaking`、両 code generator を用いた `buf generate` と出力先検査が既に存在する。受入条件をこの実効 gate と一致させる必要がある。

## Decision

- `.proto` は引き続き公開 protocol 契約の正本とし、生成コードは commit しない。
- 条件 6 は次のすべてが CI で pass することとする。
  1. `buf lint` が protocol 規約違反を検出する。
  2. `buf breaking --against <base>:proto` が base branch に対する後方互換性の破壊を検出する。
  3. repository の設定と同じ plugin で `buf generate` を実行した後、tracked file の差分がゼロであり、ignore されない path の untracked 生成物もゼロである。
- 条件 7 は、生成コードが repository に tracked file として存在せず、build または code generation のたびに正本から生成されることとする。したがって生成コードへの手編集は保存・review・merge の対象にならず、構造的に防止される。
- `generated/` のように明示的に ignore された作業用出力は許容する。出力先を `sdk/` 等の非 ignore path へ誤変更した場合は CI を失敗させる。
- OpenAPI は人手で管理する正本であり、この ADR の Protocol Buffers code generation gate の対象外とする。OpenAPI 自身の validation と compatibility policy は別の gate で扱う。

## Alternatives

### 生成コードを commit する

再生成差分を単純に比較できるが、ADR-004 に反し、生成物の review noise、複数言語生成物の同期、手編集混入を招く。

### `buf lint` と `buf breaking` のみを使う

schema の規約と互換性は検出できるが、plugin 設定の破損や生成先の誤変更を検出できない。

### build 時生成だけに依存する

Rust の生成可否は確認できるが、SDK generator と non-ignored path への意図しない出力を検出できない。

## Consequences

- 受入条件が ADR-004 と実際の CI に一致する。
- protocol の規約、互換性、generator の実行可能性、出力先逸脱をそれぞれ検出できる。
- CI は code generation plugin を用意する必要があり、cache がない場合は時間が増える。
- ignore された生成物の内容そのものを commit 済み snapshot と比較する gate ではない。互換性の正本は `.proto` と base branch との比較である。

## Migration

1. `repo-crate-conventions.md` §4.3 と §8.2 条件 6・7を本決定に合わせる。
2. required PR CI で両 code generator、tracked diff、non-ignored untracked file の検査を維持する。
3. 新しい generator または出力先を追加するときは同じ gate の対象へ含める。
