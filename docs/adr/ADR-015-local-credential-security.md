# ADR-015: ローカル credential のセキュリティ

- Status: Accepted
- Date: 2026-08-03
- Revised: 2026-08-14 (W-27) — must_change_password を advisory へ変更
- Decision Owners: avistoria

## Context

AA-02 は Argon2id の parameter、password 最小長、弱い password の扱いを配備環境での計測後に決めるとしていた。また、自己登録を持たない初期版には初期管理者と新規利用者へ credential を安全に引き渡す手順が必要である。存在しない login ID と誤 password の応答形状だけを揃えても、hash 計算の有無による時間差から user enumeration が可能である。

2026-08-03 に Windows 11 Home、Intel Core i7-10510U（4 core / 8 logical processor）、15.8 GiB RAM、Rust 1.95.0、`argon2` crate 0.5.3 の release build で Argon2id v=19、output 32 bytesを計測した。固定した password/salt で各候補を1回warm-up後9回実行した結果は次の通りである。

| memory KiB | time | parallelism | min ms | median ms | p95/max ms |
|---:|---:|---:|---:|---:|---:|
| 19,456 | 2 | 1 | 27.55 | 35.79 | 52.99 |
| 32,768 | 2 | 1 | 47.40 | 78.12 | 96.86 |
| 65,536 | 2 | 1 | 106.57 | 158.80 | 213.58 |
| 32,768 | 3 | 1 | 93.10 | 120.14 | 141.44 |
| 65,536 | 3 | 1 | 168.95 | 243.44 | 323.74 |

初期構成は単一 server で、小規模な教育・業務用途を想定する。同時 password hash は既定4件に制限し、通常時の login burst は毎秒10件以下を設計目標とする。64 MiB/t=3 は4並列で256 MiB、測定中央値約243 msで online guessing の費用を高めつつ、正規利用者の login latency と server memoryを許容範囲に保つ。rate limit と account lock は hash parameter の代替ではなく併用する。

## Decision

### Password hash と policy

- Argon2id v=19、memory 65,536 KiB、time cost 3、parallelism 1、output 32 bytesを初期parameterとする。saltはpasswordごとに16 bytes以上をCSPRNGで生成し、PHC stringとして保存する。
- hash/verify は専用の bounded blocking executor へ隔離し、同時実行数の既定を4とする。queueにも上限を設け、超過は`RATE_LIMITED`とする。loginのIP/account rate limitと5回失敗・15分lockを併用する。
- parameterは配備classごとに起動時benchmarkで自動変更しない。変更は再計測とADRで行い、login成功時に保存済みPHC parameterが現行値と異なればrehashする。
- passwordはUTF-8で12文字以上、128文字以下とする。長いpassphraseを許容し、前後空白を含め入力を正規化・trimしない。
- 初期版は外部の巨大辞書をruntimeへ同梱しない。login ID、display name、製品名`orbisync`、`password`、連続/反復文字、広く使われる上位10,000語から生成したversion固定・repository管理のdenylistとのcase-insensitive完全一致を拒否する。denylistのversion更新は後方互換なpolicy強化として扱う。

### Timing と enumeration

- login IDが存在しない場合も、起動時に生成するdummy PHC hashを用いて必ず1回Argon2id verifyする。dummy hashはprocessごとにCSPRNG password/saltから生成し、永続化・log出力しない。起動時生成失敗はfail closedとする。
- 存在、password誤り、lock中の全経路はcredential lookupを1回、Argon2 verifyを1回行う。lock判定はverify後に適用し、外部には同じstatus、error code、body形状を返す。意図的なsleep/jitterは設けない。
- DB cache、scheduler、networkに起因する時間差は完全には除去できない。response body/header、audit有無、metric labelで存在を露出せず、IP/account rate limitで統計的samplingを制限し、latency分布をsecurity integration testで比較する。厳密な一定時間responseはDoS時にworker占有を増やすため採用しない。

### Initial administrator

- 初期管理者はserverの通常HTTP endpointやseed migrationでは作らない。`orbisync-server bootstrap-admin --login-id <id> --display-name <name>`を、migration権限を持つ管理端末で明示実行する。
- commandはactiveな管理permission保有者が0人の場合だけ成功する。transaction内でUser、Credential、組込みの`SystemAdministrator` Role割当、audit eventを作成し、再実行はfail closedとする。seed credentialや既定passwordをbinary/migration/environmentへ埋め込まない。
- 20 random bytesのCSPRNGをunpadded base64url化した一時passwordを生成し、成功時にinteractive terminalへ一度だけ表示する。stdoutがTTYでない場合は`--temporary-password-file <new-file>`を必須とし、既存fileを上書きせずowner-only権限で作成する。environment variable、CLI argument、log、auditにはpasswordを渡さない。
- Userは`must_change_password=true`で作成する。サーバーは`must_change_password`とlogin応答で状態を通知するが、M1では操作をブロックしない（advisory）。clientは`must_change_password==true`のときにpassword変更を促す。将来`identity_access.authorize`に判定を追加して強制できる。運用者はout-of-bandの承認済み経路でpasswordを本人へ渡し、表示/fileを直ちに破棄する。従来の「初回login後はchange-password以外を拒否する」記述は本改訂でadvisoryへ置き換える（2026-08-14, W-27）。

### New user and reset credential

- 通常のUser作成でもserverが同じ方式の20-byte temporary passwordを生成する。管理者指定passwordとemail reset linkは初期版で採用しない。
- `POST /v1/users`で一時passwordを必要とするclientは`Accept: application/vnd.orbisync.user-credential+json`を指定し、一度だけ`CreatedUserCredential`を受け取る。既存の`application/json`の`User` responseは維持するが、temporary passwordを取得しないclientは直後にreset-passwordを実行する必要がある。
- reset-passwordもserver生成値を`TemporaryPassword` responseで一度だけ返す。平文は永続化せず、response bodyのlogging/tracingを禁止し、監査にはactor/target/resultだけを記録する。response delivery失敗時は同じIdempotency-Keyによるretryだけが保存済み暗号化responseを24時間以内に再取得できる。
- いずれも`must_change_password=true`とし、既存sessionを失効する。

## Alternatives

- 32 MiB/t=3はmedian 120 msと軽いが、測定機で64 MiB/t=3が許容範囲だったためonline/offline attack costを優先した。
- 管理者指定passwordはsecure channelの責任をclientへ移し、request/log漏洩面を増やすため不採用。
- environment variableによるbootstrap passwordはprocess environmentやdeployment manifestへ残るため不採用。
- fixed dummy PHCをsourceへ埋め込む案は全配備で同じ値となる。processごとの起動時生成で、実Userと同じparameterを保つ。

## Consequences

- 4並列hashで約256 MiBに加え通常memoryを要するため、低memory配備は同時hash上限を下げる必要がある。
- login pathは不存在・lock中でも高価であるため、hash前のIP rate limitとbounded queueが必須になる。ただしaccount存在に依存するpre-hash shortcutは禁止する。
- temporary passwordを一度だけ扱うHTTP/client UIにはredactionと画面上の明示的な取扱いが必要である。

## Migration

1. password policy、dummy verify、bounded hash executorをidentity use caseへ実装する。
2. bootstrap commandとtemporary credential responseをcontract test、leak test、timing distribution testで検証する。
3. parameterをconfigurableにせず、将来変更時は新ADRとrehash-on-loginで移行する。

