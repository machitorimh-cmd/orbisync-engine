# Presence・永続化の運用診断

管理 Console の「運用診断」と `GET /v1/admin/diagnostics` は、`admin.diagnostics.read` を持つ利用者に、接続・Membership・永続化待ちを集約して表示する。通常の認証とセッション失効確認を通り、トークン、User/Instance/Presence ID、Entity の内容は返さない。`/metrics` の既存の公開範囲は変更しない。Prometheus の収集先は既存の配置方針に従って保護する。

## 状態の読み方

| 項目 | 意味 | 調査の目安 |
|---|---|---|
| 接続中 | 現行の socket 世代が所有する Presence | Token の発行時 TTL が過ぎても接続中の所有は残る |
| 猶予待ち | 切断後、通常の Resume 猶予期間内 | 正常。単独で障害通知しない |
| 回収の応答待ち | 猶予切れ、または容量確保のための evict 後、Leave の受理を待つ所有記録 | 一時的な mailbox Full は再試行される。5分以上続く場合は control mailbox と回収処理を調べる |
| 所有記録なし | Runtime Membership があるのに、接続中・猶予待ち・回収待ちのどの所有記録にも含まれない | 持続する場合は孤児 Membership の候補 |
| Membership なし | 所有記録の対象が Runtime に存在しない | Join/Leave の短い遷移を除き、所有記録の不整合の候補 |
| 接続と所有の差 | 参加済み delivery 接続数と接続中所有数の差の絶対値 | Resume の古い socket が閉じるまで一時的に増える。猶予待ちはこの比較に含めない |

Runtime の値は15秒ごと、および Resume 回収処理後に収集する。複数のロックを同時に保持しないため、Join/Resume/Leave と競合する一回の観測で差が出ることがある。通知例は5分の継続を条件とし、正常な猶予待ちの件数は異常条件に使用しない。Console は手動更新で、表示上の注意秒数を変更できる。ログアウト・接続先変更後の遅延応答は表示に復帰しない。

## 永続化待ちと Resume

`persistence.pending` は次の作業段階の合計であり、論理的なコマンドの一意件数ではない。

| 種類 | 数えるもの | 待ち時間の起点 |
|---|---|---|
| `entity_events` | Legacy actor の bounded outbox と、storage worker が所有する未保存効果 | Actor への enqueue。worker への drain と再試行でも最古の enqueue 時刻を引き継ぐ |
| `command_outcomes` | 実行結果があるが耐久性未確認の Legacy 結果。Dirty の fence も含む | コマンドの admission。実行前の予約だけの項目と保存済み結果は除外 |
| `generation_snapshots` | Generation storage が保持し、確定・不在などの耐久性解決を待つ Snapshot | 最初の保持。foreground timeout や再試行ではリセットしない |

`oldest_pending_seconds` は現在の各段階の最長バッチ待ち時間で、作業がない場合は0。Actor queue と Generation の年齢は単調時計、worker と command の年齢は注入された runtime clock を使う。Process 再起動後に以前のメモリ待ち時間は引き継がない。Generation の projection backlog と拡張 outbox/DLQ は別系統の項目であり、この合計に含めない。

Resume の拒否理由は `token_mismatch`、`history_unavailable`、`revision_gap`、`future_revision`、`not_authorized`、`ownership_lost` の固定6種類。HTTP の診断値と `resume_rejections_total{reason}` は process 起動後の累計で、Token 文字列や利用者由来の自由な文字列を label にしない。Wire のエラーコード・Resume の許可条件は変更しない。

## Prometheus と通知先

例は [orbisync-alerts.yml](../../deploy/prometheus/orbisync-alerts.yml)、通知先は [alertmanager.example.yml](../../deploy/prometheus/alertmanager.example.yml) にある。既定値は例であり、本番の容量、Checkpoint 間隔、Resume 猶予期間、通知先に合わせて編集する。Alertmanager の初期例は通知先未設定で、実際の通知は送らない。

Prometheus の既存設定へ次を追加する。`job_name` が異なる環境では rule の selector も変更する。

```yaml
rule_files:
  - /etc/prometheus/orbisync-alerts.yml
scrape_configs:
  - job_name: orbisync
    scrape_interval: 15s
    static_configs:
      - targets: ["orbisync-server:8080"]
```

| 通知 | 例の条件 | 環境別の調整 |
|---|---|---|
| Presence discrepancy | 孤児・所有欠落・接続世代の差が5分 | `for` を猶予期間と通常の引継ぎ時間より長くする |
| 回収停止 | 回収待ちが5分 | control mailbox の混雑と回収 cadence を考慮する |
| 永続化遅延 | 待ち作業あり、最長60秒超が2分 | 通常の書込 latency と障害許容時間 |
| Checkpoint 停滞 | 待ち作業の最長600秒超、最新保存も600秒以上前、2分継続 | `world.checkpoint_interval_secs` より余裕を持たせる。待ち作業のない空のサーバーは通知しない |
| DLQ 増加 | 保持行数が10分で増加、2分継続 | 保存済み DLQ gauge の差分。purge と同時の場合は増加が相殺されるため配信失敗 counter も確認 |
| Queue 圧迫 | control >51、他 >204 が3分 | 初期容量64/256の約80%。設定容量を変えたら閾値も変える |
| 観測停止 | Runtime または DB の最終収集から60秒超、2分継続 | DB timeout は2秒、収集周期15秒。古い0件を健全と見なさない |

Queue gauge は最後に観測された対象の深さであり、全 Instance の総和でも最も混んだ queue の保証値でもない。単独で飽和を完全に検知する用途には使わず、`instance_mailbox_saturated_total{queue}`、`state_updates_dropped_total` と実際の処理遅延も合わせて確認する。Checkpoint 最新時刻は Legacy・Generation の保存と DB の既存行を集約する。DB 収集は Console を開かなくても動作し、Resume 回収とは独立した timeout 付き task で実行する。

収集値は process 単位。複数ノードで集約する場合も `job, instance`（Prometheus の scrape target）を保持し、異なるノードの接続数と所有数を混ぜない。Alert の固定 label は severity と component のみである。

## 検証と一次対応

```powershell
docker run --rm --entrypoint /bin/promtool -v "${PWD}/deploy/prometheus:/rules:ro" -w /rules prom/prometheus:v3.5.0 check rules orbisync-alerts.yml
docker run --rm --entrypoint /bin/promtool -v "${PWD}/deploy/prometheus:/rules:ro" -w /rules prom/prometheus:v3.5.0 test rules orbisync-alerts.test.yml
```

Rule test は正常な猶予待ち・空のサーバーで通知しないこと、孤児 Membership と重複 socket の持続を検知すること、保存のない永続化待ちを検知することを確認する。[Prometheus の rule 定義](https://prometheus.io/docs/prometheus/latest/configuration/alerting_rules/)と [promtool の unit test 形式](https://prometheus.io/docs/prometheus/latest/configuration/unit_testing_rules/)を使用している。

Presence 異常では、まず観測時刻が新しいことを確認し、同一ターゲットの connected/grace/reclaim と不整合を比較する。`realtime.resume_bindings_pruned`、Resume の拒否理由、control mailbox の飽和を確認する。履歴の利用不可による full resync はそれだけでは所有異常ではない。

永続化遅延では、待ち段階、`checkpoint_save_failures_total`、`entity_persistence_failures_total` と DB の latency/接続状態を確認する。Dirty 結果や不確定 Generation がある場合、再起動するとメモリの調査材料を失うので、[Checkpoint operator](checkpoint-operator.md) と [Incident Response](incident-response.md) の既存の復旧手順に従う。
