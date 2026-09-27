/** Real production HTTP ticket + WS verification. Requires an isolated fixture. */
import assert from "node:assert/strict";
import { OrbiSyncClient, uuidv7, decodeEnvelope, type OrbiSyncInstance } from "../sdk/typescript/src/client.js";
const base=process.argv[2],instanceId=process.argv[3];
if(!base||!instanceId)throw new Error("usage: verify_sdk_sync_e2e.mts <isolated base URL> <instance UUID>");
const clients: Awaited<ReturnType<OrbiSyncClient["connect"]>>[]=[];
async function join(name?:string){
  const client=new OrbiSyncClient({baseUrl:base});
  if(name)await client.auth.nameOnly({displayName:name});else await client.auth.guest();
  const connection=await client.connect();clients.push(connection);
  let chunks=0;
  connection._getWs().addEventListener("message",event=>{if(decodeEnvelope(new Uint8Array(event.data as ArrayBuffer)).payload.case==="snapshot")chunks++;});
  const instance=await connection.join(instanceId);await instance.ready();
  return {instance,connection,chunks:()=>chunks};
}
function send(instance:OrbiSyncInstance,options:Parameters<OrbiSyncInstance["sendEntityCommand"]>[0]):Promise<void>{
  const commandId=uuidv7();
  return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>finish(new Error("confirmation timeout")),10000);
    const confirmed=(raw:unknown)=>{if((raw as {commandId:string}).commandId===commandId)finish();};
    const error=(raw:unknown)=>{const e=raw as {requestMessageId?:string;code?:string};if(e.requestMessageId===commandId)finish(new Error(e.code));};
    const finish=(error?:Error)=>{clearTimeout(timer);instance.off("entityCommand",confirmed);instance.off("error",errorHandler);if(error)reject(error);else resolve();};
    const errorHandler=error;
    instance.on("entityCommand",confirmed);instance.on("error",errorHandler);
    try{instance.sendEntityCommand({...options,commandId});}catch(e){finish(e as Error);}
  });
}
async function waitFor(predicate:()=>boolean){const end=Date.now()+10000;while(!predicate()){if(Date.now()>end)throw Error("state convergence timeout");await new Promise(resolve=>setTimeout(resolve,10));}}
try{
  const a=await join();
  const ids:string[]=[];
  for(let i=0;i<7;i++){
    const entityId=uuidv7();ids.push(entityId);
    await send(a.instance,{entityId,operation:"spawn",expectedRevision:0n,args:{kind:"object",visibility:"global",position_x:i,position_y:1,locked:false}});
    assert.equal(Object.keys(a.instance.state.entities.get(entityId)!.properties).length,0);
    await send(a.instance,{entityId,operation:"update",args:{component_key:"test.sdk04",text:`note-${i}-`+"あ".repeat(680),color:"mint",locked:false}});
  }
  const b=await join("SDK04 late join");
  assert.ok(b.chunks()>1,"must receive actual multiple Core snapshot frames");
  for(const id of ids)assert.deepEqual(b.instance.state.entities.get(id)?.properties,a.instance.state.entities.get(id)?.properties);
  const id=ids[0]!;
  await send(a.instance,{entityId:id,operation:"delete"});
  await waitFor(()=>!b.instance.state.entities.has(id));
  await send(a.instance,{entityId:id,operation:"spawn",expectedRevision:0n,args:{kind:"object",visibility:"global",locked:false}});
  await waitFor(()=>b.instance.state.entities.get(id)?.revision===1n);
  assert.equal(a.instance.state.entities.get(id)?.revision,1n);
  await send(a.instance,{entityId:id,operation:"update",args:{component_key:"test.sdk04",text:"recreated",locked:false}});
  await waitFor(()=>b.instance.state.entities.get(id)?.revision===2n);
  const c=await join();
  assert.deepEqual(c.instance.state.entities.get(id)?.properties,a.instance.state.entities.get(id)?.properties);
  console.log(JSON.stringify({pass:true,seeded:ids.length,actualSnapshotChunks:b.chunks(),deleteRespawn:true,lateJoinCustom:true}));
}finally{await Promise.all(clients.map(connection=>connection.disconnect()));}
