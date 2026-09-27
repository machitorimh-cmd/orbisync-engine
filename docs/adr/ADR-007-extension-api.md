# ADR-007: Extension API・Webhook delivery

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

初期Extension方式、認証、署名、retry、timeout、idempotency、SSRF境界を公開前に固定する必要がある。

## Decision

- v1はout-of-process Extensionだけをsupportし、outbound Webhookと認証済みREST Command APIを提供する。
- gRPCとWASM pluginはv1に含めない。
- Extension tokenは256-bit opaque token、有効期間30日、scope付き、HMAC digest保存、rotation可能とする。
- Webhookはtimestamp、event ID、bodyをHMAC-SHA-256で署名する。受信側の許容clock skewは5分とする。
- delivery timeoutは5秒、retryは最大5回、full-jitter exponential backoffは1秒開始・30秒上限とする。
- 5回連続失敗で30秒circuit open、最終失敗はDLQへ7日保持する。
- event IDはUUIDv7で、receiverはidempotency keyとして扱う。
- URLはHTTPSのみ、redirect無効、IPv4/IPv6のloopback/private/link-local/metadata/reservedをDNS解決後と接続時に拒否する。
- Extension failureを正準状態更新のtransaction/critical pathへ入れない。

## Alternatives

- in-process native plugin: crash/security境界が弱いため不採用。
- gRPC必須: 小規模Extensionに運用負担が大きい。
- delivery成功までdomain transactionを保持: 外部障害がCoreを停止させる。

## Consequences

- outbox worker、DLQ、secret rotation、egress policyが必要になる。
- exactly-onceではなくat-least-once deliveryである。

## Migration

Webhook schema、signature vector、SSRF negative test、retry/DLQ integration testを作成する。

2026-09-27: inbound側は [拡張Command API](../guides/extension-command-api.md) に具体化。
v1の閉じたコマンド集合は `entity.get` / `audit.get`。entity参照には明示instance scopeも必要。
登録・token発行/rotation・失効は運用者CLI、要求受付は `/v1/extensions/commands` とする。
