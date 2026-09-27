import { OrbiSyncClient, uuidv7, type SyncedEntity, type OrbiSyncInstance } from "../../../sdk/typescript/src/client.js";
import { notePayload } from "../rules/whiteboard-state.mjs";
import { ConfirmedCommands, CommandUncertain, type Request } from "./confirmed_commands.js";
import "./style.css";
type Note={id:string;text:string;x:number;y:number;color:string;locked:boolean;revision:bigint};
const $=<T extends HTMLElement>(id:string)=>document.getElementById(id) as T;
let client:OrbiSyncClient|null=null;let instance:OrbiSyncInstance|null=null;let joined=false;const notes=new Map<string,Note>();let commands:ConfirmedCommands|null=null;const colors=["yellow","rose","mint","blue"];
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
function render(){const b=$("board");b.replaceChildren();for(const n of notes.values()){const el=document.createElement("article");el.className=`note ${n.color} ${n.locked?"locked":""}`;el.style.left=`${n.x}px`;el.style.top=`${n.y}px`;el.innerHTML=`<div class="note-head"><span>${n.locked?"🔒":"✦"}</span><span><button class="lock-button" aria-label="${n.locked?"ロック解除":"ロック"}">${n.locked?"解除":"ロック"}</button><button aria-label="削除">×</button></span></div><textarea aria-label="付箋本文"></textarea><small>${n.locked?"ロック中":"未ロック"}</small>`;const ta=el.querySelector("textarea")!;ta.value=n.text;ta.disabled=!joined||!!commands?.hasPending(n.id);for(const button of Array.from(el.querySelectorAll("button")))button.disabled=!joined||!!commands?.hasPending(n.id);ta.addEventListener("change",()=>update(n,{text:ta.value}));el.querySelector(".lock-button")!.addEventListener("click",()=>toggleLock(n));el.querySelectorAll("button")[1]!.addEventListener("click",()=>remove(n));el.addEventListener("pointerdown",e=>drag(e,el,n));b.appendChild(el)}$("count").textContent=String(notes.size)}
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
async function refreshMethods(){const server=($("server") as HTMLInputElement).value.trim();try{const r=await fetch(`${server}/v1/auth/methods`);if(!r.ok)return;const d=await r.json() as {methods?:string[]};const allowed=new Set(d.methods??[]);const s=$("authMethod") as HTMLSelectElement;for(const o of Array.from(s.options))o.hidden=!allowed.has(o.value);if(s.selectedOptions[0]?.hidden){const first=Array.from(s.options).find(o=>!o.hidden);if(first)s.value=first.value}syncAuthFields()}catch{/* 未起動なら既定の選択肢のまま */}}
async function login(){const server=($("server") as HTMLInputElement).value.trim();client=new OrbiSyncClient({baseUrl:server});const method=selectedMethod();try{if(method==="guest")await client.auth.guest();else if(method==="name_only"){const name=($("displayName") as HTMLInputElement).value.trim();if(!name)return message("表示名を入力してください。");await client.auth.nameOnly({displayName:name})}else await client.auth.login({loginId:($("loginId") as HTMLInputElement).value.trim(),password:($("password") as HTMLInputElement).value});$("connection").textContent="認証済み";$("connection").className="status online";message(method==="local"?"ログインしました。instance一覧を読み込めます":"参加しました。instance一覧を読み込めます")}catch(e){const disabled=(e as {code?:string})?.code==="AUTH_METHOD_DISABLED";message(disabled?"この参加方式はサーバーで有効になっていません。":`参加失敗: ${e instanceof Error?e.message:String(e)}`)}}
async function load(){if(!client)return message("先にログインしてください。");try{const r=await fetch(`${client.getBaseUrl()}/v1/instances`,{headers:{Authorization:`Bearer ${client._getAccessToken()}`}});if(!r.ok)throw Error(`instance一覧 ${r.status}`);const d=await r.json() as {items?:Array<{id:string;world_id:string;status:string;revision:number}>};const s=$("instances") as HTMLSelectElement;s.replaceChildren();for(const i of d.items??[]){const o=document.createElement("option");o.value=i.id;o.textContent=`${i.id.slice(0,8)}… (${i.status})`;s.appendChild(o)}message(`${d.items?.length??0}件読み込みました`)}catch(e){message(`一覧取得失敗: ${e instanceof Error?e.message:String(e)}`)}}
async function join(){
  if(!client)return message("先にログインしてください。");
  const id=($("instances") as HTMLSelectElement).value;if(!id)return message("instanceを選択してください。");
  ($("join") as HTMLButtonElement).disabled=true;
  try{
    const abandoned=commands?.unresolvedCount??0;
    if(abandoned)message(`前の入室の未確定操作${abandoned}件の追跡を終了します。次の入室へ再送しません。既に送信した操作の取消ではありません。`);
    const previous=instance;instance=null;joined=false;commands?.dispose();commands=null;creations.clear();notes.clear();syncControls();if(previous)await previous.leave();
    const conn=await client.connectWithRetry();
    try { instance=await conn.join(id); } catch(error) { await conn.disconnect();throw error; }
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
  finally{($("join") as HTMLButtonElement).disabled=false}
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
$("authMethod").addEventListener("change",syncAuthFields);$("server").addEventListener("change",()=>void refreshMethods());$("login").addEventListener("click",()=>void login());$("load").addEventListener("click",()=>void load());$("join").addEventListener("click",()=>void join());$("add").addEventListener("click",add);render();
