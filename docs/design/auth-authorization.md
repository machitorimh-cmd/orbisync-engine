# OrbiSync 認証・認可設計

## 0. 表記規則

本書は `metaverse_core_specification.md` §19（認証仕様）と §20（認可仕様）を、アカウント発行、パスワード、セッション/トークン、RBAC、権限名前空間、所有権モデル、認可の実行境界の実装可能な設計へ具体化する。

- **[SPEC] 仕様由来の確定事項**: `metaverse_core_specification.md` 本文で要求または禁止されている事項
- **[REC] 設計上の推奨**: 要求を満たすための設計案。実装前にレビューする
- **[ADR] ADR 待ち**: 複数案が成立し、現時点では確定しない事項
- **設計前提**: 既存の設計文書で合意済みの設計判断。本書は変更せず前提として参照する。仕様本文由来ではないため [SPEC] とは区別する

本書が主担当となる要 ADR 判断は `AA-xx` で管理する。

### 0.1 他設計文書との関係

| 関連事項 | 正本となる文書 | 本書の扱い |
|---|---|---|
| モジュール所有権、DAG、Application coordinator、認証フロー | `architecture.md` §3, §3.1, §5.1 | 前提として参照。所有権を再定義しない |
| User/Credential/Role/Permission 集約、ID 体系、Auth/Realtime/Presence 分離 | `domain-model.md` §2, §3.1, §3.2, §3.6 | 前提として参照。集約定義を繰り返さない |
| REST 認証 API、エラー形式、DTO 変換境界 | `rest-api-persistence.md`、`transport-boundaries.md` §1.1, §3, §4 | 認証の HTTP 表面は REST 書へ委ね、本書は意味論を定義 |
| Argon2id、token 技術選定 | `technology-decisions.md` TD-07, ADR-002 | 前提として参照 |
| ログイン試行制限の閾値 | `domain-model.md` DM-05 | 前提として参照。値を再定義しない |
| ログイン試行制限、ロック期間 | `domain-model.md` DM-05 | 前提として参照 |
| Access Token検証とRealtime ticket発行 | `realtime-protocol-and-connection.md` §1, §6.1、ADR-002 | `validateAccessToken` / `issueRealtimeTicket` / `consumeRealtimeTicket`の意味を本書が定義 |
| Resume Token（realtime 復帰用） | `mobile-resume-interest-backpressure.md` §3 | 本書の Auth Session と分離。再定義しない |

## 1. 所有権と前提

**設計前提** 認証・ユーザー・ロール・権限・Identity Session・token/失効は `identity_access` が所有し、`AuthSessionId` を所有する（`architecture.md` §3）。

**設計前提** Feature module 同士を直接 orchestration させず、Application coordinator が use case の順序を所有する（`architecture.md` §3.1）。認可判定は application 境界で実施する。

**[SPEC]** 全管理操作と状態変更は認証・認可を application 境界で実施する（仕様 §26.3、`architecture.md` §6.1）。

**[SPEC]** クライアントの自己申告（owner、クライアント種別、その他自己申告値）を権限判定に使ってはならない（仕様 §20.3, §15.2）。権限と所有権はサーバーが確定する。

## 2. アカウント発行

**所有モジュール:** `identity_access`

**[SPEC]** 仕様 §19.1 の確定要件：

- 自己登録は提供しない
- 管理者がアカウントを作成する
- メールアドレスは必須ではない

**[REC]** 仕様 §19.1 が MAY/任意とする機能は、初期版で次を採用する：

- CSV 一括作成（MAY）
- 初期パスワードの発行（任意）
- 初回ログイン時のパスワード変更推奨（advisory、サーバー強制なし）— `must_change_password` を初期 `true` とし、サーバーは状態を通知するのみで操作をブロックしない（下記決定を参照）

**[REC]** User 集約と Credential 集約は論理的に分離する（`domain-model.md` §3.1）。アカウント作成は User 集約の生成と、初期 Credential の生成を伴う。

**[REC]** `must_change_password` を初期 `true` とし、初回ログイン時のパスワード変更を推奨する。サーバーはログイン応答の `must_change_password` とユーザー集約のフラグで状態を通知するが、強制はクライアント側の責務とする（advisory）。サーバー側で操作をブロックしない。理由: M1 では監査と基本 RBAC の到達が優先であり、強制は全管理操作に横断的な認可ゲートを追加して複雑性を増し、サービスアカウント等の自動運用で意図せずロックアウトする恐れがある。強制が必要になった場合は `identity_access.authorize` に `must_change_password` 判定を追加して拒否する設計にできる（将来 ADR）。変更完了後に `false` へ更新する。

**[REC]** CSV 一括作成は管理操作として `identity_access` の application service が処理する。各行を検証し、失敗行は集約して報告する。部分成功の扱いは §11 で定める。

**設計前提** ADR-016によりCSV一括作成はUTF-8 `text/csv`、最大1,000行/1 MiB、行単位の部分成功とし、重複は当該行の`RESOURCE_CONFLICT`として集約報告する。

## 3. パスワード

**所有モジュール:** `identity_access`

**[SPEC]** 仕様 §19.2 の確定要件：

- 平文保存禁止
- Argon2id でハッシュ
- 組織ポリシーに応じた最小長
- 管理者も現在のパスワードを閲覧できない

**[REC]** 仕様 §19.2 が MAY とする弱い既知パスワード拒否は、初期版で採用してよい。具体の辞書採用は AA-02 で決める。

**設計前提** パスワード保存には Argon2id を使用する（`technology-decisions.md` TD-07、仕様 §8）。

**[REC]** `PasswordHash` は Value Object として表現し、構築時にハッシュ形式を検証する（`domain-model.md` §3.1）。平文パスワードを domain 層に保持しない。

**[REC]** ハッシュ検証は CPU blocking 処理として隔離する（`technology-decisions.md` TD-07、`spawn_blocking` 等）。async worker を直接ブロックしない。

**[SPEC]** 管理者を含め、いかなる主体も現在のパスワード（平文・ハッシュ）を閲覧できない（仕様 §19.2）。API はパスワードハッシュを返さない（`rest-api-persistence.md` §3 の outbound 変換で除外）。

**[REC]** Argon2id の parameter 変更時に再ハッシュ判定を行う。検証成功時に現行 parameter と一致しなければ、新しい parameter で再ハッシュして保存する。

**[REC]** パスワード変更時に `password_changed_at` を更新する（`domain-model.md` §3.1）。これは rotation の追跡と、変更前の Refresh Token 失効判定に使用する（§5.4）。

**設計前提** ADR-015の実測によりArgon2id v=19、memory 65,536 KiB、time 3、parallelism 1、output 32 bytes、password長12〜128文字とする。version固定の上位10,000弱password denylistと文脈語/単純pattern拒否を採用する。

### 3.1 ログイン試行制限

**設計前提** ログイン試行制限の閾値は `domain-model.md` DM-05（5 回失敗で 15 分ロック）を推奨初期値とする。

**[REC]** `user_credentials.failed_login_count` と `locked_until` でロックを管理する（`domain-model.md` §3.1、仕様 §22.3）。ロック中の認証試行は失敗を返す。

**[SPEC]** 認証失敗の外部応答では ID の存在有無を露出しない（仕様 §26.3、`architecture.md` §5.1）。ロック中・不存在・パスワード誤りのいずれも区別できない応答にする。

## 4. パスワードリセット

**所有モジュール:** `identity_access`

**[SPEC]** メールリセットを前提としない（仕様 §19.3）：

```text
利用者 → 管理者へ連絡
管理者 → 一時パスワード発行
利用者 → 次回ログインで変更
```

**[REC]** 管理者は `POST /v1/users/{user_id}/reset-password`（`rest-api-persistence.md` §2）で一時パスワードを発行する。発行時に `must_change_password = true` へ設定し、次回ログインで変更を推奨する（advisory、§2）。サーバーは状態を通知するのみでブロックしない。

**[REC]** 一時パスワード発行は既存の Credential を無効化し、新しい `PasswordHash` で置き換える。発行操作は監査する（§12）。

## 5. セッションとトークン

**所有モジュール:** `identity_access`（`AuthSessionId` を所有、`domain-model.md` §2）

### 5.1 推奨モデル

**[REC]** 仕様 §19.4 はトークンの推奨モデルを示す（確定要件ではない）。本書はこの推奨モデルを採用する：

- 短命 Access Token
- 長命 Refresh Token
- Refresh Token は DB へハッシュ保存
- Refresh Token rotation
- ログアウト時失効
- ユーザー無効化時に全セッション失効

**設計前提** ADR-002によりAccess Tokenは15分のEd25519署名JWT、Refresh Tokenは30日有効の256-bit opaque tokenとする。browser SDKの既定はmemory保持、native SDKの永続化はOSの安全な資格情報storageを使用する（`docs/adr/ADR-002-authentication-session.md`）。

### 5.2 Auth Session 集約

**[REC]** Identity Session を `AuthSession` 集約として表現する：

```text
Aggregate Root: AuthSession
├── id: AuthSessionId
├── user_id: UserId
├── status: Active / Revoked
├── created_at: Timestamp
├── revoked_at: Option<Timestamp>
└── refresh_token: RefreshToken（Entity、ハッシュ保存）
```

**[REC]** `RefreshToken`はEntityとして`AuthSession`内に保持し、raw tokenではなく用途別secret keyによるHMAC-SHA-256 digestを保存する（仕様 §19.4、ADR-002）。

**[REC]** `auth_sessions` テーブルと `refresh_tokens` テーブルは `identity_access` が所有する（`rest-api-persistence.md` §8、仕様 §22.1）。

### 5.3 Access Token の検証

**[REC]** `identity_access`は`validateAccessToken(token)`を公開し、JWT署名・algorithm・`iss`/`aud`/時刻claimを検証したうえで、`sid`のActiveな`AuthSession`を確認し、認証済み主体（`UserId`）と`AuthSessionId`を返す（ADR-002）。

**[REC]** REST呼び出しはHTTP headerのBearer tokenを`http_api`が抽出し、`validateAccessToken`へ渡す。WebSocket接続はAccess Tokenをupgradeへ渡さず、RESTで発行した単一使用のRealtime接続ticketをClientHelloで検証する（ADR-002、`realtime-protocol-and-connection.md` §6.1）。

**設計前提** ADR-002でAccess Token形式、期限、session status確認、Refresh Token rotation、Realtime接続ticketを確定した（`docs/adr/ADR-002-authentication-session.md`）。

### 5.4 失効と rotation

**[REC]** Refresh Token rotation：refresh のたびに新しい Refresh Token を発行し、旧 token を無効化する。再利用された旧 token を検知した場合は、当該 `AuthSession` を即時失効する（token theft 検知）。

**[REC]** 失効トリガー：

| トリガー | 効果 |
|---|---|
| ログアウト | 当該 `AuthSession` を Revoked |
| ユーザー無効化 | 当該ユーザーの全 `AuthSession` を Revoked（仕様 §19.4） |
| パスワード変更 | 変更以前の `AuthSession` を失効（推奨） |
| Refresh Token 再利用検知 | 当該 `AuthSession` を即時失効 |
| Access Token 期限切れ | refresh が必要。Access Token 自体は短命 |

**[REC]** 推奨モデルに従い、ログアウト時に当該セッションを失効し、ユーザー無効化時に全セッションを失効する（仕様 §19.4 の推奨を採用）。

**設計前提** logout・利用者無効化・refresh reuse時はAuthSession cacheを即時無効化し、以後のAccess Token検証とRealtime ticket発行を拒否する。既存Realtime接続を切断する通知経路はapplication coordinatorが所有する（ADR-002）。

**[REC]** `AuthSessionId` は Realtime の接続キーとして再利用しない。Realtime 接続は `validateAccessToken` の結果（認証済み主体）を用い、Resume Token は `realtime_presence` が別途所有する（`mobile-resume-interest-backpressure.md` §3、`domain-model.md` §3.6）。

## 6. 外部認証 trait

**所有モジュール:** `identity_access`

> **[ADR-026により上書き]** 本節の「外部認証は初期版では実装しない」という `[SPEC]` 記述は、
> [ADR-026](../adr/ADR-026-authentication-method-selection.md) が明示的に上書きした。
> 外部認証（署名JWT検証）は実装され、`auth.methods` に `external` を加えたときのみ有効になる。
> 既定は無効で、ローカル認証の動作・API・スキーマは無変更である。
> 同様に `technology-decisions.md` §8「外部 IdP を採用しない」も ADR-026 の範囲で上書きされる。
> ADR-026 が採る方式は ID Token 相当の署名JWT検証であり、OIDC discovery / code flow は含まない。

**[SPEC→上書き済]** 外部認証は初期版では実装しない。ローカル認証が唯一の公式実装となる（仕様 §19.5）。
→ ADR-026 で実装対象に変更。既定無効・local 互換維持という条件つきで有効化する。

**[REC]** 仕様 §19.5 が任意（MAY）とする trait 境界を定義する：

```rust
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn authenticate(&self, request: AuthRequest) -> Result<AuthIdentity, AuthError>;
    async fn disable_identity(&self, user_id: UserId) -> Result<(), AuthError>;
}
```

> **実装状況の注記**: この trait は本書執筆時点では**コードに存在しなかった**（文書のみ）。
> ADR-026 が port として**新規に定義**し、署名JWT検証 adapter を実装する。

**[REC]** `IdentityProvider` は `identity_access` が定義する port として扱い、Composition Root が実装を注入する（`architecture.md` §4.3）。ローカル認証（パスワード検証）は引き続き既定有効の adapter である。

**[REC→上書き済]** ~~外部 IdP 実装は追加しない~~ → ADR-026 が静的JWKSによる署名検証 adapter を追加する。外部サービスとの契約は不要で、ローカルの署名 issuer で実経路を検証できる。

## 6.1 認証方式の選択（ADR-026）

**[ADR-026]** 認証方式は `auth.methods` で選択・併用する：`local`（既定有効）/ `guest` / `name_only` / `external`（いずれも既定無効）。

**[ADR-026]** 4方式は主体の確定方法だけが異なり、確定後は単一の発行経路に合流する。`AuthSession`・access token・refresh token・Realtime ticket・`WorldAuthorizer`・RBAC・所有権・監査は4方式で同一であり、方式による分岐を持たない。

**[ADR-026]** guest / name-only は `users` 行を持ち `user_credentials` 行を持たない。`find_login` が INNER JOIN であるため、パスワードログイン経路に構造的に到達できない。「永続アカウントなし」は「DBへ一切保存しない」ではない。

**[ADR-026]** 一時主体は `ephemeral_subjects.expires_at` を絶対上限として持ち、反復 refresh で延命できない。期限到達後は失効 job の実行有無に関わらず拒否される。

**[ADR-026]** 期限切れ一時主体は session とロールを失うが、`users` 行は参照用主体として保持する。checkpoint の owner 参照（FK 無し、復元時に検証される）と `persistent_entities.owner_id` を壊さないためであり、物理削除は行わない。

**[ADR-026]** 一時主体の参加先は `ephemeral_subjects.allowed_worlds`（発行時 snapshot）で制限する。判定は instance から解決した world に対して行い、join / resume / command 受理の共通経路に置く。local / external の world 参加制限は対象外である。

**[ADR-026]** 一時主体のロールに含めてよい permission は Core 固定の allowlist（`world.instance.read` / `entity.spawn` / `entity.update.own` / `entity.update.any`）に限る。`admin.*`・`world.instance.create|start|stop`・`moderation.kick`・未知 permission を含むロールは起動失敗とする。allowlist は上限であって付与内容ではない。

**[ADR-026]** owner 専用操作の強制は Core の RBAC ではなく ADR-025 の pre-commit 外部 rule が担う。`entity.update.any` を持つ主体が他人の entity を更新できるのは RBAC の設計どおりの挙動である。

## 7. RBAC

**所有モジュール:** `identity_access`

**[SPEC]** 初期版は Role-Based Access Control を採用する（仕様 §20.1）：

- User は複数 Role を持てる
- Role は複数 Permission を持つ
- Permission は名前空間付き文字列

**[REC]** Deny rule は初期版では持たず、allow-only とする（仕様 §20.1 は allow-only を推奨）。

**[REC]** Role / Permission の集約構造は `domain-model.md` §3.2 に従う：

```text
Entity: Role
├── id: RoleId
├── name: String
├── permissions: Vec<Permission>
└── description: Option<String>

Value Object: Permission
└── 名前空間付き文字列
```

**[REC]** 有効権限の評価：subject の有効 Permission 集合は、その User に割当された全 Role の Permission の和集合とする。Deny rule を持たないため、いずれかの Role が許可すれば許可とする。

**[REC]** 権限評価は `identity_access.authorize(subject, permission)` として公開する。coordinator は管理操作の前にこれを呼び、結果に基づいて use case の実行可否を決める（§10）。

**[ADR]** 権限評価結果のキャッシュ（TTL、失効時の無効化）は AA-03 で決める。

## 8. 権限名前空間

**[SPEC]** 管理 API 権限とワールド内権限を分離する（仕様 §20.2）。例：

```text
admin.users.create
admin.audit.read
world.join
world.instance.create
entity.spawn
entity.update.own
entity.update.any
moderation.kick
```

**[REC]** Permission 文字列は `<resource>.<action>` または `<resource>.<action>.<scope>` の形式とする（`domain-model.md` §3.2）。`admin.*` を管理 API 権限、`world.*` / `entity.*` / `moderation.*` をワールド内権限の命名空間として用いる。

**[REC]** 用途固有ロール（Student、Teacher、Employee 等）をコアへ埋め込まない（仕様 §9.2、`domain-model.md` §3.2）。コアは権限文字列のみを扱い、ロールの意味は運用が定義する。

**[ADR]** 管理 API 権限とワールド内権限の命名空間を統合するか分離するかは AA-04 で決める（仕様 §20.2、`domain-model.md` §3.2 の認可 ADR）。本書は初期案として `admin.*` と `world.*/entity.*/moderation.*` の分離を推奨する。

## 9. 所有権モデル

**所有モジュール:** `instance_runtime`（Entity 集約、`domain-model.md` §3.5）

**[SPEC]** エンティティは owner を持てる（仕様 §20.3）：

- owner のみ更新可能
- 任意権限保持者は更新可能（`entities.update.any`）
- サーバーのみ更新可能
- 所有権移譲イベントを監査可能

**[SPEC]** クライアントの自己申告 owner を信用しない。サーバーが所有権を確定する（仕様 §20.3、`domain-model.md` §3.5）。

**[REC]** Entity 集約の `owner: Option<UserId>` と `visibility` で所有・可視性を表現する（`domain-model.md` §3.5）。更新可否の判定順序：

1. サーバーのみ更新可能なエンティティは、クライアント更新をすべて拒否する
2. `entity.update.any` 権限保持者は更新可能
3. `owner` が設定されているエンティティは、owner（または `entity.update.own` 権限を持つ owner）のみ更新可能
4. いずれにも該当しなければ拒否する

**[REC]** 所有権検証は `instance_runtime` の command 処理内で行う（`state-and-runtime.md` §2.4）。coordinator は所有権判定を `instance_runtime` に委譲し、transport 層で先取りしない。

**[REC]** 所有権移譲は `OwnershipTransferred` ドメインイベントを発行し、監査可能にする（`domain-model.md` §5、仕様 §20.3）。

## 10. 認可の実行境界

**[SPEC]** 全管理操作と状態変更は認証・認可を application 境界で実施する（仕様 §26.3、`architecture.md` §6.1）。

**[REC]** 認可は 2 系統で実行する：

| 認可 | 判定箇所 | 所有モジュール | 入力 |
|---|---|---|---|
| 管理 API 権限 | coordinator → `identity_access.authorize` | `identity_access` | subject + `admin.*` permission |
| ワールド内権限・所有権 | coordinator → `instance_runtime` command 検証 | `instance_runtime` | subject + entity + action |

**[REC]** coordinator は use case 実行前に認可を完了させる。認可失敗は use case を実行せず、期待された入力拒否として扱う（`architecture.md` §6）。

**設計前提** transport 層は認可を行わない。transport 層は構文検証（サイズ、形式、version）のみを行い、意味的認可は application/domain 層が実施する（`state-and-runtime.md` §2.2、`realtime-protocol-and-connection.md` §5.2）。

**[REC]** インスタンス参加中の認可（kick、role change 等）は、`realtime_presence` の binding 情報ではなく、`identity_access` の権限評価と `instance_runtime` の所有権判定に基づく。binding は認証済み主体を保持するのみである（`mobile-resume-interest-backpressure.md` §0.2）。

## 11. 脅威モデルと失敗時挙動

**[REC]** 本書が対象とする脅威と対策：

| 脅威 | 対策 | 根拠 |
|---|---|---|
| credential stuffing / brute force | ログイン試行制限・ロック | §3.1、DM-05 |
| ユーザー enumeration | 認証失敗応答で ID 存在有無を露出しない | 仕様 §26.3、§3.1 |
| パスワード漏洩 | 平文保存禁止、Argon2id、管理者も閲覧不可、ログへ出さない | 仕様 §19.2, §26.4 |
| token 盗用 | 短命 Access Token、Refresh Token rotation・再利用検知、失効 | 仕様 §19.4、§5.4 |
| 権限昇格 | サーバー側認可、自己申告を信用しない | 仕様 §20.3, §26.3 |
| セッション固定/再利用 | ログアウト/無効化時の失効、パスワード変更時の失効 | 仕様 §19.4、§5.4 |
| secret のログ混入 | token/password/hash をログ・telemetry へ記録しない | 仕様 §26.4、TD-08 |

**[REC]** 失敗時挙動：

| 失敗 | 応答 | 状態変化 |
|---|---|---|
| 認証失敗（パスワード誤り/不存在/ロック） | 401（区別不能） | failed_login_count 増加、ロック判定 |
| 認可失敗（権限不足） | 403 | なし |
| token 無効/期限切れ | 401 | refresh を促す |
| ドメイン検証失敗（パスワードポリシー等） | 422 / 400 | なし |
| CSV 一括作成の部分失敗 | 成功行を反映、失敗行を集約報告 | 成功行のみ永続化 |
| DB 障害 | 500（詳細非公開） | 永続更新を成功扱いしない |

**[SPEC]** 認証失敗の外部応答では ID 存在有無を露出しない（仕様 §26.3）。Transport/DB エラーは公開エラーコードへ変換し、内部エラーを露出しない（仕様 §21.8、`architecture.md` §6）。

**設計前提** ADR-016によりCSV一括作成は部分成功とする。各行は `identity_access` の local transaction で原子に処理し、入力順の結果を集約する。

## 12. 監査

**設計前提** 管理操作と状態変更は監査する（`architecture.md` §1.2、`domain-model.md` §5）。

**[REC]** 本書が対象とする監査対象イベント：

| イベント | 発行元 | 監査対象 |
|---|---|---|
| ログイン成功/失敗 | `identity_access` | actor, source_ip, result |
| ログアウト | `identity_access` | actor, session |
| パスワード変更/リセット | `identity_access` | actor, target_user |
| User 作成/有効化/無効化 | `identity_access` | actor, target_user |
| Role 割当/剥奪 | `identity_access` | actor, target_user, role |
| 所有権移譲 | `instance_runtime` | actor, entity, old/new owner |

**[REC]** 監査記録は `audit_observability` の audit sink port 経由で `audit_events` テーブルへ保存する（`rest-api-persistence.md` §8、仕様 §22.5）。`identity_access` は audit port を呼ぶが、`audit_observability` の実装へ直接依存しない（`architecture.md` §3）。

**[SPEC]** 監査ログへパスワード、token、機密情報を含めない（仕様 §26.4、TD-08）。

## 13. テスト可能な受入条件

**[REC]** 実装は次の受入条件をテストで示す。fake adapter と決定論的 clock を用いる（`architecture.md` §9、仕様 §31.4）。

1. 自己登録の経路が存在しない。アカウントは管理者操作（単一または CSV）でのみ作成される。
2. 作成された User は `must_change_password = true` であり、初回パスワード変更完了後に `false` となる。
3. パスワードは Argon2id ハッシュで保存され、平文が DB・API 応答・ログのいずれにも現れない。管理者向け API もパスワードハッシュを返さない。
4. 存在しない login_id と誤りパスワードで、同一形状の 401 応答を返す（enumeration 不能）。
5. 連続失敗が DM-05 の閾値に達するとロックされ、ロック中の認証は失敗する。
6. login成功で15分のEd25519署名Access Tokenと30日のopaque Refresh Tokenを発行し、Refresh TokenはHMAC-SHA-256 digestで保存する（`POST /v1/auth/login` は `access_token` と `refresh_token` を返す。`POST /v1/auth/refresh` も実装済み）。
7. `/v1/auth/refresh` は新しいRefresh Tokenを発行し旧tokenを消費する。消費済みtokenの再提示は拒否され、同じsession familyが即時失効する。
8. `/v1/auth/logout` で当該AuthSessionが失効し、そのAccess Tokenは以後の検証に失敗する。
9. User無効化で当該ユーザーの全AuthSessionが失効する。
10. Realtime接続ticketは60秒で期限切れとなり、1回のatomic consume後の再利用が拒否される。
11. ticket検証前のWebSocketはClientHello以外を処理せず、5秒timeoutで閉じる。
12. 権限のない主体の管理操作は403を返し、use caseは実行されない。
13. `entity.update.any`を持たない非ownerは、owner付きEntityを更新できない。clientが自己申告したownerは無視され、server確定のownerが用いられる。
14. 所有権移譲は`OwnershipTransferred` eventを発行し、監査記録に残る。
15. token/ticket/password/hashがlog、metric、trace、telemetryに含まれない。

## 14. 要 ADR 事項

本書が主担当となる判断を AA ID で管理する。他文書が正本の判断（ADR-002、DM-05、TD-07 等）は再定義せず参照のみ行う。

| ID | 判断事項 | 推奨案 | 根拠 |
|---|---|---|---|
| AA-01 | CSV 一括作成の上限・部分成功・報告形式 | **ADR-016で解決:** 最大1,000行/1 MiB、部分成功、行別結果 | 仕様 §19.1 は MAY。運用負荷と原子性の均衡 |
| AA-02 | Argon2id parameter・最小長・弱パスワード辞書 | **ADR-015で解決:** 64 MiB/t=3/p=1、12〜128文字、version固定denylist | 仕様 §19.2、TD-07。実測で確定 |
| AA-03 | 権限評価結果のキャッシュ | 短 TTL + 失効/割当変更時の無効化 | 評価負荷の低減。即時性と性能の均衡 |
| AA-04 | 管理 API 権限とワールド内権限の命名空間 | `admin.*` と `world.*/entity.*/moderation.*` の分離 | 仕様 §20.2、domain-model §3.2。認可境界の明確化 |

Access Token の形式・期限・失効モデルは ADR-002、ログイン試行制限の閾値は DM-05 が正本であるため、本書の AA ID では管理しない。
