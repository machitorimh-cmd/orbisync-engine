# ADR-001: 公開名称・package命名・識別子形式

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

原仕様では公開名称`OrbiSync`と仮称`metaverse-core`が混在し、server binary、Rust package、WebSocket subprotocolの正式名称が未確定だった。また、domain IDの内部表現としてUUID v4、UUID v7、ULID等が候補に残っていた。

repository scaffold、公開contract、DB schemaを作成する前に、利用者へ露出する名称と永続識別子を統一する必要がある。

## Decision

- 製品・repositoryの公開名称は`OrbiSync`とする。
- server binary/package名は`orbisync-server`とする。
- 公開するRust packageは`orbisync-*`を接頭辞とする。
- WebSocket subprotocolは`orbisync.v1.protobuf`とする。
- 開発時限定のJSON debug subprotocolを採用する場合は`orbisync.v1.json`とする。
- Protocol Buffersのpackageは`orbisync.v1`とする。
- `UserId`、`RoleId`、`WorldId`、`InstanceId`、`EntityId`、`AuthSessionId`、`RealtimeConnectionId`、`PresenceId`等の永続・公開IDはUUIDv7を内部表現とする。
- RustではIDごとのnewtypeで包み、異なるIDを同じ`Uuid`引数として取り違えない。
- PostgreSQLではnative `UUID` columnを使用する。
- JSON、OpenAPI、Protocol Buffersではcanonical lowercase hyphenated UUID stringとして表現する。
- `Revision`と接続内`sequence`はUUIDではなく`u64`のままとする。

原仕様内の`metaverse-core`、`metaverse-core-server`、`metaverse.v1.*`は、仕様作成時の仮称・例として読み替える。原仕様本文自体は履歴保持のため一括置換しない。

## Alternatives

### UUID v4

広く普及しているが、生成順序を持たず、DB index localityと運用時の時系列把握でUUIDv7に劣る。

### ULID

文字列表現が短く時系列順に扱いやすいが、PostgreSQL native UUID型およびRust UUID ecosystemとの直接的な整合でUUIDv7に劣る。

### Snowflake型ID

短く順序性を持つが、node ID、clock、発行基盤の運用が必要となり、初期の単一binary構成には過剰である。

### `metaverse-core`の継続

用途を限定して見せやすく、仕様内の仮称と一致するが、正式名称として既に明記された`OrbiSync`と不整合になる。

## Consequences

- 公開contract、DB、log、test vectorでID表現を統一できる。
- UUIDv7生成には暗号学的乱数源と正しい時刻処理が必要になる。
- IDの順序性はpaginationや業務上の作成時刻の正本として使用しない。正式な時刻は`created_at`等で保持する。
- UUIDv7に含まれる時刻情報を秘密情報として扱う必要がある用途では、別のopaque external IDを検討する。
- Rust package名とcrate import名は、それぞれhyphenとunderscoreの通常変換に従う（例: package `orbisync-domain`、import `orbisync_domain`）。

## Migration

実装前の決定であり、既存データmigrationは不要である。

1. repository、Cargo package、binary、Protocol packageへ決定名を使用する。
2. DDLのID columnをPostgreSQL `UUID`として作成する。
3. transport adapterでUUID stringとdomain newtypeを明示変換する。
4. 非UUID、UUID v4、形式不正、ID type取り違えのnegative testを追加する。
