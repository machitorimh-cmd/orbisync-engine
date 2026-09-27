# 受入条件トレーサビリティ: extension-mechanism.md

2026-09-27更新。#8・#9のinbound実装に加え、残った6条件の局所検証を追加した。
以前の `58cc57d` 時点の「inbound経路なし」という判定は、現在の実装には適用しない。
追加検証の実行記録は extension-acceptance-completion (internal record omitted from this source distribution) を参照。

`COVERED` は記載した振る舞いの直接検証あり、`PARTIAL` は未確認部分あり、`GAP` は対応検証なし。
具体的なAPIとscopeは [利用手順](../../guides/extension-command-api.md)、今回の実行結果は
実装・検証記録 (internal record omitted from this source distribution) を参照。

| # | 受入条件 | 状態 | 証拠・範囲 |
|---|---|---|---|
| 1 | 第三者native libraryをコアへロードせずプロセス外で拡張 | COVERED | `check_architecture.py` でserverの推移的normal依存とRustソース133ファイルを検査。native loader依存・直接OS loader APIを拒否する負例も成功、CIへ接続 |
| 2 | domain mutationとoutboxを同一transactionへ保存し外部配送を含めない | COVERED | `extension_acceptance::outbox_insert_failure_rolls_back_user_credentials_and_audit` が実create_user処理のoutbox INSERTを失敗させ、user・credential・audit・outboxがすべて0件であることを確認。障害解除後は同じ操作が保存され公開payloadも一致 |
| 3 | HMAC署名・timestamp・event IDと受信側での署名再現 | COVERED | `delivery.rs::timestamp_changes_the_signature` |
| 4 | exponential backoff・再送上限・DLQ | COVERED | `delivery.rs::jitter_is_bounded_and_not_fixed_to_the_base` / `five_failures_are_recorded_in_the_dlq` |
| 5 | at-least-once配送で同じevent IDを使い受信側でdedup可能 | COVERED | `extension_acceptance::persisted_retries_preserve_event_id_and_do_not_expose_signing_key` が実worker/DBでtransport失敗→503→204を駆動。受信portの3要求すべてでheader/bodyのIDとpayloadを照合、署名再計算、dedup結果1件、失敗2回の永続化を確認 |
| 6 | circuit breakerが失敗宛先だけを遮断 | COVERED | `delivery.rs::circuit_breaker_is_per_destination` |
| 7 | 配送失敗・遅延がtick/正準更新を止めない | COVERED | `extension_acceptance::blocked_and_failed_delivery_does_not_block_actor_ticks_or_identity_commit` が実workerのHTTP portを明示的に停止。その間にactorのspawn・更新3回・tick/checkpoint3回・実DBのuser作成が完了。解除後timeout→DLQとなりtickも継続 |
| 8 | scoped tokenのcapability外・管理操作を拒否 | COVERED | `extension_commands.rs` の実DB/HTTPでscope外・別instance・停止・期限・権限縮小を拒否。`extension-runtime.ts` の実mainでユーザー管理APIも401 |
| 9 | gateway認証→application認可/use case→read port | COVERED | `ExtensionGateway` → `ExtensionCommandUseCase` → `ExtensionReadPort`。実DB/HTTP/actorのentity取得・component bytesとrevision・監査取得を検証。mainの依存配線gateも成功 |
| 10 | signing secret / scoped tokenの非出力 | COVERED | 既存のsecret参照保存・token digest保存・redaction・実CLI/HTTP/serverログ確認に加え、`extension_acceptance` でworkerの成功/再送/鍵取得失敗のtrace/span、実Prometheus出力、読み戻したmanifest、送信要求をcaptureし署名鍵マーカー非含有を確認。capture対象のtrace/metricが空でないことも検査 |
| 11 | v1でWASMをロードしない | COVERED | serverの推移的normal依存にWASM実行engineがないことを `check_architecture.py` で検査。wasmtime/wasmi/wasmer/extismの導入を模した負例が拒否されることも確認、CIへ接続 |

11 COVERED / 0 PARTIAL / 0 GAP。この拡張機構の11条件についての判定であり、他の仕様書の全条件や未知の不具合がないことの証明ではない。
DB試験は `ORBISYNC_REQUIRE_DB=1` を指定して実施した。試験名と実際の確認範囲は上表で区別する。
