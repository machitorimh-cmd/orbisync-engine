# ADR-025: 状態確定前のExtension検証フック（Core改造なしのアプリ固有ルール強制）

- Status: Accepted
- Date: 2026-09-07
- Decision Owners: avistoria

## Context

`docs/design/generalization-and-llm-app-platform.md` §2.2は、Core本体を改造せずにアプリ固有の検証ルール（共同編集ツールの編集権限細分化、ドローン・シミュレーターの飛行禁止区域判定等）を強制する経路が現状存在しないという欠落を指摘している。

`extension_gateway`はADR-007・`extension-mechanism.md`により次の2方向のみを公開契約として確定済みである。

1. Core→Extension: ドメインイベント発生後の非同期Webhook配送（事後通知、at-least-once）
2. Extension→Core: scoped tokenによる認証済みExtension Command API（Extension発の能動的要求）

いずれも「Coreが状態変更を確定する前に、外部Extensionへ判定を委ねる」経路ではない。仕様§23.2はこの2方向を初期版の拡張方式として定めるが、3つ目の方向を禁止してはいない。

コード読解により次を確認した。

- `InstanceActor::handle`（`crates/orbisync-world-runtime/src/actor.rs`）は完全同期でI/Oを一切行わない。同期HTTP待ちをここに置くことはできない。
- entity mutationコマンド（`SpawnEntity`/`UpdateEntityComponent`/`DeleteEntity`）がmailboxへ投入される経路は、`crates/orbisync-server/src/realtime_ws_connection_runtime.rs`の1箇所（約1031行目の`registry.submit`呼び出し）のみである。RESTには entity操作エンドポイントが存在しない。`UpdateTransform`は別の投入点（約668行目）を持ち、20Hzの連続ストリームである。
- 同ファイルの当該分岐では、`submit`呼び出しの直前で`acquire_command_durability_guard`によりinstance単位の`tokio::sync::Mutex`（`CommandDedupStore::durability_guard`）を取得し保持する。これより後で外部HTTP待ちを挟むと、同一instance内の無関係な操作がフック応答待ちでブロックされる。
- コマンド発行済みの`command_id`（UUIDv7）は`CommandDedupStore`により既にLeader/Duplicate/Conflict判定される、再送に強い idempotency キーとして機能している。

## Decision

第3の相互作用方向「Core→Extension同期問い合わせ（pre-commit validation gate）→Core確定」を追加する。

- **対象コマンド**: `SpawnEntity`、`UpdateEntityComponent`、`DeleteEntity`のみ。`UpdateTransform`（20Hzストリーム）、`Join`/`Leave`/`TransferOwnership`は対象外とする。禁止区域のような位置依存ルールは、位置を伴う`SpawnEntity`または位置componentを明示的に運ぶ`UpdateEntityComponent`で実現するものとし、Transformストリームでのリアルタイム位置検証は同期呼び出しのレイテンシ予算と両立しないため対象としない。**この機構だけでは「位置移動そのもの」を継続的に禁止区域から締め出すことはできない**（後述「既知の迂回経路」）。`TransferOwnership`は現行のWSエンティティコマンド分岐（`realtime_ws_connection_runtime.rs`の`operation`match、`_ =>`アームで未知operationを拒否）からは到達不能であることをコード上確認済みであり、これは「対象外」ではなく「別経路が存在しない」ため迂回の懸念がない。
- **挿入位置**: `realtime_ws_connection_runtime.rs`の該当分岐で、`instance_command`構築後・`acquire_command_durability_guard`呼び出し前。フックのawaitはinstance単位のdurability mutexを保持しない。
- **オプトイン**: instanceに紐づくActiveなExtension registrationが、当該コマンド種別に対応するcapability（`hooks:entity:spawn` / `hooks:entity:update` / `hooks:entity:delete`）を購読していない限り、フックは一切呼ばれない。既定はフックなし（既存挙動と完全に同一）。1コマンド種別につき購読するActive拡張は高々1件（複数拡張の合議・優先順位は本ADRの範囲外、将来ADRへ）。
- **権限分離**: RBAC・所有権判定はCoreが引き続き専有する。フックはCoreが既に許可した操作に対する追加の拒否権（AND条件）のみを持ち、Coreが拒否する操作を許可へ変える権限を持たない。フックはactorへの到達可否のみを左右し、actor内の`is_owner_allowed`・`expected_revision`比較・数値検証等を変更・スキップしない。
- **既定拒否**: timeout・非2xx応答・接続失敗・応答パース不能は、いずれも拒否として扱う（フェイルオープンにしない）。拒否時は状態変更なし、`ErrorMessage`を返す。ADR-007の「Extension failureを正準状態更新のtransaction/critical pathへ入れない」は、失敗時=状態変更なしの拒否として満たされる（状態更新そのものの失敗ではない）。
- **冪等性**: 新しい重複排除スキームを作らず、既存の`CommandDedupStore`の`command_id`ベースのLeader/Duplicate判定をそのまま再利用する。同一`command_id`の再送はフックを再度呼ばない。
- **revision不一致**: フック問い合わせ中に対象entityのrevisionが進んだ場合、再判定は行わない。フックがallowを返しても、actorは`submit`到達後に既存の`expected_revision`比較をそのまま行うため、revision不一致は既存の楽観的並行性制御でそのまま拒否される。フックの判定は「参考情報」であり、確定は常にactorが行う。**この保証の正確な範囲と限界は次の「revisionの束縛と限界」節を参照。**
- **登録の一意性**: 同一capabilityを複数のActive registrationが同時に持つことを、書き込み時（`save_registration`、`pg_advisory_xact_lock`によるcritical section）と読み取り時（`find_active_registration_by_capability`、2件以上ヒットしたらエラー）の両方で拒否する。`LIMIT 1`は一意性の保証ではないため、この2つのチェックがそれを補う（`crates/orbisync-storage-postgres/src/extension.rs`、`tests/integration/tests/precommit_registration_db.rs`で実PostgreSQL検証済み）。
- **timeout値**: Webhook配送（ADR-007、5秒）とは別に、独立した短いtimeout（既定500ms、`extensions.pre_commit_validation_timeout_ms`で設定可能）を設ける。同期パスでの待ち時間としてWebhookのtimeoutをそのまま流用しない。
- **bounded concurrency**: プロセス全体での同時フック呼び出し数に上限（`extensions.pre_commit_validation_max_concurrency`、既定16）を設ける。
- **endpoint**: クライアントは指定できない。`extension_registrations`テーブルの`endpoint`列と`signing_secret_ref`（既存スキーマ、`rest-api-persistence.md` §8）をそのまま使う。ADR-007のegress policy（loopback/private/link-local/metadata/reserved拒否、`orbisync-extensions::delivery`の`DnsResolver`実装）を再利用する。テスト専用に`extensions.allow_loopback_endpoints`（既定false）でloopbackを許可できるが、本番ではfalseを維持する。

### 既知の迂回経路（Transform auto-create）— §8で解消

`UpdateTransform`は対象外とする設計判断（上記）とは別に、当初は実際に迂回経路が1つ存在した。`InstanceActor`は未知の`entity_id`に対する最初の`UpdateTransform`をentityのauto-create（M2、`actor.rs`）として扱い、クライアントが送った位置でそのままentityを生成する。この生成経路はフックを一切通らないため、`SpawnEntity`を送らず`UpdateTransform`だけを送るクライアントに対して、Spawnフックが強制するはずの制約（禁止区域判定など）がまったく効かなかった。

独立検証（worker-brief §8、優先項目2）はこれを「対象外の操作」ではなく実在するバイパスと判定し、**同期HTTPをTransformストリームへ追加しない**という制約のもとで、auto-createそのものを無効化する修正を要求した。

**解消策**: `RealtimeState`が`spawn_hook_active: Arc<AtomicBool>`という、ホットパスに一切I/Oを持ち込まないプロセス内フラグを保持する。

- このフラグは`extension_registrations.find_active_registration_by_capability("hooks:entity:spawn")`の結果を反映し、プロセス起動時に一度、その後は既存の拡張登録キャッシュ（`PgExtensionOutboxStore::refresh_active_registration_cache`）と同じ15秒周期のバックグラウンドtaskで再取得される（`RealtimeState::refresh_spawn_hook_active_cache`）。**単なる起動時固定flagではない**——登録の追加・無効化はこの周期内に反映される。
- 同じ`Arc<AtomicBool>`を、既に起動済みの`InstanceActor`ともコンストラクタ経由で共有する（`InstanceActor::with_spawn_hook_active`）。actorはこれを`Ordering::Acquire`で読むだけで、DBやHTTPへは一切触れない——`InstanceActor::handle`のNo-I/O不変条件を破らない。instance側の再起動やactor再生成を待たずに、既存のactorも次のコマンド処理でフラグの最新値を見る。
- `actor.rs`の`UpdateTransform`ハンドラは、entityが存在しない（auto-create分岐に入る）場合にこのフラグを確認し、`true`なら`EXPLICIT_SPAWN_REQUIRED`で拒否する。既存entityへの通常のTransform更新（entityが既に存在する分岐）には一切影響しない。

**検証**（`tests/integration/tests/precommit_hook_2_2.rs`、いずれも実配線）:

- `transform_auto_create_is_rejected_when_a_spawn_hook_is_active`: deny-everythingなgateかつspawn hookが登録された状態で、未知entityへのTransformが`EXPLICIT_SPAWN_REQUIRED`で拒否され、`gate.calls == 0`のまま（HTTPを一切呼ばずに拒否している）ことを確認。
- `transform_auto_create_still_works_without_a_spawn_hook`: spawn hook未登録なら、auto-createは従来どおり成功する（オプトアウト互換）。
- `existing_entity_transform_updates_still_work_when_a_spawn_hook_is_active`: 既にSpawn済みのentityへの通常のTransform更新は、spawn hookが有効でも成功する（このフラグがauto-create分岐だけを制限し、通常の移動を妨げないことの確認）。

**この修正でも塞がらない範囲**: このフラグは「auto-createそのものを許可するか」の二値判定であり、Spawnフックが実際に判定する内容（禁止区域か等）をTransformの継続的な移動には適用しない。`SpawnEntity`で一度生成された後の位置移動（既存entityへの通常Transform）は、引き続きフックの対象外である（§2.3の設計判断どおり）。「新規entityの無許可生成」という迂回だけを閉じており、「既存entityの継続的な位置移動を禁止区域判定で縛る」という、より広い要求（提案文書のドローン例が示唆する用途）には応えていない——これは本ラウンドでも明示的なスコープ外のままである。

#### §9で追加解消: 15秒キャッシュ窓とlookup失敗のfail-open

再検証（worker-brief §9）は`spawn_hook_active`の周期キャッシュだけでは2つの残存ギャップが残ることを、`AtomicBool`を直接書き換えるのではなく実登録storeと実`refresh_spawn_hook_active_cache`を駆動する再現テストで確認した。

- **ギャップA（タイミング窓）**: 起動後にspawn hookを登録しても、次の周期リフレッシュ（最大15秒後）までauto-createが迂回可能だった。
- **ギャップB（fail-open）**: `refresh_spawn_hook_active_cache`のlookup失敗（重複登録Conflict、DB障害等）時に以前の値（起動直後の初期値`false`含む）を保持していたため、lookup失敗が続く限りauto-createが無期限に迂回可能だった。同一条件下でexplicit `SpawnEntity`はライブlookupで正しくfail-closedしており、非対称だった。

いずれも「毎Transformへ同期HTTPを追加する」以外の方法で解消する。

**解消策（2つの独立した対策、両方とも実装）**:

1. **新規auto-create候補のみのライブ確認**（`realtime_ws_connection_runtime.rs`のUpdateTransform処理、`registry.submit`呼び出し直前）: entityが既に存在するかを`registry.interest_views(instance_id)`（`RwLock`読み取り、interest filteringで既に使われているキャッシュ、actor round tripなし）で確認する。**既に存在する場合は何もしない**——既存entityへの通常のTransform移動は一切の追加コストを受けない。**存在しない場合のみ**（真にauto-create候補である場合のみ、entity生存期間中に一度しか起きない）、`state.extension_registrations.find_active_registration_by_capability("hooks:entity:spawn")`を**キャッシュを介さずライブに**呼び、explicit `SpawnEntity`が既に行っているのと同じ問い合わせを行う。Activeな登録が見つかれば`EXPLICIT_SPAWN_REQUIRED`、lookup自体が失敗すれば`PRE_COMMIT_UNAVAILABLE`で、いずれもactorへ到達させずに拒否する。この経路はタイミング窓を構造的に持たない（周期リフレッシュを待たない）うえ、lookup失敗時も明示的にfail-closedする。
2. **`spawn_hook_active`キャッシュ自体のfail-closed化**（`RealtimeState::refresh_spawn_hook_active_cache`）: lookupが`Err`を返した場合、以前の値を維持するのではなく`true`（拒否側）へ設定するよう変更した。actorの`spawn_hook_active`チェックはこれにより1と独立した2つ目の防御層として、lookup障害中も安全側に倒れる。

1が主要な防御（ゼロ窓・lookup失敗時fail-closed）、2は多層防御として維持する（actorの既存チェック自体を無意味にしないため）。

**検証**（`tests/integration/tests/precommit_hook_2_2.rs`、いずれも実配線・実登録store経由）:

- `spawn_hook_registered_after_boot_is_enforced_without_waiting_for_a_cache_refresh`: `DynamicRegistrationStore`（構築後に`activate_spawn_hook()`で登録内容を変える実装）で、起動後の登録を`refresh_spawn_hook_active_cache`を一度も呼ばずに送ったTransformが即座に`EXPLICIT_SPAWN_REQUIRED`で拒否されることを確認——ギャップAの解消。
- `spawn_hook_cache_refresh_failure_fails_closed_for_both_explicit_spawn_and_auto_create`: `FailingThenActiveRegistrationStore`（`find_active_registration_by_capability`が指示するまでErrを返す）で、起動時リフレッシュ自体が失敗した状態でも、explicit SpawnEntity（`PRE_COMMIT_UNAVAILABLE`）とTransform auto-create（`EXPLICIT_SPAWN_REQUIRED`または`PRE_COMMIT_UNAVAILABLE`）の両方が正しくfail-closedすることを確認——ギャップBの解消。

### revisionとCore観測状態の束縛（2026-09-14更新）

- `RuntimeRegistry::read_entity`はowner task内で対象Entityだけを複製する。外側Noneは読取不能、内側Noneは対象不在。読取はforeground deadline内で打切り、失敗時はPRE_COMMIT_UNAVAILABLE。
- 署名要求に`current_entity`を追加。存在時は`entity_id`, `revision`, `owner_id`, `components`。componentsは予約`core.*`を除きnamespaceを保持し、`{encoding:"json"|"base64",value:...}`で送る。クライアントpayload内の同名フィールドを正本として読まない。
- Update/Deleteは、状態を外部へ渡す前にCoreの所有権/権限と要求revisionを確認する。古い値・省略値はフックを呼ばずREVISION_MISMATCH。従来はactor到達後に拒否していたため、この点は外部呼出し回数の変更となる。
- allow後に内部専用`WithExpectedEntityState`で観測Entityを束縛する。実mailbox処理の位置で現在Entity全体と比較し、不一致なら拒否する。その直後に元コマンドを処理するため、比較と適用の間に別コマンドは割り込まない。外部I/Oはactor内に入れない。
- Spawnは不在を渡し、不在のままであることを確定時に再確認。Update/Deleteは観測状態一致に加え、従来のexpected_revisionと権限検証も維持。同ID・同revisionで再生成された異なる所有者/状態にも古い承認を適用しない。対象削除時はENTITY_NOT_FOUND、その他の状態不一致はREVISION_MISMATCH。
- 判定の依存範囲は対象Entityのみ。他Entity・集計値・外部DBの変化はこの契約の保証対象ではない。複数Entityや分散トランザクションの保証は追加していない。
- 外部ルールは副作用なしで現在状態と要求だけを判定することを推奨する。Core内componentをルール状態の正本にすれば、外部予約/finalizeとの二重管理は不要。既存HTTP応答形状はallow/denyのままで、旧Extensionには追加フィールドを無視できる互換性がある。

### 権限・所有・参加状態のwindow — §8で部分的に解消

`run_connected_socket`が受け取る`permissions: WorldPermissions`は接続確立時（`ServerHello`前後）に一度だけ解決され、接続が切れるかCoreプロセスが再起動するまで再解決されない。これは本ADR以前からのCoreの既存の性質であり、フックの有無に関係なく成立している。

当初はこの性質を「フックが導入する新しい欠落ではない」として、再検証機構の追加を本改善のスコープ外とする判断を記していた。独立検証（worker-brief §8、優先項目1）はこの判断を「Coreに機構がないことは免責事由にならない」として差し戻し、`permission_revoked_during_pending_hook_wait_is_still_applied_with_the_stale_grant`という実行可能な再現テストを添えた。これを受け、フックが実際に呼ばれる経路に限定して同期的な再検証を追加した。

**解消策**（`realtime_ws_connection_runtime.rs`のフック呼び出しブロック内、`resolve_world_permissions`を再利用）:

1. **フック呼び出し直前**: `instance_command`構築後・`gate.validate`呼び出し前に、`state.world_authorizer`へ`resolve_world_permissions`で再問い合わせし、その結果を`instance_command.set_permissions(..)`で反映する。ハンドシェイク時の古いスナップショットではなく、この時点の実際の認可元の応答を使う。副次効果として、既に失効している場合はフックへのHTTP往復を無駄にしない。
2. **フック応答直後（Allow時のみ）**: `gate.validate`がAllowを返した直後、`command_durability_guard`取得前に、同じ`resolve_world_permissions`をもう一度呼び、`instance_command`の`permissions`を再度上書きする。フック待機中に失効・変更が起きた場合はこちらで捕捉される。

いずれも`InstanceCommand`側の変更は不要（`WorldPermissions`を上書きする`InstanceCommand::set_permissions`を追加しただけ）で、既存のactorのownership/permission判定（`is_owner_allowed`等）はそのまま使われる——確定させるのは常にactorであり、この修正はactorへ渡す入力を新鮮にするだけである。

**検証**（`tests/integration/tests/precommit_hook_2_2.rs`、いずれも実配線）:

- `permission_revoked_before_a_pending_hook_wait_is_not_applied_with_the_stale_grant`: ハンドシェイク後・フック呼び出し前に失効させるケース（検証担当の再現テストと同じ形）。`NOT_OWNER`で拒否されることを確認。
- `permission_revoked_strictly_after_hook_start_is_still_caught_before_commit`: `SignalingDelayGate`でフック呼び出しの開始を確実に観測してから失効させ、応答解放前という狭いwindowで発生した失効が、フック前の再確認では原理的に捕捉できないタイミングであることを保証したうえで、フック後の再確認が実際に捕捉することを確認する。

**検知できる範囲・できない範囲を正確に記す**（指示書「検知しないケースを隠さない」に対応）:

- 検知できる: 同じ`WorldAuthorizer`実装が`require`呼び出し時点で判定に反映する変更全般——ロール剥奪、権限テーブルの更新など、`WorldAuthorizer::require`が同期的に「参照する」情報。**§9で追加**: この接続自身の`presence_id`がinstanceからいなくなった場合（Leave）と、この接続の認証セッション自体が失効した場合（下記参照）も、同じ2箇所で検知するようになった。
- 検知できない: `WorldAuthorizer`自体が内部でキャッシュしていて反映が遅れる変更（本ADRの再検証は認可元を「呼ぶ」だけで、認可元自身の鮮度は保証しない）。
- 最後の再確認（フック応答直後）から実際の`registry.submit`によるactor確定までの間には、`command_durability_guard`取得と実際のmailbox投入という短い区間が残る。この区間はフックの待機時間（最大500ms）と比べて無視できるほど短く、フック追加前から存在した「コマンド受理から確定までの一瞬のwindow」と同じ性質のものである——ここをゼロにするには、権限チェックをactor内の確定処理と同一のcritical sectionに含める必要があり、それは本ラウンドのスコープを超える。

#### §9で追加: instance退出（Leave）の検知

権限失効の再検証とは別の関心事として据え置いていたが、独立検証の指摘（「実装不能とする場合は具体的ブロッカーを報告」）を受けて実経路を調査した結果、実装可能であることが分かったため実装した。

**解消策**: `permissions`の再確認と同じ2箇所（フック呼び出し直前・Allow応答直後）で、この接続自身の`presence_id`（`JoinAccepted.presence_id`としてクライアントへ返している値、`run_connected_socket`の`final_presence`引数）がまだinstanceのmemberかどうかを、`registry.is_present(instance_id, final_presence)`で確認する。これは`InstanceHandle`が`interest_views`/`member_count`と同じ`RwLock`キャッシュパターンで保持する`present_presences: Arc<RwLock<Arc<HashSet<PresenceId>>>>`の読み取りであり、actorへのround tripを追加しない（`registry.rs`、`refresh_runtime_cache`が`Submit`/`Tick`/`ReapIfIdle`のたびに更新）。`Some(true)`以外（`Some(false)`=退出済み、`None`=instance自体が見つからない）なら`PRESENCE_LOST`で拒否する。

**検証**（`tests/integration/tests/precommit_hook_2_2.rs`、実配線）:

- `presence_lost_during_pending_hook_wait_is_not_applied`: この接続自身の`presence_id`を`JoinAccepted`から取得し、フック呼び出し保留中に`registry.submit(instance_id, InstanceCommand::Leave{presence_id})`で実際にLeaveコマンドを投入したうえで、遅延していたフックのallow後にコマンドが`PRESENCE_LOST`で拒否されることを確認した。**注記**: 本番でこの状況（接続を維持したまま、その接続自身のpresenceだけが外部要因で失われる）を引き起こす実トリガーは現時点で存在しない（強制切断/admin kickの仕組み自体が未実装）。このテストは検知・拒否メカニズム自体（`is_present`キャッシュとWS層の2箇所チェック）が正しく機能することを、実際の`Leave`コマンド経路を通して検証しており、将来トリガーが追加された際にも同じ経路がそのまま機能する設計であることを示す。

#### §9で追加: 認証セッション失効の検知

前ラウンドでは「実装不能」として報告していたが、独立検証の指摘（「実装不能なら具体的な構造・必要な変更・保証範囲をブロッカーとして報告」）を受けて実装した。当初の調査で判明していた3点の変更を、既存公開契約の互換性を可能な限り保持する形で実施した。

**解消策**:

1. `orbisync_realtime::gateway::RealtimeTicketVerifier`に、`verify`と同じ挙動でセッションIDも返す新メソッド`verify_with_session`をデフォルト実装付きで追加した。デフォルト実装は`self.verify(ticket).await.map(|user_id| (user_id, None))`——**既存の5実装（`StubTicketVerifier`・`DenyAllTicketVerifier`・`AccessTokenTicketVerifier`・`orbisync-e2e-helper`の`FixedUserVerifier`・テストダブル）はすべて無改修のまま`verify`のみでコンパイル・動作し続ける**。セッションを実際に追跡する本番実装`HmacRealtimeTicketVerifier`のみ`verify_with_session`をオーバーライドし、`RealtimeTicketConsumption::Consumed{ user_id, session_id }`の`session_id`をそのまま返す（従来`..`で捨てていた値）。既存の`verify`メソッド自体は変更していない——trait契約の破壊的変更ではなく追加。
2. `realtime_ws_connection_auth.rs`のハンドシェイクが`verify`の代わりに`verify_with_session`を呼び、`Option<AuthSessionId>`を`AuthenticatedConnection`経由で`run_connected_socket`まで運ぶ。
3. 新しいportは追加していない——`RealtimeState`が既に保持している`identity_repository: Option<Arc<dyn IdentityRepository>>`（viewer roles解決に既存利用）の`find_session`をそのまま再利用する（`realtime_ws_state.rs`の新関数`is_session_still_active`）。`AuthSession::is_active_at(now)`（既存のdomainメソッド、status=ActiveかつexpiryのRevoked/期限切れも判定）で判定する。
4. 権限・参加状態の再確認と同じ2箇所（フック呼び出し直前・Allow応答直後）でこれを呼び、非activeなら`SESSION_REVOKED`で拒否する。`session_id`が`None`（セッション非追跡の verifier）または`identity_repository`が未配線の場合は「制限なし」として振る舞う——既存の全テスト・全デプロイの挙動を変えないオプトイン設計。lookup自体が失敗した場合も`false`（拒否）扱いとし、フェイルオープンにしない。

**検証**（`tests/integration/tests/precommit_hook_2_2.rs`、実配線、本番と同じ`HmacRealtimeTicketVerifier`+`RealtimeTicketStore`+`IdentityRepository`を使用、フェイクの`RealtimeTicketVerifier`で`session_id`を素通りさせる経路ではない）:

- `session_revoked_during_pending_hook_wait_is_not_applied_but_an_active_session_commits`: 実際にactiveな`AuthSession`を`FakeIdentityRepository`へ保存し、そのsessionへ紐づくticketで接続、まずセッションが有効なままspawnが正常に確定することを確認したうえで、フック呼び出しが保留中の別コマンドの最中に同じセッションを`revoke()`して保存し直し、保留していたフックのallow後にコマンドが`SESSION_REVOKED`で拒否されることを確認した。

**検知できる範囲**: 同じ`IdentityRepository`実装が`find_session`で返す`status`/`expires_at`に反映される変更——管理者によるセッション無効化、有効期限切れ。**検知できない範囲**: `IdentityRepository`自体のキャッシュ遅延（`WorldAuthorizer`の場合と同じ限界）。また、`session_id`を追跡しない`RealtimeTicketVerifier`実装（`StubTicketVerifier`等、ローカル開発・大半のテストで使用）ではこの検知は働かない——これは意図的なオプトインであり、既存挙動を壊さないための設計判断である。

### CommandDedupStoreとの相互作用

- **フックの前後関係**: `CommandDedupStore::begin`によるLeader/Duplicate/InFlight/Conflict/Capacity判定は、フック呼び出しよりも前に行われる。フックへ到達するのはLeaderのみ——Duplicate/InFlight/Conflict/Capacityはいずれもフックへ到達する前に`continue`する。
- **同一IDへの別payload**: `begin`はcommand_idごとにpayloadのfingerprint（SHA-256）も比較する。同一`command_id`で異なるpayloadを送ると`Conflict`となり、フックへは到達しない（既存の`command_dedup`の性質をそのまま利用しており、フック追加による変更はない）。
- **Duplicate待ち（InFlight）**: 同一command_idの処理が既に進行中の別リクエストは、フックの結果を待つのではなく、Leaderの最終結果（`CommandDedupResult`）を待つ。フックの判定結果そのものは`CommandDedupResult`に含まれず、Leaderが確定した後続の結果（Applied/Errorなど）だけが共有される。
- **timeout/拒否時のLeader枠の解放**: フックがdenyまたはlookupエラーを返した場合、コード上は`continue`し、その時点で`command_dedup_reservation`（`Option<CommandDedupReservation>`、ループのこのブランチ内のローカル変数）がスコープを抜けてdropされる。`CommandDedupReservation`は`complete`されないままdropされると`Drop`実装が`remove_pending`を呼び、pendingエントリを完全に削除する（`command_dedup.rs`）。したがって**denyされたcommand_idの枠はリークしない**——同じcommand_idを再送すると新しいLeaderとして再度admitされ、フックも再度呼ばれる（`tests/integration/tests/precommit_hook_2_2.rs`の`a_denied_commands_slot_is_released_so_a_resend_consults_the_hook_again`で実配線検証済み）。これはAllow確定後の挙動（`complete`されるため、再送はフックを再度呼ばずLeaderの結果をそのまま再生する、`duplicate_command_id_calls_the_hook_at_most_once`で検証済み）とは対照的であり、意図した非対称性である——「拒否の結果」は再現性を保証する契約の対象にしていない。
- **cancel/shutdownでの回収**: 本コードベースには、フック待機中の接続task自体を外部から`abort()`する経路は存在しない（`run_connected_socket`は単一の`tokio::select!`ループで、`msg = socket.recv()`ブランチの中でフックの`.await`を含む処理全体を完了させてから次のselectへ戻る。したがってTCPソケットが閉じても、shutdown通知が来ても、フック待機中はそのブランチの完了まで検知されない）。真にtaskがdropされる経路（プロセス終了によるランタイム停止など）では、`command_dedup_reservation`・フックの`Semaphore`許可はいずれもローカル変数/RAIIガードであり、他のtaskへdetach（`tokio::spawn`）されていないため、Rustの通常のdrop順序でどちらも解放される。この構造的な保証に加えて、本番で使われる唯一の`PreCommitGate`実装（`PreCommitValidationGate`）はフック呼び出し自体を内部の`tokio::time::timeout`で必ず自己制限するため（後述「Heartbeat/接続timeoutとの関係」）、フック待機が無期限に継続するコード経路は存在しない。**ただし**`PreCommitGate` traitは呼び出し元（`realtime_ws_connection_runtime.rs`）側で追加のtimeoutを課さないため、将来自己制限しない`PreCommitGate`実装を追加した場合はこの保証が崩れる——traitの実装者はセルフバウンドが必須という制約を、trait定義のdoc commentに明記する。

### Heartbeat/接続timeoutとの関係

フックの`.await`は`run_connected_socket`の単一`tokio::select!`ループの1ブランチ内で実行される。フック呼び出しが長引く間、そのconnectionの心拍送受信・timeoutチェックは同じブランチが完了するまで進まない——他のconnection（`slow_hook_does_not_block_unrelated_operations_on_the_same_instance`で検証済み）や他のinstanceの処理は影響を受けないが、**そのconnection自身**の心拍処理はフックの待機時間分だけ遅延する。

既定値で比較すると、フックのtimeout（`extensions.pre_commit_validation_timeout_ms`、既定500ms、未実測の暫定値）は`realtime.heartbeat_interval_seconds`（既定20秒、設定可能範囲`5〜120`秒）や`realtime.connection_timeout_seconds`（既定60秒、`heartbeat_interval_seconds`より大きい値を要求するバリデーション済み）に対して二桁小さく、既定設定では心拍timeoutを誘発する実害はない。ただし`pre_commit_validation_timeout_ms`は`realtime.connection_timeout_seconds`の設定と独立にバリデーションされており、運用者が両方を極端な値へ変更した場合（例えば`heartbeat_interval_seconds`を最小の5秒にし、`pre_commit_validation_timeout_ms`を数秒単位まで引き上げた場合）、フック待機がそのconnection自身の心拍timeoutと競合する余地は理論上残る。本ADRはこれを自動で防止するcross-fieldバリデーションを追加しない（`timeout_ms < connection_timeout_seconds * 1000`のような単純な比較は、59秒 vs 60秒のような無意味な組み合わせも許してしまい、実効的な安全策にならない）。代わりに、運用ガイド（`orbisync.toml.example`のコメント）で「`pre_commit_validation_timeout_ms`は`realtime.heartbeat_interval_seconds`より十分小さく保つこと」を明記する。

### 確定順序の明示

1コマンドの処理順序を明示する（`realtime_ws_connection_runtime.rs`）。

1. `CommandDedupStore::begin`によるLeader admission（`command_durability_guard`取得より前）。
2. `expected_revision`をクライアントの送信値から一度だけ計算（このスナップショットはCore側の追加読み取りではなく、クライアント主張値の確定タイミングを指す）。
3. フックの呼び出しと応答待ち（`command_durability_guard`取得より前——instance単位のdurability mutexは保持しない）。
4. フックがdenyまたはlookupエラーなら、ここで`continue`（枠は上記の通り解放される）。
5. `command_durability_guard`の取得。
6. `registry.submit`によるactorへの投入。actor内で`ensure_matches(expected_revision)`等の既存チェックを実行し、最終確定または拒否。

この順序では、手順3のフック待機中に手順6のrevisionチェックが依拠する状態が変化しても（例えば別connectionが先に同じentityを更新してrevisionを進めても）、手順6は独立して現在の実際の状態と比較するため、古い承認がそのまま適用されることはない。`stale_revision_during_pending_hook_wait_is_not_applied`（`tests/integration/tests/precommit_hook_2_2.rs`）は、フック応答を意図的に遅延させ、その間に別connectionから競合するupdateを完了させたうえで、遅延していたフックがallowを返しても後発の承認が拒否されることを実配線で検証している。

## Alternatives

- **Webhook経由の同期化（配送成功までブロック）**: ADR-007の「配送成功までtransactionを保持しない」方針に反し、外部障害がCoreを停止させる。不採用。
- **Extension Command APIの拡張として実装（Extensionが能動的にポーリング）**: 状態確定前という同期性が実現できない（Extensionが確定前にポーリングする保証がない）。不採用。
- **actor内での直接HTTP呼び出し**: `InstanceActor::handle`の「No I/O」不変条件（`architecture.md` §10 ARC-02、`state-and-runtime.md` §3.1）に反する。不採用。
- **複数拡張の合議（AND/OR/優先順位）を初期版から実装**: 未確定の設計判断が多く、初期版のスコープを超える。将来ADRへ。

## Consequences

- Core本体を改造せずに、アプリ固有の検証ルール（禁止区域判定、編集権限の細分化等）をentity mutationコマンドに対して強制できる。
- オプトインかつ既定offのため、フックを使わないデプロイの既存挙動・既存test・既存E2Eは無変更で合格する。
- Transformストリーム（20Hz）はフック対象外であり、リアルタイム位置に対する外部ルール強制は本ADRでは実現しない。ドローン・シミュレーター用途の一部（飛行中の逐次位置検証）はカバーされない。
- 1 `InstanceCommand` = 1 actor stepが原子性の単位であり、複数コマンドにまたがる原子更新（例: entity削除＋score更新の同時原子性）は保証しない。
- フックの判定はrevision進行を検知しないため、フックallow後にactor側でrevision不一致拒否が起こり得る（想定内の挙動、クライアントは通常のrevision競合として再試行する）。
- 1コマンド種別につき高々1件のActive拡張という制限は、複数の独立したルールセットを同時に強制したいデプロイには不十分であり、将来の拡張が必要。

## Migration

- `docs/design/extension-mechanism.md`へ第3の方向を追記する節を新設する（既存節は変更しない）。
- `docs/design/state-and-runtime.md` §2.2へ、フックがCoreの不変条件を迂回しないことの参照を追記する。
- `ExtensionsConfig`（`orbisync-config`）へ新規キーを追加し、ADR-024と同じパターン（省略時デフォルト、TOML/環境変数からの上書き、型検証）でconfig testを追加する。
- `orbisync-extensions`に、`delivery.rs`のSSRF境界・署名ロジックを再利用した同期問い合わせの実装を追加する。
- `realtime_ws_connection_runtime.rs`へ実配線し、`docs/plans/generalization-2.2-pre-commit-validation-hook.md`のテスト表（T1〜T15）を実装する。
- `examples/`に、禁止区域判定を行う最小のローカルHTTPサーバー例を追加する。
