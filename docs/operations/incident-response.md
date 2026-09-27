# Incident Response Runbook

## Severity

| Severity | 例 | 初動 |
|---|---|---|
| SEV-1 | 資格情報漏洩、全停止、正準状態破損 | 即時招集、変更凍結、利用者通知準備 |
| SEV-2 | 一部world停止、重大な性能劣化 | 30分以内に担当決定 |
| SEV-3 | 回避可能な不具合、監視劣化 | 通常保守で追跡 |

## 初動

1. incident ID、開始時刻、commander、scribe、影響範囲を決める。
2. audit/log/metrics/trace、deployment revision、設定hashを保全する。tokenや個人情報をticketへ貼らない。
3. `/health/live`、`/health/ready`、DB接続、mailbox、slow consumer、error code増加を確認する。
4. containmentを選ぶ: 新規受付停止、対象instance drain、該当account/session失効、extension停止、旧releaseへrollback。
5. すべての変更、時刻、実行者、結果をtimelineへ記録する。

## Security incident

[SECURITY.md](../../SECURITY.md)の非公開経路を使用する。漏洩secretを特定し、[Secret Rotation](secret-rotation.md)に従う。証拠保全前にlogやDBを削除しない。法務・契約上の通知期限はdeployment組織が判断する。

## Recovery and closure

受入条件を定義して段階的にtrafficを戻す。24時間以内に再発監視を設定する。5営業日以内を目安に、原因、寄与要因、検知gap、対応、恒久策、ownerと期限を含む非難しないpostmortemを作成する。
