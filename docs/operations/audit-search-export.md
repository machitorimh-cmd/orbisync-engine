# 監査ログの検索と出力

管理Consoleの監査ログは、日時範囲、Actor ID、action、resource type、resource ID、request IDを組み合わせて検索できます。日時は開始・終了を含み、開始が終了より後の場合は拒否されます。action等は完全一致です。参照・出力には従来と同じ `admin.audit.read` が必要です。

## APIとページの扱い

- `GET /v1/audit-events` は検索ページを返します。
- `GET /v1/audit-events/export?format=json` はJSONの1ページを返します。
- `GET /v1/audit-events/export?format=csv` はCSVの1ページを返します。

3つの操作で同じ `from`、`to`、`actor_id`、`action`、`resource_type`、`resource_id`、`request_id`、`limit`、`cursor` を使用します。limitは1～200、既定50です。文字列フィルターは空文字・制御文字を許可せず、256バイトまでです。取得件数を増やすための自動的な全ページ走査はありません。

カーソルは全フィルターと精密な日時に束縛されます。条件を変えるときはカーソルを破棄し、先頭から取得します。管理画面も入力変更時に一覧とカーソルをリセットし、以前の応答を表示へ混ぜません。並びは日時・IDの降順で、PostgreSQLのマイクロ秒精度を保持します。更新前のauditカーソルは再利用せず検索し直してください。

JSONは `next_cursor`、CSVは `X-Next-Cursor` ヘッダーで続きを示します。CORS許可済みoriginでは、このヘッダーをブラウザから読めます。ヘッダーがなければ終端です。管理画面の出力ボタンは最後に取得した1ページの条件・カーソルを使い、画面で蓄積した全行を一括取得する動作はしません。

## 出力と保存

単件参照・一覧・両出力は同じメタデータ除去処理を使います。password、認証token、secret、source IP等のフィールドは入れ子も含めて除き、保持期限を過ぎたIPを証跡へ混ぜません。元の監査保存・IP保持期限の仕組みは変更しません。

CSVは引用符・改行をエスケープし、数式として扱われる `=`、`+`、`-`、`@` 等で始まるセルにはアポストロフィを付けます。応答は `Cache-Control: no-store` です。取得した証跡ファイルにも組織の保管権限と保存期限を適用してください。
