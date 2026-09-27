# ADR-002: 認証session・token・Realtime接続ticket

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

仕様は短命Access Token、長命Refresh Token、Refresh Tokenのhash保存とrotation、logout・利用者無効化時の失効を推奨しているが、具体形式と期限は未決だった。

また、設計文書はWebSocket upgrade要求の`Authorization` headerへAccess Tokenを設定していた。しかし、browser標準の`WebSocket` APIは任意HTTP headerを設定できず、公式TypeScript browser SDKと両立しない。Access TokenをURL queryやsubprotocolへ入れると、access log、proxy、browser履歴、telemetryへ漏れる危険もある。

## Decision

### Access Token

- Access Tokenは署名付きJWTとする。
- 有効期間は発行から15分とする。
- `sub`に`UserId`、`sid`に`AuthSessionId`、`jti`にtoken固有IDを格納する。
- `iss`、`aud`、`iat`、`nbf`、`exp`を必須claimとする。
- serverは許可した署名algorithmだけを受理し、`none`、claim欠落、algorithm confusionを拒否する。
- 初期署名algorithmはEdDSA（Ed25519）とする。private keyはsecretとして管理し、公開鍵に`kid`を付けてrotation可能にする。
- clock skew許容は前後30秒とする。
- RESTでは`Authorization: Bearer <access-token>`を使用する。
- 検証時はJWTの暗号学的検証に加え、`sid`の`AuthSession`がActiveであることを確認する。短TTL cacheを使用してよいが、logout・利用者無効化・refresh reuse時は即時無効化する。

### Refresh Token

- Refresh Tokenは256-bit CSPRNGによるopaque tokenとする。
- 有効期間は発行から30日とする。
- DBにはraw tokenを保存せず、専用secret keyを用いたHMAC-SHA-256 digestだけを保存する。
- refresh成功ごとに旧tokenを消費し、新tokenへrotationする。
- 消費済みtokenの再利用を検知した場合、同じsession family全体を即時失効し、監査eventを記録する。
- logoutは当該session family、利用者無効化は当該利用者の全session familyを失効する。
- browser SDKの既定はtokenのmemory保持とし、永続化しない。host applicationが永続化する場合はplatformのsecure storageを使用する。

### Realtime接続ticket

- clientは有効なAccess Tokenを使い、`POST /v1/realtime/tickets`からRealtime接続ticketを取得する。
- ticketは256-bit CSPRNGによるopaque token、有効期間60秒、単一使用とする。
- ticketは対象`UserId`、`AuthSessionId`、許可するprotocol audienceへserver-sideでbindingする。
- ticket stateは短命のserver-side storeへhashで保持し、process restart時に失効してよい。初期の単一processではbounded in-memory storeを使用する。
- WebSocket upgrade時にAccess TokenやticketをURL query、Cookie、`Authorization` header、subprotocolへ入れない。
- upgrade後、clientは最初のapplication messageである`ClientHello`にticketを含める。
- serverは5秒以内にClientHelloを受信し、ticketをatomicにconsumeして認証済み主体と`AuthSessionId`を得る。失敗・timeout・再利用時は接続を閉じる。
- ticket検証前はClientHello以外のapplication messageを処理しない。
- Resume Tokenは認証を兼ねない。resumeには新しい接続ticketによる認証と、有効なResume Tokenの両方が必要である。

### Secretとlog

- Access Token、Refresh Token、Realtime接続ticket、Resume Tokenのraw値およびprefixをlog、metric、trace、auditへ記録しない。
- 相関が必要な場合は、用途別keyのHMACまたはserver生成のopaque correlation IDを使用する。

### Password reset時の既存セッションの扱い (PW-1)

**判断: 対象利用者の全 `auth_sessions` と紐づく `refresh_tokens` を失効させる。**

理由:

1. セキュリティ: パスワードをリセットしたのに古い access token / refresh token が生き続けると、攻撃者に乗っ取られた後にリセットしても追い出せない。管理者によるリセットは「当該アカウントの全セッションを信頼できない」と宣言する操作であり、既存セッションを即時無効化しなければ復旧の意味が無い。
2. 整合性: `POST /v1/users/{user_id}/reset-password` は管理者による正規の回復経路であり、成功後は一時パスワードでの再ログインのみを許可すべきである。旧セッションが有効なままでは `must_change_password = true` の強制も迂回され、監査の一貫性も損なわれる。
3. 代替案 (維持) は不採用: 失敗回数維持の議論と異なり、セッション維持は「使い勝手のため古い token を残す」利点があるが、セキュリティ損失がそれを上回る。refresh token は `auth_sessions.status = 'revoked'` により間接的に失効する (rotate 時 `session_status != 'active'` で `Rejected`) ため、`refresh_tokens` 行の明示的削除は不要だが、効果として両方が失効する。
4. トランザクション: パスワード hash 更新・`must_change_password = true` 更新・`auth_sessions` の revoke を同一トランザクション (`IdentityMutation::ResetPassword`) で行う。途中で失敗した場合は全てロールバックし、部分的な状態 (パスワードだけ変わってセッションが残る) を作らない。監査 `password.reset` は成功時に `actor_id` と `resource_id` を記録するが、一時パスワード自体は記録しない。

検証: `POST /v1/users/{user_id}/reset-password` 成功後に旧 access token で `GET /v1/users/{user_id}` 等が `401 AUTHENTICATION_REQUIRED` になること、および旧 refresh token の rotate が `Rejected` になることを統合テストで検証する (PW-1 §2 / §9)。

## Alternatives

### WebSocket upgradeのAuthorization header

non-browser clientでは利用できるが、browser標準WebSocket APIで設定できないため不採用とした。

### Access Tokenをqueryまたはsubprotocolへ設定

browserで実装しやすいが、URL・proxy・access log・telemetryへ漏れる危険が高いため不採用とした。

### Opaque Access Token

即時失効は単純になるが、全REST要求でserver-side token lookupが必要となる。公開claimと署名検証を標準化しやすいJWTを採用し、session status確認を組み合わせる。

### Refresh TokenをHttpOnly Cookieへ固定

browserでは有力だが、cross-origin配置、CSRF、native SDKとの契約が複雑になる。初期SDKはmemory保持を既定とし、cookie modeは後続のbrowser deployment要件が確定した時点で追加検討する。

## Consequences

- browser、native、server clientで同じRealtime handshakeを使用できる。
- WSS確立後からClientHello認証完了まで、短時間の未認証socketを保持するため、handshake timeoutと未認証接続数limitが必須になる。
- JWTだけで完全にstatelessにはならず、即時失効のためAuthSession status確認が必要になる。
- Ed25519 key rotation、`kid`、active/retiring keyの運用が必要になる。
- ticket storeはboundedでなければならず、発行・期限切れ・consume・再利用をmetric化する。

## Migration

実装前の決定であり、既存token migrationは不要である。

1. OpenAPIへlogin、refresh、logout、Realtime ticket発行endpointを定義する。
2. `.proto`のClientHelloへconnection ticket fieldを追加する。
3. SDKの接続手順をrefresh → ticket取得 → WSS → ClientHelloへ変更する。
4. header/query/subprotocolへtokenを入れない回帰testを追加する。
5. JWT検証、session失効、refresh rotation/reuse、ticket expiry/reuse/timeoutをtestする。

### V-10: refresh digest 鍵の分離に伴う既存 digest の移行方針

**判断: 一斉失効（同時無効化）を許容する。二重照合（旧 pagination 鍵と新 refresh 鍵の両方で検証）期間は置かない。**

理由:

1. 本番運用前の是正である。V-10 時点では本番に永続化された refresh digest が存在せず、影響は開発・検証環境の再ログインに限定される。一時的に旧鍵でも検証可能にする二重照合は、移行期間中の鍵共有という V-10 が解消しようとした結合そのものを再導入する。
2. 二重照合はコードと運用の複雑さを増やす。照合分岐はテストと監査の対象を倍増させ、旧鍵の漏洩・運用ミスが refresh 認証へ波及する窓を延長する。単一鍵への即時切替は、鍵のライフサイクル分離（ADR-002 §Refresh Token「専用secret key」および §Secretとlog「用途別key」）を最も速く達成する。
3. refresh token 自体は 30 日で自然失効し、失効した場合は login による再発行が正規の回復経路である。開発環境で全利用者が 401 を受けても、再 login で即時復旧できる。可用性影響は一時的かつ限定的である。
4. 将来本番で鍵ローテーションが必要になった場合は、ADR-002 の「用途別key」原則に従い、旧鍵での検証を許容するローテーション手順を別途設計する（例: 新旧鍵の並行検証と段階的移行）。V-10 はその前提となる「鍵を分離する」こと自体を達成する変更であり、ローテーション機構の導入は範囲外とする。

運用手順:

- V-10 デプロイ前に `ORBISYNC_REFRESH_TOKEN_HMAC_KEY` を生成し、環境へ配布する。未設定では `config.verify_secrets` および `main.rs` の起動時検証が fail-closed で起動を拒否する。
- デプロイ直後、旧 pagination 鍵で生成された digest は検証失敗し refresh は 401 を返す。利用者は再 login する。
- 旧 pagination 鍵の値は refresh 用途では再利用しない。pagination cursor 鍵のローテーションは refresh セッションに影響しないことを確認する。
