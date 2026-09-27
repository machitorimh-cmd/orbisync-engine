import { it } from "node:test";
import assert from "node:assert/strict";
import { ConfirmedCommands, CommandUncertain, CommandRejected } from "./confirmed_commands.js";
import type { OrbiSyncInstance } from "../../../sdk/typescript/src/client.js";
class Instance {
  handlers = new Map<string, Set<(value: unknown) => void>>();
  sent: unknown[] = [];
  immediate = false;
  state = { entities: new Map<string, {revision: bigint}>() };
  on(event: string, handler: (value: unknown) => void) { if(!this.handlers.has(event))this.handlers.set(event,new Set());this.handlers.get(event)!.add(handler); }
  off(event: string, handler: (value: unknown) => void) { this.handlers.get(event)?.delete(handler); }
  emit(event: string, value: unknown) { for(const handler of this.handlers.get(event)??[])handler(value); }
  sendEntityCommand(request: { commandId: string; entityId: string; operation: string }) { this.sent.push(request);if(this.immediate)this.emit("entityCommand",{commandId:request.commandId,entityId:request.entityId,operation:request.operation,expectedRevision:1n}); }
}
it("caps awaiting plus uncertain payloads; only confirmed results release capacity", async () => {
  const instance = new Instance(), commands = new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const pending: Promise<unknown>[] = [];
  for (let i = 0; i < 18; i++) {
    const request = commands.request({ entityId: `a${i}`, operation: "update", args: { text: "x".repeat(30000) } });
    pending.push(commands.send(request).catch(error => error));
  }
  assert.ok(instance.sent.length < 18);
  instance.emit("syncStateChanged", "reconnecting");
  await Promise.all(pending);
  instance.immediate = true;
  assert.ok(commands.uncertain.length > 0);
  for (const entry of commands.uncertain) await commands.send(entry.request);
  await commands.send(commands.request({ entityId: "next", operation: "update", args: { text: "x".repeat(30000) } }));
  commands.dispose();
});
it("registers correlation before sending and resolves only the matching confirmation", async () => {
  const instance=new Instance();instance.immediate=true;
  const commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const request=commands.request({entityId:"a",operation:"spawn",expectedRevision:0n});
  assert.equal((await commands.send(request)).expectedRevision,1n);
  commands.dispose();
});
it("disconnect settles pending operations; retry preserves the exact id and request",async()=>{
  const instance=new Instance(),commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const request=commands.request({entityId:"a",operation:"update",expectedRevision:1n,args:{text:"body"}});
  const failed=assert.rejects(commands.send(request),CommandUncertain);
  instance.emit("syncStateChanged","reconnecting");await failed;
  instance.immediate=true;await commands.send(request);
  assert.equal(instance.sent[0],instance.sent[1]);
  commands.dispose();assert.equal([...instance.handlers.values()].reduce((sum,set)=>sum+set.size,0),0);
});
it("a correlated Core rejection cannot be mistaken for persistence",async()=>{
  const instance=new Instance(),commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const request=commands.request({entityId:"a",operation:"update",expectedRevision:1n});
  const failed=assert.rejects(commands.send(request),/PRE_COMMIT_DENIED/);
  instance.emit("error",{requestMessageId:request.commandId,code:"PRE_COMMIT_DENIED"});await failed;commands.dispose();
});

it("all mutations retain exact requests through retryable failure and a newer snapshot", async () => {
  for (const operation of ["spawn", "update", "delete"]) {
    const instance = new Instance(); instance.state.entities.set("a", {revision:1n});
    let settled = 0;
    const commands = new ConfirmedCommands(instance as unknown as OrbiSyncInstance, () => {}, () => {settled++;});
    const args = { text:"original", color:"mint", locked:false };
    const request = commands.request({entityId:"a",operation,args});
    args.text="mutated by caller";
    const failed = assert.rejects(commands.send(request), error => error instanceof CommandUncertain && error.code === "PERSISTENCE_UNAVAILABLE");
    instance.emit("error", {requestMessageId:request.commandId,code:"PERSISTENCE_UNAVAILABLE",retryable:true});
    await failed;
    assert.equal(settled,0); assert.equal(commands.uncertain.length,1);
    instance.state.entities.set("a", {revision:2n}); instance.emit("syncStateChanged","ready");
    assert.equal(commands.uncertain.length,1,"snapshot ready is not a command receipt");
    instance.immediate=true; await commands.send(commands.uncertain[0]!.request);
    assert.equal(instance.sent[0],instance.sent[1]); assert.equal(request.expectedRevision,1n);
    assert.equal(request.args!.text,"original"); assert.equal(settled,1); assert.equal(commands.uncertain.length,0);
    commands.dispose();
  }
});
it("late matching confirmation resolves uncertain without another send; rejection is definitive",async()=>{
  const instance=new Instance(); let settled=0;
  const commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance,()=>{},()=>{settled++;});
  const request=commands.request({entityId:"a",operation:"update",expectedRevision:1n});
  const failed=assert.rejects(commands.send(request),CommandUncertain);
  instance.emit("error",{requestMessageId:request.commandId,code:"PERSISTENCE_UNAVAILABLE",retryable:true});await failed;
  instance.emit("entityCommand",{commandId:request.commandId,entityId:"other",operation:"update"});assert.equal(settled,0);
  instance.emit("entityCommand",{commandId:request.commandId,entityId:"a",operation:"update",expectedRevision:2n});
  assert.equal(settled,1);assert.equal(instance.sent.length,1);assert.equal(commands.uncertain.length,0);
  const denied=commands.request({entityId:"a",operation:"delete",expectedRevision:2n});
  const rejected=assert.rejects(commands.send(denied),CommandRejected);
  instance.emit("error",{requestMessageId:denied.commandId,code:"PRE_COMMIT_DENIED",retryable:false});await rejected;
  assert.equal(commands.uncertain.length,0);assert.equal(settled,1);commands.dispose();
});
it("uncertain entries consume the shared count bound and leave clears all resources",async()=>{
  const instance=new Instance(),commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  for(let i=0;i<32;i++){
    const request=commands.request({entityId:String(i),operation:"delete",expectedRevision:1n});
    const pending=assert.rejects(commands.send(request),CommandUncertain);
    instance.emit("error",{requestMessageId:request.commandId,code:"COMMAND_TIMEOUT",retryable:true});await pending;
  }
  await assert.rejects(commands.send(commands.request({entityId:"33",operation:"delete"})),{code:"RESOURCE_LIMIT"});
  assert.equal(instance.sent.length,32);assert.equal(commands.uncertain.length,32);
  instance.emit("syncStateChanged","closed");assert.equal(commands.uncertain.length,0);
  assert.equal([...instance.handlers.values()].reduce((n,s)=>n+s.size,0),0);
  await assert.rejects(commands.send(commands.request({entityId:"closed",operation:"delete"})),CommandRejected);
});

it("unresolved entity blocks a new command ID instead of submitting a second mutation",async()=>{
  const instance=new Instance(),commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const request=commands.request({entityId:"a",operation:"update",expectedRevision:1n});
  const pending=assert.rejects(commands.send(request),CommandUncertain);
  instance.emit("error",{requestMessageId:request.commandId,code:"PERSISTENCE_UNAVAILABLE",retryable:true});await pending;
  assert.equal(commands.hasPending("a"),true);
  await assert.rejects(commands.send(commands.request({entityId:"a",operation:"update",expectedRevision:2n})),{code:"OPERATION_PENDING"});
  assert.equal(instance.sent.length,1);commands.dispose();
});
it("confirmation timeout retains a typed uncertain request and late receipt releases it",async t=>{
  t.mock.timers.enable({apis:["setTimeout"]});
  const instance=new Instance(),commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance);
  const request=commands.request({entityId:"a",operation:"delete",expectedRevision:1n});
  const pending=assert.rejects(commands.send(request),{code:"CONFIRMATION_TIMEOUT",outcome:"uncertain"});
  t.mock.timers.tick(10001);await pending;assert.equal(commands.uncertain.length,1);
  instance.emit("entityCommand",{commandId:request.commandId,entityId:"a",operation:"delete",expectedRevision:101n});
  assert.equal(commands.uncertain.length,0);commands.dispose();
});

it("actual SDK snapshot advancement preserves exact retry wire and resolves old durable receipt",async()=>{
  const { OrbiSyncInstance, decodeEnvelope, encodeEnvelope } = await import("../../../sdk/typescript/src/client.js");
  const { create } = await import("@bufbuild/protobuf");
  const { EnvelopeSchema } = await import("../../../sdk/typescript/src/generated/orbisync/v1/realtime_pb.js");
  const sent: Uint8Array[]=[];let sequence=0;
  const socket={readyState:1,bufferedAmount:0,send:(bytes:Uint8Array)=>sent.push(bytes)} as unknown as WebSocket;
  const instance=new OrbiSyncInstance(socket,"room",()=>++sequence);
  const snapshot=(revision:number,entityRevision:number)=>create(EnvelopeSchema,{instanceId:"room",payload:{case:"snapshot",value:{snapshotId:String(revision),chunkCount:1,instanceRevision:BigInt(revision),data:new TextEncoder().encode(JSON.stringify({format:"orbisync.snapshot.v1",revision,instance:{instance_id:"room"},entities:[{entity_id:"a",kind:"object",revision:entityRevision,properties:{}}],users:[]}))}}});
  instance._beginSync(1,true);instance._dispatch(snapshot(100,1));await instance.ready();
  const commands=new ConfirmedCommands(instance);
  try{
    const request=commands.request({entityId:"a",operation:"update",args:{component_key:"note.body",text:"same"}});
    const uncertain=assert.rejects(commands.send(request),CommandUncertain);
    instance._dispatch(create(EnvelopeSchema,{instanceId:"room",payload:{case:"error",value:{code:"PERSISTENCE_UNAVAILABLE",requestMessageId:request.commandId,retryable:true}}}));await uncertain;
    instance._beginSync(2,true);instance._dispatch(snapshot(110,2));await instance.ready();
    const retry=commands.send(request);
    const first=decodeEnvelope(sent[0]!).payload,second=decodeEnvelope(sent[1]!).payload;
    assert.equal(first.case,"entityCommand");assert.deepEqual(first,second);
    instance._dispatch(create(EnvelopeSchema,{instanceId:"room",payload:{case:"entityCommand",value:{commandId:request.commandId,entityId:"a",operation:"update",expectedRevision:2n,instanceRevision:101n,arguments:request.args as any}}}));
    await retry;assert.equal(commands.uncertain.length,0);assert.equal(instance.state.revision,110n);assert.equal(instance.state.entities.get("a")?.revision,2n);
    assert.ok(encodeEnvelope(snapshot(110,2)).length>0);
  }finally{commands.dispose();const {SyncError}=await import("../../../sdk/typescript/src/sync_state.js");instance._stopSync(new SyncError("CLOSED"),"closed");}
});

it("explicit leave interrupts tracking and a new instance cannot transmit the old request",async()=>{
  const oldInstance=new Instance(),nextInstance=new Instance();let late=0;
  const oldCommands=new ConfirmedCommands(oldInstance as unknown as OrbiSyncInstance,()=>{},()=>{late++;});
  const request=oldCommands.request({entityId:"same-id",operation:"delete",expectedRevision:1n});
  const closed=assert.rejects(oldCommands.send(request),{code:"CLOSED",outcome:"interrupted"});
  assert.equal(oldCommands.unresolvedCount,1);
  oldCommands.dispose();await closed;assert.equal(oldCommands.unresolvedCount,0);
  const nextCommands=new ConfirmedCommands(nextInstance as unknown as OrbiSyncInstance);
  await assert.rejects(nextCommands.send(request),{code:"REQUEST_SCOPE"});
  oldInstance.emit("entityCommand",{commandId:request.commandId,entityId:"same-id",operation:"delete"});
  assert.equal(late,0);assert.equal(nextInstance.sent.length,0);nextCommands.dispose();
});

it("Dirty retries and rejection after lost dedup never claim an original outcome from a snapshot",async()=>{
  const instance=new Instance();let applied=0;
  const commands=new ConfirmedCommands(instance as unknown as OrbiSyncInstance,()=>{},()=>{applied++;});
  const request=commands.request({entityId:"a",operation:"update",expectedRevision:1n,args:{text:"original"}});
  for(const [code,retryable] of [["PERSISTENCE_UNAVAILABLE",true],["PERSISTENCE_UNAVAILABLE",true],["REVISION_MISMATCH",false]] as const){
    const waiting=assert.rejects(commands.send(request),error=>error instanceof CommandUncertain && error.code===code);
    instance.emit("error",{requestMessageId:request.commandId,code,retryable});await waiting;
    instance.state.entities.set("a",{revision:2n});instance.emit("syncStateChanged","ready");
    assert.equal(commands.uncertain.length,1);assert.equal(applied,0);
  }
  assert.equal(instance.sent.length,3);assert.ok(instance.sent.every(sent=>sent===request));
  assert.equal(request.expectedRevision,1n);commands.dispose();assert.equal(commands.unresolvedCount,0);
});
