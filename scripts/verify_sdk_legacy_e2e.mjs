/** Uses SDK source and generated schema from ef3ed18, prepared in ignored cache. */
import assert from 'node:assert/strict';
const legacy=await import('../sdk/typescript/node_modules/.cache/sdk04-old/client.ts');
const [base,instanceId]=process.argv.slice(2);if(!base||!instanceId)throw Error('explicit isolated Core and instance required');
const client=new legacy.OrbiSyncClient({baseUrl:base});await client.auth.guest();const connection=await client.connect();
let snapshots=0;const confirmations=[];const deltas=[];
connection._getWs().addEventListener('message',event=>{const envelope=legacy.decodeEnvelope(new Uint8Array(event.data));if(envelope.payload.case==='snapshot')snapshots++;if(envelope.payload.case==='entityCommand')confirmations.push(envelope.payload.value);if(envelope.payload.case==='stateDelta')deltas.push(envelope.payload.value);});
const waitFor=async predicate=>{const until=Date.now()+10000;while(!predicate()){if(Date.now()>until)throw Error('legacy confirmation timeout');await new Promise(resolve=>setTimeout(resolve,10));}};
try{
 const instance=await connection.join(instanceId);await waitFor(()=>snapshots>0);
 const entityId=legacy.uuidv7();
 instance.sendEntityCommand({entityId,operation:'spawn',expectedRevision:0,args:{kind:'object',visibility:'global',position_x:0,position_y:0,position_z:0}});
 await waitFor(()=>confirmations.length===1);assert.equal(confirmations[0].expectedRevision,1n);assert.equal(Object.hasOwn(confirmations[0],'instanceRevision'),false);
 instance.sendEntityCommand({entityId,operation:'update',expectedRevision:1,args:{component_key:'test.compat',text:'legacy request'}});
 await waitFor(()=>confirmations.length===2);assert.equal(confirmations[1].expectedRevision,2n);
 instance.sendTransform({entityId,position:{x:0.01,y:0,z:0},expectedRevision:2});await waitFor(()=>deltas.length>0);
 assert.equal(deltas.at(-1).entities.find(entity=>entity.entityId===entityId)?.revision,3n);
 instance.sendEntityCommand({entityId,operation:'delete',expectedRevision:3,args:{}});await waitFor(()=>confirmations.length===3);
 assert.ok(confirmations[2].expectedRevision>3n);
 console.log(JSON.stringify({pass:true,legacyBase:'ef3ed18',snapshotDecoded:true,additiveFieldIgnored:true,spawnEntityRevision:'1',updateEntityRevision:'2',transformEntityRevision:'3',deleteInstanceRevision:confirmations[2].expectedRevision.toString()}));
}finally{await connection.disconnect();}
