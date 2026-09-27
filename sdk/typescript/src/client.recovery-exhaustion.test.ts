import { it } from 'node:test';
import assert from 'node:assert/strict';
import { create, type MessageInitShape } from '@bufbuild/protobuf';
import { SyncError } from './sync_state.js';
import { EnvelopeSchema } from './generated/orbisync/v1/realtime_pb.js';
import { ServerHelloSchema } from './generated/orbisync/v1/realtime_pb.js';
import { OrbiSyncClient, OrbiSyncConnection, decodeEnvelope, encodeEnvelope } from './client.js';

const bytes = new TextEncoder().encode(JSON.stringify({format:'orbisync.snapshot.v1',revision:100,instance:{instance_id:'room'},entities:[],users:[]}));
it('independent04: recovery exhaustion must be terminal and immediately observable',async()=>{
 class Socket extends EventTarget {
   readyState=1;
   private serverSequence=2n;
   send(data:Uint8Array) {
     if(decodeEnvelope(data).payload.case==='joinInstance') {
       const receive=(payload:MessageInitShape<typeof EnvelopeSchema>['payload'])=>this.dispatchEvent(new MessageEvent('message',{data:encodeEnvelope(create(EnvelopeSchema,{instanceId:'room',sequence:this.serverSequence++,payload}))}));
       receive({case:'joinAccepted',value:{instanceRevision:100n}});
       receive({case:'snapshot',value:{snapshotId:'s',chunkCount:1,instanceRevision:100n,data:bytes}});
     }
   }
   close(){this.readyState=3;}
 }
 const conn=new OrbiSyncConnection(new Socket() as unknown as WebSocket,create(ServerHelloSchema,{enabledFeatures:['orbisync.state-sync.v1'],heartbeatIntervalMs:20000n}),new OrbiSyncClient({baseUrl:'http://localhost'}));
 try {
   const instance=await conn.join('room');await instance.ready();
   instance._stopSync(new SyncError('DISCONNECTED'));
   (conn as any).reconnectAttempt=5;
   (conn as any).scheduleReconnect();
   assert.equal(instance.syncStatus,'failed','exhausted retries must not leave a permanently reconnecting instance');
   await assert.rejects(instance.ready(),{code:'RECOVERY_EXHAUSTED'});
 } finally {await conn.disconnect();}
});
