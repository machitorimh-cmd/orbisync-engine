# Graceful Shutdown Runbook

## Process-local admission and in-flight work

The first shutdown signal lowers readiness and closes process-local admission.
New application HTTP requests (including realtime ticket issuance) return 503
before body, authentication, or storage work. `/health/live`, `/health/ready`,
`/metrics`, and `/version` remain observable; readiness returns 503. The listener
may remain open during draining, so listener reachability is not admission.
HTTP requests that already passed the entry gate may complete under their
existing request deadlines. A ticket response arriving after shutdown does not
establish when the request was accepted or whether a WebSocket was accepted.

Fresh upgrades are refused. A previously upgraded socket rechecks admission
after receiving ClientHello and after asynchronous authentication; consumed
tickets are not restored, and no successful hello follows detected shutdown.
Join/Resume and application messages consult the shared process gate even before
the connection notification arrives. Heartbeats may continue during draining.

Realtime work accepted before the cutoff retains an admission guard through its
handler. Activation retains that guard through registration and Join. Shutdown
waits for accepted work before enumerating actors for draining and final saves,
so an accepted restore may register an actor after readiness falls and that actor
is still included. Idle sockets do not hold admission guards. This wait shares
the original first-signal 30-second drain plus 10-second force deadline; it adds
no new budget and does not cancel owned persistence or cleanup. An incomplete
wait at force exit is uncertainty, not proof of a final save.

This contract covers process-local transport admission. It is not an atomic
transaction between already admitted HTTP database writes, actor state, and
background/extension work. A universal prohibition on all mutation after signal
entry would require a separate application-wide transaction/ownership contract.

## 通常手順

1. 対象processをload balancerのreadiness対象から外し、新規HTTP/WSS受付を停止する。
2. realtime接続へserver shutdownと再接続猶予を通知する。
3. Instance Runtimeを`draining`へ移し、新規joinと新規long-running commandを拒否する。
4. 受理済みcommand、reliable queue、audit/outboxを期限内にflushする。latest-winsは最終snapshot/revisionで置換できる。
5. checkpoint対象状態を永続化し、DB poolとlistenerを閉じる。
6. task leakがないことを確認して終了する。

初期drain期限は30秒、強制終了猶予は追加10秒とする。実測でADR更新する。SIGTERMは上記手順、2回目のSIGTERM/SIGINTは緊急停止として扱う。

## 失敗時

flush失敗を成功扱いしない。期限超過時は未配送reliable/outbox件数とcheckpoint revisionを記録して終了し、次回起動時にdurable outboxを再開する。状態破損が疑われる場合はreadinessを上げず、[Incident Response](incident-response.md)へ移行する。

## 検証

deployごとに新規接続拒否、既存接続通知、command drain、outbox再開、終了code、再起動後のrevision整合を確認する。kill -9試験は通常手順と分け、chaos testとして実施する。
