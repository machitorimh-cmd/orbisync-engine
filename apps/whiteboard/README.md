# OrbiSync 共同ホワイトボード

## 今回用意したローカル実験環境

画面: http://127.0.0.1:5175/

- Core: http://127.0.0.1:18081
- 署名付き判定フック: https://localhost:8843/precommit（ロック判定はこれだけ。ロックAPIは廃止）
- 専用DB: orbisync-whiteboard-demo-db、localhost:55445
- DB永続ボリューム: orbisync-whiteboard-demo-data
- 設定・証明書・パスワード・ログ: `%LOCALAPPDATA%\OrbiSync\whiteboard-demo`

旧検証環境（担当15）のコンテナと実行ファイルはセッション終了時に失われたため、新しい専用DBにWhiteboard Demoを作成しています。旧検証の付箋データを復元したものではありません。既存の他DB・匿名ボリュームは削除していません。

## 使う

1. 画面を開き、サーバーを上記URLにする。ルールサービスの入力欄は廃止した。
2. `usera`でログイン。パスワードは `%LOCALAPPDATA%\OrbiSync\whiteboard-demo\usera-password.txt` を手元で開いて入力する。
3. 「一覧」→ボード選択→「入室」。
4. 「＋付箋」で追加。本文を変更して入力欄からフォーカスを外すと送信する。ボタン・入力欄以外の部分をドラッグすると移動する。
5. ロック/解除、×で削除。どれも通常の更新・削除コマンドとして送り、Coreの確定通知を受けてから画面へ反映する。拒否された場合は反映せず、`Core拒否 PRE_COMMIT_DENIED` をログに出す。
6. 別ブラウザまたはシークレット窓で`userb`として同じボードへ入る。パスワードは同じフォルダの`userb-password.txt`。

パスワードは公開・コミットしない。以前の`usera15/userb15`とは異なる新しい実験用アカウントです。

## 起動と停止（このPCの作成済み環境）

Docker Desktopを起動し、このリポジトリのルートでPowerShellから実行します。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/whiteboard/start.ps1
```

起動スクリプトは固定フォルダの秘密を環境変数へ読み込み、Core・判定フック・Viteを非表示プロセスとして起動します。使用中のポートがあれば停止せずエラーにします。準備済みの認証ファイル・DB・依存・Coreバイナリを使う起動手順であり、新規PC用インストーラーではありません。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/whiteboard/stop.ps1
```

停止は記録されたPIDと起動時刻が一致するプロセスだけに行います。専用DBコンテナも停止しますが、ボリュームとファイルは削除しません。ログは固定フォルダの`core/lock/ui.out.log`と`*.err.log`。

## 検証と制約

実画面での2ユーザー確認は外部ロック版で行ったものです。Core状態を正本にした今回の版は、実Core・実証明書を通した動作確認をまだ行っていません（別担当が実施予定）。正確な伝播遅延も未計測です。

付箋は`com.orbisync.whiteboard.note` componentとしてCore経由で同期します。**ロックの正本はこのcomponentの`locked`であり、Coreが保持する状態です。** 判定フックは外部ストアを一切持たず、Coreが署名付き要求に載せてくる`current_entity`（revision・owner_id・component値）だけで所有者限定のロック/解除を判定します。予約・finalize・TTLは無くなったため、確定通知消失で回収すべき外部状態も残りません。

Coreはcomponentを更新のたび丸ごと置き換えるので、UIは常に付箋全体を送ります。UIはロック状態を表示するだけで操作をローカルで止めないため、ロック中の編集や削除は実際にCoreへ届き、拒否は`PRE_COMMIT_DENIED`として観測できます。UIの無効化に依存しない証拠が必要なら、別クライアントから同じEntityへ直接EntityCommandを送ってください。

外部JSONロック版の`rules/lock-service.mjs`とそのテスト、および`%LOCALAPPDATA%\OrbiSync\whiteboard-demo\whiteboard-locks.json`は削除せず残していますが、起動スクリプトもUIも参照しません。旧サービスと新フックはどちらも8843を使うため同時起動はできません。詳細は[rules/whiteboard-lock-rule.md](rules/whiteboard-lock-rule.md)。


Unconfirmed mutations remain visible as uncertain, including edits, drag, lock and
delete. Retry resends the retained command ID, payload and expected revision. A
Snapshot becoming ready does not prove that command durability succeeded; only its
matching confirmation clears uncertainty. Pending and uncertain operations share a
32-command / 1MiB bound. A note with an unresolved operation cannot start another
mutation. Leaving the instance discards retained operations and listeners.

Explicit leave or join replacement ends local tracking and discards retained requests;
it does not undo commands already received by Core. Join replacement logs the number
of unconfirmed operations being abandoned. Requests belong to the correlation scope
that created them and cannot be submitted through a new instance/session helper.
Old callbacks cannot update the new instance UI or resume its creation sequence.
