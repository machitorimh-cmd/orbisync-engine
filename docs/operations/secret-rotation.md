# Secret Rotation Runbook

## 共通原則

secretをrepository、image、command history、logへ記録しない。新旧を同時受理できるoverlap方式を優先し、発行側を先に新secretへ切り替え、旧secretの利用が消えたことを観測してから失効する。

## JWT signing key

1. 新しい非対称鍵と一意な`kid`をsecret manager内で生成する。
2. public verification keyをkey setへ追加し、全processへ反映する。
3. readinessと新旧key検証を確認後、新`kid`でaccess token発行を開始する。
4. 最大access token寿命とclock skewを超えるまで旧public keyを保持する。
5. 旧private keyを失効・破棄し、旧`kid`での検証件数が0であることを確認する。

漏洩時はoverlapを短縮し、該当session familyを失効する。refresh tokenはhashのみ保存し、rotation/reuse検知はADR-002に従う。

## Database credential

新role/passwordを作成し同一最小権限を付与する。新credentialでpoolを再接続し、旧connection drain後に旧credentialを失効する。migration roleとruntime roleを兼用しない。

## Webhook signing secret

新key IDをextensionへ事前配布し、署名headerへkey IDを含める。overlap中は新旧検証を許可し、新secretで署名開始後、retry最大期間を過ぎて旧secretを失効する。

## 検証とrollback

rotation ID、対象、開始/終了時刻、実行者、旧key失効確認だけをauditへ記録し、secret値は記録しない。失敗時は新規発行を停止し、失効前の旧keyへ戻す。
