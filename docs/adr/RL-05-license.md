# RL-05: MIT OR Apache-2.0 デュアルライセンス

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

OrbiSyncをオープンソースとして公開する前に、仕様 §36と仕様 §42.2が候補・未決事項として示すライセンスを確定する必要がある。

Rustエコシステムとの整合、個人・企業による利用のしやすさ、明示的な特許ライセンスを選択できることを重視する。

## Decision

OrbiSyncを `MIT OR Apache-2.0` のデュアルライセンスで提供する。

- 利用者はMIT LicenseまたはApache License 2.0のいずれかを選択できる。
- repository rootへ `LICENSE-MIT` と `LICENSE-APACHE` を配置する。
- MITの著作権者表記は `Copyright (c) 2026 avistoria` とする。
- Cargo packageを作成するときは `license = "MIT OR Apache-2.0"` を指定する。
- 特定ファイルを異なる条件で提供する場合は、そのファイル上で明示し、通常のcontributionへ暗黙に適用しない。

## Alternatives

### Apache-2.0単独

明示的な特許条項を持ち企業利用に適するが、MITを希望する利用者へ選択肢を提供できない。

### MIT単独

短く理解しやすいが、Apache-2.0の明示的な特許ライセンス条項を選択できない。

### Copyleft系ライセンス

改変公開を促進できる一方、初期方針である広い採用と組込み・商用利用の容易さに対する制約が大きい。

## Consequences

- Rustプロジェクトで一般的なSPDX expressionを利用できる。
- 利用者は用途に応じてMITまたはApache-2.0を選択できる。
- release artifactには両方のライセンス本文を含める必要がある。
- dependencyのライセンス互換性は別途CIで継続検査する。
- CLA/DCOおよび商標ポリシーはRL-06として未決のまま扱う。

## Migration

1. `LICENSE-MIT`と`LICENSE-APACHE`をrepository rootへ追加する。
2. README、CONTRIBUTING、package metadataへ `MIT OR Apache-2.0` を記載する。
3. 将来作成するCargo packageへ `license = "MIT OR Apache-2.0"` を設定する。
4. release/packaging testで両ライセンス本文の同梱を検証する。
