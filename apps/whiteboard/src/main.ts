import { OrbiSyncClient, uuidv7, type SyncedEntity, type OrbiSyncInstance } from "../../../sdk/typescript/src/client.js";
import { notePayload } from "../rules/whiteboard-state.mjs";
import { ConfirmedCommands, CommandUncertain, type Request } from "./confirmed_commands.js";
import { NoteBoard, type Note } from "./note_board.js";
import { CursorPages } from "../../shared/cursor_pages.mjs";
import { resourcePage, type World, type WorldInstance } from "./resources.js";
import "./style.css";
const $=<T extends HTMLElement>(id:string)=>document.getElementById(id) as T;
let client:OrbiSyncClient|null=null;let instance:OrbiSyncInstance|null=null;let joined=false;const notes=new Map<string,Note>();let commands:ConfirmedCommands|null=null;const colors=["yellow","rose","mint","blue"];
const worldPages=new CursorPages<World>();const instancePages=new CursorPages<WorldInstance>();let authGeneration=0;let loginBusy=false;let joinBusy=false;
const boardView=new NoteBoard($("board"),{
  text:(id,text)=>{const note=notes.get(id);if(!note||!joined||!commands)return false;update(note,{text});return true},
  lock:id=>{const note=notes.get(id);if(note)toggleLock(note)},
  remove:id=>{const note=notes.get(id);if(note)remove(note)},
  drag:(event,element,id)=>{const note=notes.get(id);if(note)drag(event,element,note)},
});
function log(s:string){const e=$("log") as HTMLPreElement;e.textContent=`${new Date().toLocaleTimeString()}  ${s}\n${e.textContent}`}
function message(s:string){$("authMessage").textContent=s;log(s)}
function toNote(entity:SyncedEntity):Note|null {
  const component=entity.properties["com.orbisync.whiteboard.note"];
  if(!component||typeof component!=="object"||Array.isArray(component)||component.encoding!=="json")return null;
  const value=component.value;
  if(!value||typeof value!=="object"||Array.isArray(value))return null;
  if(typeof value.text!=="string"||typeof value.color!=="string"||typeof value.position_x!=="number"||typeof value.position_y!=="number")return null;
  return {id:entity.entityId,text:value.text,color:colors.includes(value.color)?value.color:"yellow",x:value.position_x,y:value.position_y,locked:value.locked===true,revision:entity.revision};
}
function refreshState(){notes.clear();for(const entity of instance?.state.entities.values()??[]){const note=toNote(entity);if(note)notes.set(note.id,note)}render()}
function syncControls(){joined=instance?.syncStatus==="ready";($("add") as HTMLButtonElement).disabled=!joined;$("syncState").textContent=joined?"同期完了":"同期を待っています";render();renderCreations()}
function render(){boardView.render(notes.values(),joined,id=>!!commands?.hasPending(id));$("count").textContent=String(notes.size)}
// Every command is bound to the revision Core last confirmed, which the
// pre-commit rule compares against the revision it read itself.
function latest(n:Note){return notes.get(n.id)??n}
function command(n:Note,op:string,args:Record<string,unknown>){if(!commands||!joined)return;const owner=commands;const request=owner.request({entityId:n.id,operation:op,expectedRevision:n.revision,args});void owner.send(request).catch(e=>{if(owner!==commands)return;message(`${e instanceof CommandUncertain?"結果不明・同じ操作を再送できます":"操作拒否"}: ${e.message}`);refreshState()})}
// Core replaces the note component wholesale, so notePayload stays the single
// builder for every mutation: a partial payload would drop the locked flag
// that the pre-commit rule treats as authoritative.
function update(n:Note,p:Partial<Note>){const current=latest(n);command(current,"update",notePayload(current,p));render()}
// The UI decides no lock. Lock and unlock are ordinary component updates at
// the latest confirmed revision; Core answers with a confirmation or with
// PRE_COMMIT_DENIED, and only that answer changes what is shown.
function toggleLock(n:Note){update(n,{locked:!latest(n).locked})}
function remove(n:Note){command(latest(n),"delete",{});render()}
function drag(e:PointerEvent,el:HTMLElement,n:Note){
  if(!joined||commands?.hasPending(n.id)||e.button!==0||(e.target as HTMLElement).closest("button,textarea,input,select,a"))return;
  const sx=e.clientX,sy=e.clientY,ox=n.x,oy=n.y;
  let x=ox,y=oy;
  el.setPointerCapture(e.pointerId);
  const move=(event:PointerEvent)=>{
    if(event.pointerId!==e.pointerId)return;
    x=Math.max(8,ox+event.clientX-sx);y=Math.max(8,oy+event.clientY-sy);
    el.style.left=`${x}px`;el.style.top=`${y}px`;
  };
  const finish=(event:PointerEvent)=>{
    if(event.pointerId!==e.pointerId)return;
    el.removeEventListener("pointermove",move);
    el.removeEventListener("pointerup",finish);
    el.removeEventListener("pointercancel",finish);
    if(el.hasPointerCapture(e.pointerId))el.releasePointerCapture(e.pointerId);
    if(event.type==="pointerup"&&(x!==ox||y!==oy))update(notes.get(n.id)??n,{x,y});
    else render();
  };
  el.addEventListener("pointermove",move);
  el.addEventListener("pointerup",finish);
  el.addEventListener("pointercancel",finish);
}
// 参加方式の選択（ADR-026）。どの方式もSDKが同じtoken対を保持するので、
// 以降のticket取得・接続・入室の経路は方式によらず共通のまま。
function selectedMethod():"local"|"guest"|"name_only"{return ($("authMethod") as HTMLSelectElement).value as "local"|"guest"|"name_only"}
function syncAuthFields(){const m=selectedMethod();($("localFields") as HTMLElement).hidden=m!=="local";($("nameFields") as HTMLElement).hidden=m!=="name_only"}
// サーバーが実際に受け付ける方式だけを選べるようにする。無効な方式を選んで
// 403を受け取るより、選択肢から消えている方が利用者にとって分かりやすい。
async function refreshMethods(){const generation=authGeneration;const server=($("server") as HTMLInputElement).value.trim();try{const r=await fetch(`${server}/v1/auth/methods`);if(!r.ok)return;const d=await r.json() as {methods?:string[]};if(generation!==authGeneration||server!==($<HTMLInputElement>("server").value.trim()))return;const allowed=new Set(d.methods??[]);const s=$("authMethod") as HTMLSelectElement;for(const o of Array.from(s.options))o.hidden=!allowed.has(o.value);if(s.selectedOptions[0]?.hidden){const first=Array.from(s.options).find(o=>!o.hidden);if(first)s.value=first.value}syncAuthFields()}catch{/* 未起動なら既定の選択肢のまま */}}
function resourceControls(){
  ($("login") as HTMLButtonElement).disabled=loginBusy;
  ($("load") as HTMLButtonElement).disabled=!client||worldPages.busy||instancePages.busy;
  ($("moreWorlds") as HTMLButtonElement).disabled=!client||worldPages.busy||!worldPages.nextCursor;
  ($("moreInstances") as HTMLButtonElement).disabled=!client||instancePages.busy||!instancePages.nextCursor;
  ($("join") as HTMLButtonElement).disabled=joinBusy||!client||!($<HTMLSelectElement>("instances").value);
}
function renderResources(){
  const worldSelect=$<HTMLSelectElement>("worlds"),previousWorld=worldSelect.value;
  const query=$<HTMLInputElement>("worldFilter").value.trim().toLocaleLowerCase();
  worldSelect.replaceChildren(new Option("すべてのWorld", ""));
  for(const world of worldPages.items){if(!`${world.id} ${world.name} ${world.status}`.toLocaleLowerCase().includes(query))continue;worldSelect.add(new Option(`${world.name} (${world.status})`,world.id))}
  if(Array.from(worldSelect.options).some(o=>o.value===previousWorld))worldSelect.value=previousWorld;
  const instanceSelect=$<HTMLSelectElement>("instances"),previousInstance=instanceSelect.value;
  const instanceQuery=$<HTMLInputElement>("instanceFilter").value.trim().toLocaleLowerCase();
  instanceSelect.replaceChildren();
  for(const item of instancePages.items){if(worldSelect.value&&item.world_id!==worldSelect.value)continue;if(!`${item.id} ${item.status}`.toLocaleLowerCase().includes(instanceQuery))continue;instanceSelect.add(new Option(`${item.id.slice(0,8)}… (${item.status})`,item.id))}
  if(!instanceSelect.options.length)instanceSelect.add(new Option("該当なし・続きを読み込めます", ""));
  if(Array.from(instanceSelect.options).some(o=>o.value===previousInstance))instanceSelect.value=previousInstance;
  $("pageCount").textContent=`World ${worldPages.items.length}件 / Instance ${instancePages.items.length}件 読み込み済み`;
  resourceControls();
}
function resetResources(){worldPages.reset();instancePages.reset();renderResources()}
async function leaveBoard(){
  const previous=instance;instance=null;joined=false;commands?.dispose();commands=null;creations.clear();notes.clear();syncControls();
  if(previous)await previous.leave();
}
function invalidateServer(){
  ++authGeneration;loginBusy=false;client=null;resetResources();
  $("connection").textContent="未接続";$("connection").className="status offline";
  void leaveBoard().catch(error=>log(`切断: ${error instanceof Error?error.message:String(error)}`));
}
async function login(){
  if(loginBusy)return;
  const generation=++authGeneration;loginBusy=true;client=null;resetResources();
  const server=$<HTMLInputElement>("server").value.trim(),method=selectedMethod();
  try{
    const candidate=new OrbiSyncClient({baseUrl:server});
    await leaveBoard();
    if(generation!==authGeneration)return;
    if(method==="guest")await candidate.auth.guest();
    else if(method==="name_only"){const name=$<HTMLInputElement>("displayName").value.trim();if(!name)throw new Error("表示名を入力してください。");await candidate.auth.nameOnly({displayName:name})}
    else await candidate.auth.login({loginId:$<HTMLInputElement>("loginId").value.trim(),password:$<HTMLInputElement>("password").value});
    if(generation!==authGeneration)return;
    client=candidate;$<HTMLInputElement>("password").value="";
    $("connection").textContent="認証済み";$("connection").className="status online";
    message("参加しました。World / Instance一覧を読み込めます");
  }catch(error){if(generation!==authGeneration)return;const disabled=(error as {code?:string})?.code==="AUTH_METHOD_DISABLED";message(disabled?"この参加方式はサーバーで有効になっていません。":`参加失敗: ${error instanceof Error?error.message:String(error)}`)}
  finally{if(generation===authGeneration)loginBusy=false;resourceControls()}
}
async function loadWorldPage(){
  const source=client;if(!source)return;
  const pending=worldPages.load(cursor=>resourcePage<World>(source,"worlds",cursor));resourceControls();
  try{if(await pending&&client===source)renderResources()}
  finally{resourceControls()}
}
async function loadInstancePage(){
  const source=client;if(!source)return;
  const pending=instancePages.load(cursor=>resourcePage<WorldInstance>(source,"instances",cursor));resourceControls();
  try{if(await pending&&client===source)renderResources()}
  finally{resourceControls()}
}
async function resourceAction(action:()=>Promise<unknown>){try{await action()}catch(error){message(`一覧取得失敗: ${error instanceof Error?error.message:String(error)}。同じボタンで再試行できます`)}finally{resourceControls()}}
async function load(){
  if(!client)return message("先にログインしてください。");
  resetResources();
  // A guest may list joinable Instances without permission to list Worlds.
  await Promise.all([resourceAction(loadWorldPage),resourceAction(loadInstancePage)]);
}
async function join(){
  if(joinBusy)return;
  if(!client)return message("先にログインしてください。");
  const source=client,generation=authGeneration;
  const id=($("instances") as HTMLSelectElement).value;if(!id)return message("instanceを選択してください。");
  joinBusy=true;resourceControls();
  try{
    const abandoned=commands?.unresolvedCount??0;
    if(abandoned)message(`前の入室の未確定操作${abandoned}件の追跡を終了します。次の入室へ再送しません。既に送信した操作の取消ではありません。`);
    const previous=instance;instance=null;joined=false;commands?.dispose();commands=null;creations.clear();notes.clear();syncControls();if(previous)await previous.leave();
    const conn=await source.connectWithRetry();
    if(source!==client||generation!==authGeneration){await conn.disconnect();return}
    try { const candidate=await conn.join(id);if(source!==client||generation!==authGeneration){await conn.disconnect();return}instance=candidate; } catch(error) { await conn.disconnect();throw error; }
    const current=instance;commands=new ConfirmedCommands(current, ()=>{render();renderCreations()}, (confirmed, request)=>{queueMicrotask(()=>{
      if(instance!==current||!commands)return;
      const creation=creations.get(request.entityId);
      if(creation && creation.request.commandId===request.commandId && !creation.busy){
        if(creation.phase==="spawn"){creation.phase="update";creation.request=commands!.request({entityId:creation.note.id,operation:"update",expectedRevision:confirmed.expectedRevision,args:notePayload(creation.note)});void completeCreation(creation)}
        else creations.delete(request.entityId);
      }
      refreshState();renderCreations();
    })});
    for(const event of ["snapshot","entityUpdated","entitySpawned","entityDeleted"])current.on(event,()=>{if(instance===current)refreshState()});
    current.on("syncStateChanged",()=>{if(instance===current)syncControls()});
    current.on("error",e=>{const error=e as {code?:string;message?:string};message(`同期/操作エラー ${error.code??""}: ${error.message??String(e)}`)});
    syncControls();await current.ready();if(instance!==current)return;
    refreshState();syncControls();$("connection").textContent="接続中";message("入室しました。変更はOrbiSyncの確定後に共有されます");
  }catch(error){message(`入室失敗: ${error instanceof Error?error.message:String(error)}`);syncControls()}
  finally{joinBusy=false;resourceControls()}
}
type Creation={note:Note;request:Request;phase:"spawn"|"update";busy:boolean;error:string};
const creations=new Map<string,Creation>();
function renderCreations(){const root=$("pendingNotes");root.replaceChildren();for(const creation of creations.values()){
  const row=document.createElement("p");row.textContent=creation.busy?(creation.phase==="spawn"?"付箋を作成中…":"本文・色を保存中…"):`保存未確認: ${creation.error}`;
  if(!creation.busy){const retry=document.createElement("button");retry.textContent="再送";retry.disabled=!joined;retry.onclick=()=>void completeCreation(creation);row.appendChild(retry)}root.appendChild(row)
}
  for(const item of commands?.uncertain??[]){
    if([...creations.values()].some(c=>c.request.commandId===item.request.commandId))continue;
    const row=document.createElement("p");row.textContent=`結果不明 (${item.request.operation}): ${item.error.code} `;
    const retry=document.createElement("button");retry.textContent=item.busy?"確認中…":"同じ操作を再送";retry.disabled=!joined||item.busy;
    retry.onclick=()=>{const owner=commands;void owner?.send(item.request).catch(error=>{if(owner===commands)message(`${error instanceof CommandUncertain?"結果不明":"操作拒否"}: ${error.message}`)})};
    row.appendChild(retry);root.appendChild(row);
  }
}
async function completeCreation(creation:Creation){
  if(!commands||!joined||creation.busy)return;const owner=commands;creation.busy=true;creation.error="";renderCreations();
  try{
    const confirmed=await owner.send(creation.request);
    if(owner!==commands)return;
    if(creation.phase==="spawn"){
      creation.phase="update";creation.request=owner.request({entityId:creation.note.id,operation:"update",expectedRevision:confirmed.expectedRevision,args:notePayload(creation.note)});
      renderCreations();await owner.send(creation.request);
    }
    if(owner!==commands)return;creations.delete(creation.note.id);refreshState();message("本文・色を保存しました");
  }catch(error){if(owner!==commands)return;creation.error=error instanceof Error?error.message:String(error);if(!(error instanceof CommandUncertain)){creations.delete(creation.note.id);message(`操作拒否: ${creation.error}`)}}
  finally{creation.busy=false;renderCreations()}
}
function add(){if(!joined||!commands)return;if(creations.size>=16)return message("未確定の付箋を先に再送してください");
  const note:Note={id:uuidv7(),text:"ここにアイデアを書く",x:80+Math.random()*420,y:50+Math.random()*300,color:colors[notes.size%colors.length]!,locked:false,revision:0n};
  const creation:Creation={note,phase:"spawn",busy:false,error:"",request:commands.request({entityId:note.id,operation:"spawn",expectedRevision:0n,args:notePayload(note)})};
  creations.set(note.id,creation);void completeCreation(creation)
}
$("authMethod").addEventListener("change",syncAuthFields);
$("server").addEventListener("input",invalidateServer);
$("server").addEventListener("change",()=>void refreshMethods());
$("login").addEventListener("click",()=>void login());$("load").addEventListener("click",()=>void load());
$("moreWorlds").addEventListener("click",()=>void resourceAction(loadWorldPage));
$("moreInstances").addEventListener("click",()=>void resourceAction(loadInstancePage));
$("worldFilter").addEventListener("input",()=>{worldPages.reset();instancePages.reset();renderResources();void Promise.all([resourceAction(loadWorldPage),resourceAction(loadInstancePage)])});
for(const [id,event] of [["worlds","change"],["instanceFilter","input"]])$(id!).addEventListener(event!,()=>{instancePages.reset();renderResources();void resourceAction(loadInstancePage)});
$("instances").addEventListener("change",resourceControls);
$("join").addEventListener("click",()=>void join());$("add").addEventListener("click",add);render();renderResources();
