# ADR-026: 認証方式の選択（アカウント／ゲスト／名前のみ／外部認証）

- Status: Accepted
- Date: 2026-09-16
- Decision Owners: avistoria

## Context

`docs/design/generalization-and-llm-app-platform.md` §2.3は、OrbiSyncを汎用アプリ基盤として使う場合に、常にアカウント登録を要求する現状が制約になることを指摘している。共同編集ツールの一時的な招待リンク参加や、シミュレーターの体験版のように、ゲスト参加や表示名のみでの参加が適した用途がある。

コード読解により現状を確認した。文書の記述と実装は一致していない箇所がある。

- **`IdentityProvider` traitは実装に存在しない。** `auth-authorization.md` §6と仕様§19.5のコードブロック内にあるだけで、`crates/` には定義すら無い（`grep -rn "IdentityProvider" --include="*.rs"` は0件）。本ADRは「既存traitの実装」ではなく**trait境界の新規定義と実動adapterの新規実装**を決める。
- ローカルパスワード認証が唯一の実装済み方式である。合流点は `orbisync-identity/src/login.rs` の `LoginService::login` で、成功側が `AuthSessionId` 生成 → access token発行 → refresh token生成とHMAC digest → `LoginTransactionStore::commit_success` を行う。
- **参加先（world）の認可経路が存在しない。** `crates/orbisync-server/src/realtime_ws_connection.rs` の `JoinInstance` 処理はinstance存在・capacity・lifecycleのみを見て `WorldAuthorizer::require` を一度も呼ばない。`crates/orbisync-world-runtime/src/actor.rs` の `InstanceCommand::Join` もinstance一致とcapacityだけを見る。`world.join` を強制する経路はどの層にも無い。`user_roles` はglobalで、world/instance列を持たない。
- `PgWorldAuthorizer::require` は `user_roles ⋈ role_permissions` のみを見るため、`user_roles` 行を持たない主体は全権限denyになる。
- `find_login` と `find_account` は `users JOIN user_credentials` の**INNER JOIN**である。`user_credentials` 行を持たない `users` 行はパスワードログイン経路に構造的に到達できない。
- `refresh_tokens` のrotationは `UPDATE auth_sessions SET expires_at = $2` で session期限を前進させる（sliding window）。rotationのSELECT（`crates/orbisync-storage-postgres/src/identity.rs`）は `s.status` を取るが **`s.expires_at` を取得しない**ため、現行のrefresh経路はsession期限を一度も検査していない。
- `users(id)` を参照するFKのうち `persistent_entities.owner_id` と `idempotency_records.actor_user_id` は `ON DELETE` 指定を持たない。加えて `instance_checkpoints.data` のJSONBが `owner` に `UserId` 文字列を**FK無しで**保持し、`PgCheckpointStore::validate_owner_references` が `save_checkpoint` と `load_latest` の両方から呼ばれる。`audit_events.actor_user_id` にはFKが無い。

## Decision

### 1. 仕様§19.5と `auth-authorization.md` §6の上書き

仕様§19.5と `auth-authorization.md` §6は「外部認証は初期版では実装しない。ローカル認証が唯一の公式実装となる」を `[SPEC]` として記述する。`technology-decisions.md` §8も「外部IdPを採用しない」と述べる。

**本ADRはこれらの記述を明示的に上書きする。** 理由は§2.3の汎用アプリ基盤化であり、当時の前提（単一運用者による閉じた利用）が変わったためである。

互換性のため次を守る。

- ローカルパスワード認証は**既定で有効**、動作・API・DBスキーマとも**無変更**。
- 新方式（guest / name-only / external）は**すべて既定で無効**。
- 既存の設定ファイル・既存DBをそのまま使って起動でき、挙動が変わらない。

### 2. 4方式と共通合流点

`local` / `guest` / `name_only` / `external` を `auth.methods` で選択・併用する。

主体の確定方法だけが方式ごとに異なり、確定後は `AuthSession` / access token / refresh token / Realtime ticket / `WorldAuthorizer` / RBAC / 所有権 / 監査のいずれも4方式で同一に扱われる。これらに方式による分岐を持たせない。

**合流の正確な範囲**: 資格情報を持たない3方式（guest / name-only / external）は `SessionIssuer` という単一の発行経路を通る。ローカル認証は既存の `LoginService::login` のまま残し、refresh token の生成と HMAC digest だけを `SessionIssuer` と同じ共有関数から取る。したがって「4方式が1つの関数を通る」のではなく、**token 材料の生成規則が共通で、`LoginService` だけが自分で session / refresh の record を組み立てる**。既存のローカル経路を書き換えないための選択であり、互換性を優先した結果である。永続化も `commit_success`（credential 用）と `commit_subject_success`（credential なし用）に分かれる。前者は失敗回数の予測条件とパスワード再ハッシュを運ぶが、後者にはその概念がないためである。

`realtime.allow_stub_ticket` と `auth.allow_stub_bearer` は既定falseのままで、新方式はこれらに依存しない。新方式もHMAC ticket経路（`HmacRealtimeTicketVerifier`）を通る。

### 3. guest / name-onlyの永続化範囲

**「永続アカウントを作らない」は「DBへ一切保存しない」ではない。**

`auth_sessions.user_id` と `persistent_entities.owner_id` は `users(id)` へのFKであり、`users` 行なしにsessionも所有権も成立しない。要件である「接続を閉じても再接続できる」「有効session中は同じ `UserId`」「再起動後の挙動が定義されている」はDB行が実在してはじめて満たせる。

したがってguest / name-onlyは `users` 行を持ち、**`user_credentials` 行を持たない**。`find_login` がINNER JOINであるため、これがパスワードログイン不可の構造的保証になる。

- `login_id` はサーバー生成（`guest:<UserId>` / `name:<UserId>`）。表示名から導出しないので同名でもUNIQUE衝突しない。`guest:` / `name:` / `ext:` は予約prefixとし、利用者作成経路で拒否する。
- guestの表示名はサーバー生成（設定 `display_name_prefix` 由来）で利用者入力を保持しない。name-onlyは利用者指定の表示名を `users.display_name` に保持する。**これが両方式の差である。**
- 管理APIのユーザー一覧には一時主体も**表出させる**。運用が所有者を追跡できる必要があり、隠すと監査性が落ちる。`users.kind` 列で区別できる。

### 4. 一時主体の絶対上限

`ephemeral_subjects.expires_at` を発行時に固定し、**以後一切更新しない**。これが唯一の上限である。

- 発行時: session・refresh token・access tokenの期限をすべて `min(グローバルTTL, ephemeral.expires_at)` に丸める。
- rotation時: `ephemeral_subjects` をJOINして `now >= expires_at` なら**拒否**し、新しい期限も同じく丸める。`ephemeral_subjects.expires_at` 自体は更新しない。
- 現行のrotationはsession期限を見ていない（Context参照）ため、**この新規判定が一時主体のrefreshを拒否する唯一の根拠**である。既存検査には頼れない。
- 恒久主体（local / external）は `ephemeral_subjects` 行を持たないため、判定も丸めも適用されず**現行挙動が保たれる**。

### 5. 失効と保持の分離

`instance_checkpoints.data` のJSONBが `owner` をFK無しで保持し、`validate_owner_references` が `load_latest` からも呼ばれるため、**「参照が無いこと」をSQLで確認する手段が無い**。期限切れ主体の `users` 行を削除すると、checkpointの保存だけでなく**復元も失敗する**。

したがって清掃を2つに分ける。

- **失効**（`expires_at + retention_seconds` 経過後に必ず実行）: sessionをRevoked、`realtime_tickets` 削除、**`user_roles` 行を削除**して全権限を剥奪、`users.status = 'disabled'`。この時点でその主体としてできることは何も無い。
- **保持**: `users` 行は参照用主体（tombstone）として残す。FK整合・checkpoint復元・監査の可読性のためだけに存在し、資格情報もロールも持たない。

**物理削除は行わない。** `retention_seconds` は「削除までの期間」ではなく「失効処理までの猶予」である。

アクセスの拒否は `expires_at` の到達時点で成立し、失効jobの実行有無に依存しない。`retention_seconds` はアクセス有効期間を延長しない。

監査は `audit_events.actor_user_id` にFKが無いため、いずれの場合も残る。

### 6. 参加先（world）境界

既存に参加先の認可点が無いため（Context参照）、**新しい認可点を追加する**。既存経路への合流だけでは要件を満たせない。

- `ephemeral_subjects.allowed_worlds` に、発行時点の設定値をサーバーが焼き込む。クライアントは送らないし変更できない。空は起動時に禁止する。
- 判定は常に **instance → `world_id()` 解決の結果**に対して行う。instance UUIDの静的リストでは、運用中に生成される新instanceを必ず弾くため維持できない。join処理は既にinstanceからworldを解決しているので追加クエリは要らない。
- 検査点は `JoinInstance`、`ResumeSession`（`binding.instance_id` から解決）、および**command受理の共通経路**に置く。pre-commit hookの権限再解決は有効な拡張が登録されている場合にのみ走るため、そこに相乗りしてはならない。`UpdateTransform` も対象に含める。lookup失敗は全経路でfail-closedとする。
- **`allowed_worlds` は発行時のsnapshotである。** 設定からworldを削除しても既発行の主体には即時反映されず、その主体は自分の `expires_at`（最大 `session_ttl_seconds`、既定1時間）まで参加を続けられる。即時に締め出す手段はsessionのrevokeである。join / resume / commandのたびに設定を読み直す設計は、設定reloadと実行中接続の整合が複雑になり fail-closed の判断点が増えるため採らない。snapshotなら判定はDB行の読み取りだけで完結し、影響時間が `session_ttl_seconds` で有界になる。
- **local / external の world 参加制限は本ADRの対象外**である。汎用のworld-scoped RBACは別課題として残る。

### 7. 一時主体のロール付与

ロール名はCoreに埋め込まず設定由来とする（`role_names`）。起動時にロール名を `RoleId` へ解決し、**見つからなければ起動失敗**とする。

権限の検査は**Core固定のallowlist**とし、設定で広げられないようにする。

| permission | 一時主体のroleに含めてよいか |
|---|---|
| `world.instance.read` | 可 |
| `entity.spawn` | 可 |
| `entity.update.own` | 可 |
| `entity.update.any` | 可 |
| `admin.*` | 不可（起動失敗） |
| `world.instance.create` / `world.instance.start` / `world.instance.stop` | 不可（起動失敗） |
| `moderation.kick` | 不可（起動失敗） |
| 未知のpermission文字列 | 不可（起動失敗） |

`admin.*` のdenylistでは不十分である。ADR-020がロールの名前空間混在を禁じているため、一時主体向けに指定されるロールは必然的にworld側のみとなり、`admin.*` 検査は常に通過する。`world.instance.create` 等は `admin.*` ではないのでdenylistを素通りする。

**allowlistの候補をすべて自動付与することはしない。** 実際に付与されるのは運営が `role_names` で指定したロールが持つpermissionだけである。allowlistは「ロールに含めてよい上限」であって付与内容ではない。

`entity.update.any` を持つ主体が他人のentityを更新できるのは**RBACの設計どおりの正しい挙動**であり、共同編集では意図された動作である。Coreはowner専用操作を強制しない。「owner専用のロック解除を他人が実行できない」という要求は、ADR-025のpre-commit外部ruleが強制するものであり、RBACとは別レイヤである。両者を混同して記述しない。

リクエストDTOには `role` / `permission` / `user_id` / `owner` フィールドを**定義しない**（無視ではなく不在）。OpenAPIは `additionalProperties: false` とする。

### 8. 外部認証の具体契約

署名済みJWTの検証とする。OIDCのID Tokenと同じ形だが、discoveryやcode flowは含まない。

- 入口は `POST /v1/auth/external { "token": "<JWT>" }`。
- 鍵は `auth.external.jwks_path` が指す静的JSONファイル。**HTTP取得はしない**ので、ローカルの署名issuerで実経路を検証できる。
- 検証: `kid` で鍵を引く／署名検証／`alg` が鍵ごとの期待値と一致（`none` は常に拒否、alg-confusionを構造的に失敗させる）／`iss` 一致／`aud` 一致／`exp` と `nbf` をleeway込みで検証／`sub` が非空。
- 内部ID対応は `external_identities` の `UNIQUE (issuer, subject)` で保証する。既知の組は既存 `UserId` へ、未知の組は新規 `users` 行を作る。**別issuerの同一subjectは別 `UserId`** になる。
- 自己申告の `email` / `name` / `role` / `groups` をidentityや権限の決定に使わない。**既存localユーザーへ自動で紐付けない。**
- 鍵更新は `kid` 単位で、新旧をJWKSに併記して無停止ローテーションする。`kid` 不明・ファイル読取失敗・パース不能はすべて401でfail-closedとする。
- `auth.external.issuer` が内部access tokenのissuerと一致する設定は**起動失敗**とする。内部発行JWTが `/v1/auth/external` で受理される自己受理を防ぐ。
- externalは恒久主体であり `ephemeral_subjects` 行を持たない。`user_credentials` 行も持たないためパスワードログインは不可である。

## Alternatives

- **guestをDBに一切保存しない**: `auth_sessions` と `persistent_entities.owner_id` のFKが成立せず、entity所有・再接続・再起動後の同一性がすべて満たせない。却下。
- **期限切れguestを物理削除する**: checkpointのJSONBがownerをFK無しで保持し復元時に検証されるため、削除するとinstanceの復元が失敗する。参照の有無をSQLで確認する手段も無い。却下。
- **参加先をinstance UUIDのリストで制限する**: instanceは運用中に生成されるため、同じworldの新instanceを必ず弾く。運用不能。却下。
- **`admin.*` のdenylistで権限を制限する**: ADR-020の名前空間分離により常に通過するため機能しない。却下。
- **書込側の再判定をpre-commit hookの権限再解決に相乗りさせる**: hookが登録されている場合にのみ走るため、既定構成では関門が不在になる。却下。
- **JWKSをHTTPで取得する**: fetch・cache・retry・失敗時挙動の設計が増え、オフラインでの実経路検証ができなくなる。静的ファイルで足りる。却下。
- **外部認証をOIDC code flowとして実装する**: redirect・state・PKCE・session cookieの設計が必要で、本改善の範囲を大きく超える。ID Token検証で用途を満たせる。却下。

## Consequences

- アカウント登録なしで参加するアプリをOrbiSync上で構成できる。
- 認証方式が増えても内部識別子とRBACの経路は1本のままであり、方式ごとの分岐が入らない。
- 一時主体は有界な寿命を持ち、反復refreshで延命できない。
- 期限切れの一時主体は権限とsessionを失うが、`users` 行は残るため所有entityとcheckpoint復元と監査は壊れない。DB行は増え続けるので、運用は `session_ttl_seconds` と主体の発生量を見て容量を見積もる必要がある。
- 参加先の認可点が新設されるが、効果は一時主体に限られる。local / external は従来どおり全instanceへ参加できる。
- `allowed_worlds` はsnapshotなので、設定変更の反映に最大 `session_ttl_seconds` の遅れがある。
- 外部IdPとの接続は静的JWKSの運用（鍵配布とローテーション）を前提とする。

## Migration

- `migrations/0018_auth_methods.sql` が `users.kind`（既定 `'account'`）を追加し、`ephemeral_subjects` と `external_identities` を作る。既存行はすべて `'account'` になるため、既存のlocalユーザーの挙動は変わらない。
- 新しい設定キーはすべて既定で新方式OFFなので、既存の設定ファイルは変更せずにそのまま使える。
- ロールバックは、新方式を `auth.methods` から外せば経路が無効になる。テーブルは残るが既存経路は参照しない。
