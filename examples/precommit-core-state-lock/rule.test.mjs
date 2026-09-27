import test from 'node:test';
import assert from 'node:assert/strict';
import { decide, COMPONENT } from './rule.mjs';
const request = (locked = true) => ({ operation:'precommit.entity.update', requester:'owner', client_expected_revision:7,
 current_entity:{revision:7,owner_id:'owner',components:{[COMPONENT]:{encoding:'json',value:{locked}}}},
 component_key:COMPONENT,payload:{locked:false} });
test('owner unlock uses Core state without an external reservation',()=>assert.equal(decide(request()).decision,'allow'));
test('another user cannot unlock using forged payload state',()=>{
 const r=request();r.requester='other';r.payload.current_entity={owner_id:'other',locked:false};
 assert.equal(decide(r).decision,'deny');
});
test('locked document blocks delete and unrelated component changes',()=>{
 const r=request();r.operation='precommit.entity.delete';assert.equal(decide(r).decision,'deny');
 r.operation='precommit.entity.update';r.component_key='org.other.data';assert.equal(decide(r).decision,'deny');
});
test('missing state and revision mismatch fail closed',()=>{
 const r=request();delete r.current_entity;assert.equal(decide(r).decision,'deny');
 const stale=request();stale.client_expected_revision=8;assert.equal(decide(stale).decision,'deny');
});
test('unlocked document accepts edits; only owner may lock',()=>{
 const r=request(false);r.requester='other';assert.equal(decide(r).decision,'allow');
 r.payload.locked=true;assert.equal(decide(r).decision,'deny');r.requester='owner';assert.equal(decide(r).decision,'allow');
});
