# OrbiSync 設計図

図は理解補助であり、契約の正本は各設計文書、ADR、OpenAPI、Protoである。

## システムコンテキスト

```mermaid
flowchart LR
  Client[任意クライアント / SDK] -->|HTTPS JSON| API[REST Adapter]
  Client -->|WSS Protobuf| RT[Realtime Gateway]
  Admin[組織管理者] -->|公開管理API| API
  API --> App[Application Coordinators]
  RT --> App
  App --> Domain[Domain Modules]
  Domain --> Runtime[Instance Runtime]
  App --> Ports[Owned Ports]
  Ports --> PG[(PostgreSQL)]
  Ports --> Ext[Extension Webhooks]
  Ports --> Obs[Audit / Telemetry]
```

## モジュラーモノリスの依存方向

```mermaid
flowchart TB
  subgraph Adapters
    HTTP[http_api]
    WS[realtime_gateway]
    DB[persistence]
    Hook[extension adapter]
  end
  subgraph Application
    Coord[use-case coordinators]
    Contract[commands / queries / events / ports]
  end
  subgraph Domain
    Identity[identity_access]
    Worlds[world_directory]
    Runtime[instance_runtime]
    Interest[interest]
  end
  HTTP --> Coord
  WS --> Coord
  DB --> Contract
  Hook --> Contract
  Coord --> Contract
  Contract --> Identity
  Contract --> Worlds
  Contract --> Runtime
  Runtime --> Interest
```

## Realtime接続状態機械

次図は主要経路だけを示す非規範の要約図であり、全event/transitionを列挙しない。完全な機械契約は`contracts/realtime-connection-state-machine.json`、人間可読な完全表は`realtime-protocol-and-connection.md` §5.1を正本とし、両者をCIで対照する。

```mermaid
stateDiagram-v2
  [*] --> connecting
  connecting --> awaiting_hello: upgrade_succeeded
  connecting --> failed: upgrade_failed
  awaiting_hello --> ready: hello_accepted
  awaiting_hello --> failing: fatal protocol event
  ready --> joining: join_requested
  joining --> active: join_accepted
  joining --> ready: join_rejected
  ready --> resuming: resume_requested
  resuming --> active: resume_accepted
  resuming --> ready: resync_required
  active --> active: resync_required
  active --> closed: transport_lost / heartbeat_timeout
  ready --> closing: close_requested
  active --> closing: close_requested
  active --> failing: fatal protocol event
  closing --> closed: graceful_close_completed
  failing --> failed: fatal_close_completed
```

## 状態更新と配送

```mermaid
sequenceDiagram
  participant C as Client
  participant G as Realtime Gateway
  participant A as Application
  participant R as Instance Runtime
  participant D as Realtime Delivery
  C->>G: Envelope(command_id, sequence, expected_revision)
  G->>A: decoded command
  A->>R: validated domain command
  R->>R: dedup + authority + revision
  R-->>A: result + events
  A->>D: control/reliable/latest-wins
  D-->>G: bounded outbound sink
  G-->>C: serialized Envelope
```
