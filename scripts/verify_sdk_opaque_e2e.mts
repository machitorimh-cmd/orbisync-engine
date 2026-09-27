import assert from 'node:assert/strict';
import { OrbiSyncClient, uuidv7 } from '../sdk/typescript/src/client.js';
import { execFileSync } from 'node:child_process';
const [base,instanceId,entityId,container,db,worldId]=process.argv.slice(2);
if(!base||!container||!db||![instanceId,entityId,worldId].every(id=>/^[-a-f0-9]{36}$/.test(id??'')))throw Error('usage: verify_sdk_opaque_e2e.mts <isolated Core> <new instance> <new entity> <isolated container> <DB> <existing world>');
const client=new OrbiSyncClient({baseUrl:base});await client.auth.guest();
const subject=JSON.parse(Buffer.from(client._getAccessToken()!.split('.')[1]!,'base64url').toString()).sub;
assert.match(subject,/^[-a-f0-9]{36}$/);
// Test-only fixture ingress for bytes and revisions that Struct cannot express.
// Insert before instance load, never into an active runtime's backing rows.
const sql=`BEGIN;
INSERT INTO world_instances(id,world_id,lifecycle,capacity,created_at,started_at,revision) VALUES ('${instanceId}','${worldId}','running',100,now(),now(),9007199254740993);
INSERT INTO persistent_entities(id,instance_id,kind,owner_id,visibility,revision,created_at,updated_at) VALUES ('${entityId}','${instanceId}','object','${subject}','{"type":"global"}',9007199254740993,now(),now());
INSERT INTO persistent_entity_components(entity_id,component_key,payload) VALUES ('${entityId}','test.opaque',decode('00ff1080','hex'));
INSERT INTO persistent_entity_components(entity_id,component_key,payload) VALUES ('${entityId}','test.unsafe',convert_to('{"integer":9007199254740993,"maximum":18446744073709551615}','UTF8'));
COMMIT;`;
execFileSync('docker',['exec','-i',container,'psql','-U',db,'-d',db,'-v','ON_ERROR_STOP=1'],{input:sql,stdio:['pipe','pipe','pipe']});
const connection=await client.connect();
try {
  const instance=await connection.join(instanceId);await instance.ready();
  const entity=instance.state.entities.get(entityId);assert.ok(entity);
  assert.equal(entity.revision,9007199254740993n);
  assert.deepEqual(entity.properties['test.opaque'],{encoding:'base64',value:'AP8QgA=='});
  assert.deepEqual(entity.properties['test.unsafe'],{encoding:'base64',value:Buffer.from('{"integer":9007199254740993,"maximum":18446744073709551615}').toString('base64')});
  assert.ok(instance.state.revision>9007199254740993n);
  const commandId=uuidv7();
  await new Promise<void>((resolve,reject)=>{
    const timer=setTimeout(()=>finish(new Error('confirmation timeout')),10000);
    const confirmed=(raw:unknown)=>{if((raw as {commandId:string}).commandId===commandId)finish();};
    const error=(raw:unknown)=>{const value=raw as {requestMessageId:string;code:string};if(value.requestMessageId===commandId)finish(new Error(value.code));};
    const finish=(failure?:Error)=>{clearTimeout(timer);instance.off('entityCommand',confirmed);instance.off('error',error);if(failure)reject(failure);else resolve();};
    instance.on('entityCommand',confirmed);instance.on('error',error);
    try{instance.sendEntityCommand({commandId,entityId,operation:'update',expectedRevision:entity.revision,args:{component_key:'test.json',value:'bigint request'}});}catch(error){finish(error as Error);}
  });
  assert.equal(instance.state.entities.get(entityId)?.revision,9007199254740994n);
  assert.deepEqual(instance.state.entities.get(entityId)?.properties['test.opaque'],entity.properties['test.opaque']);
  assert.deepEqual(instance.state.entities.get(entityId)?.properties['test.unsafe'],entity.properties['test.unsafe']);
  console.log(JSON.stringify({pass:true,entityRevision:entity.revision.toString(),confirmedRevision:'9007199254740994',instanceRevision:instance.state.revision.toString(),opaque:'AP8QgA==',ingress:'isolated PG fixture -> production persistent store -> snapshot -> ticket/WS -> SDK -> bigint request -> confirmation'}));
} finally { await connection.disconnect(); }
