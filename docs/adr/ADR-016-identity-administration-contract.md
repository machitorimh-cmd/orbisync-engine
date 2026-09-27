# ADR-016: Identity Administration の公開契約

- Status: Accepted
- Date: 2026-08-03
- Revised: 2026-08-14 (W-27) — must_change_password を advisory へ変更
- Decision Owners: avistoria

## Context

M1のOpenAPIにはpathはあるが、role、audit、password、CSV importのrequest/response schemaと、routeごとのpermissionが不足していた。`enabled` PATCHと専用enable/disable endpointも同じ状態を異なるretry semanticsで公開している。既存path/schemaを壊さず、追加だけで実装可能な契約を確定する。

## Decision

### Permissions

| Route | Required permission |
|---|---|
| GET `/v1/users`, GET `/v1/users/{id}` | `admin.users.read` |
| POST `/v1/users` | `admin.users.create` |
| PATCH `/v1/users/{id}` | `admin.users.update`。`enabled`を含む場合は加えて`admin.users.status` |
| POST `/v1/users/{id}/enable`, `/disable` | `admin.users.status` |
| POST `/v1/users/{id}/reset-password` | `admin.users.credentials.reset` |
| POST `/v1/users/import` | `admin.users.import` |
| GET `/v1/roles`, GET `/v1/roles/{id}` | `admin.roles.read` |
| POST `/v1/roles` | `admin.roles.create` |
| PATCH `/v1/roles/{id}` | `admin.roles.update` |
| DELETE `/v1/roles/{id}` | `admin.roles.delete` |
| PUT `/v1/users/{id}/roles` | `admin.roles.assign` |
| GET `/v1/audit-events`, GET `/v1/audit-events/{id}` | `admin.audit.read` |

`/v1/auth/*`と`/v1/realtime/tickets`は認証済み本人の操作でありadmin permissionを要求しない。`must_change_password==true`はadvisoryとし、サーバーは状態を通知するのみで操作をブロックしない（M1では横断的認可ゲートを増やさず、サービスアカウントのロックアウトを避けるため。将来`identity_access.authorize`で強制可能）。clientはpassword変更を促す。`POST /v1/auth/change-password`と`POST /v1/auth/logout`はいずれも配信済みであり、本ADRの「logout」言及はそのまま適用される（2026-09-05に`/v1/auth/logout`を配線、当初は planned だった）。本改訂で従来の「must_change_password中はchange-password/logout以外を拒否する」をadvisoryへ置き換える（2026-08-14, W-27）。permissionはallow-onlyで、組込みrole名ではなく上表の文字列を判定する。

### User statusの二つの表面

- canonical domain stateは`UserStatus::Active/Disabled`。REST DTOでは既存契約の`enabled`へ写像する。
- PATCHの`enabled`と専用enable/disableは両方維持する。前者はIf-Matchによるconditional idempotency、後者はIdempotency-Keyによるresult replayを用いる。
- どちらも`admin.users.status`を要求し、同じdomain commandと`user.enabled`/`user.disabled` audit actionを使用する。既に目標状態なら成功し、新しいstate transition/auditを重複生成しない。

### CSV import

- `text/csv; charset=utf-8`、UTF-8、header必須。列は`login_id,display_name`だけを初期契約とし、未知列、重複header、空必須値をrow errorとする。最大1,000 data rows、最大1 MiB。
- partial successを採用する。各rowはUser/Credential/auditを一つのlocal transactionで処理し、入力順に結果を返す。file全体の構文/header/size不正は0件処理で`INVALID_REQUEST`。
- DB内または同じfile内の重複login_idは該当rowだけ`RESOURCE_CONFLICT`。同一Idempotency-Keyのretryは保存済み全体結果を返し、新たなUser/passwordを生成しない。
- 202 responseは`UserImportResult`として`total/succeeded/failed/results`を返す。各成功rowのtemporary passwordは一度だけ含むため、response全体をsecretとしてredactする。非同期jobやpolling endpointは導入せず、202は既存契約との互換のため維持する。

### Errors and UUID

- `openapi/errors.yaml`の汎用codeだけを正本とする。resource固有codeを追加しない。存在を秘匿する認証では`AUTHENTICATION_REQUIRED`、permission不足は`ACCESS_DENIED`、不存在は`RESOURCE_NOT_FOUND`、一意制約等は`RESOURCE_CONFLICT`、revision mismatchは412`REVISION_MISMATCH`、内部障害は`INTERNAL_ERROR`。
- `UuidV7`の既存regexは変更しない。狭めると既存contractが許容したuppercase入力等を壊すためである。inboundはcase-insensitive UUID parserで受理した後、UUID version=7を検証する。response/cursor/audit outputはcanonical lowercase hyphenatedに正規化する。任意hyphen位置等、UUID parserが拒否する文字列は`format: uuid`にも適合しないため400とする。
- request IDはADR-014に従う。

## Alternatives

- resource固有error codeはclient分岐を細かくできるが、既存registryの汎用codeで十分で、enumeration面とcontract面を増やすため不採用。
- CSV all-or-nothingは単純だが、設計の失敗row集約と大規模運用での再投入負荷に劣る。
- `enabled`の一方を削除する案は既存path/schemaを破壊するため不採用。

## Consequences

- 同じstatus変更に二つのHTTP表面が残るが、domain command、permission、auditを共有して意味を一つに保つ。
- CSV responseとtemporary credential media typeは強いsecret redactionを必要とする。
- permission追加時はこの表とOpenAPI description/authorization testを同時に更新する。

## Migration

1. 欠落しているschema/contentをOpenAPIへ追加する。既存path/schema/error codeは変更・削除しない。
2. REST設計のerror例とrevision statusをregistry/ADR-003へ合わせる。
3. routeごとのpermission contract testとaudit/idempotency testを追加する。

