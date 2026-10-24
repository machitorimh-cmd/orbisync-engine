import { it } from 'node:test';
import assert from 'node:assert/strict';
import { CursorPages } from './cursor_pages.mjs';
const rows = (start, count) => Array.from({length: count}, (_, n) => ({id: String(start + n)}));
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise, resolve, reject}; };
for (const resource of ['reference World', 'reference Instance', 'whiteboard World', 'whiteboard Instance']) {
  it(`${resource}: reaches item 101 only through an explicit next-page request`, async () => {
    const pages = new CursorPages(); const requests=[];
    const fetch = async cursor => { requests.push(cursor); return cursor ? {items:rows(101,1)} : {items:rows(1,100),next_cursor:'next'}; };
    await pages.load(fetch); assert.equal(pages.items.length,100); assert.deepEqual(requests,[null]);
    await pages.load(fetch); assert.equal(pages.items.at(-1).id,'101'); assert.equal(pages.nextCursor,null);
    await pages.load(fetch); assert.deepEqual(requests,[null,'next']);
  });
}
it('duplicate clicks share one request and failed pages preserve cursor/data for retry', async () => {
  const pages = new CursorPages(), pending=deferred(); let requests=0;
  const fetch = () => {requests++; return pending.promise;};
  const a=pages.load(fetch), b=pages.load(fetch); assert.equal(a,b);
  await Promise.resolve(); assert.equal(requests,1);
  pending.resolve({items:rows(1,100),next_cursor:'next'}); await a;
  await assert.rejects(pages.load(async()=>{throw Error('offline');}));
  assert.equal(pages.items.length,100); assert.equal(pages.nextCursor,'next'); assert.equal(pages.busy,false);
  await pages.load(async cursor=>{assert.equal(cursor,'next');return {items:[{id:'100',updated:true},{id:'101'}]};});
  assert.equal(pages.items.length,101); assert.equal(pages.items.find(item=>item.id==='100').updated,true);
});
for(const failure of [false,true]) it(`server/search reset discards a stale ${failure?'error':'page'} and does not clear a newer busy state`,async()=>{
  const pages=new CursorPages(), old=deferred(), current=deferred();
  const first=pages.load(()=>old.promise); await Promise.resolve(); pages.reset();
  const second=pages.load(()=>current.promise); await Promise.resolve();
  if(failure)old.reject(Error('old server'));else old.resolve({items:rows(1,100),next_cursor:'old-cursor'});
  assert.equal(await first,false); assert.equal(pages.busy,true); assert.equal(pages.items.length,0);
  current.resolve({items:[{id:'new'}]});await second;assert.deepEqual(pages.items,[{id:'new'}]);
});
it('repeated cursors, excessive pages, and retained-item overflow fail atomically',async()=>{
  const pages=new CursorPages(101);await pages.load(async()=>({items:rows(1,100),next_cursor:'a'}));
  await assert.rejects(pages.load(async()=>({items:[{id:'101'}],next_cursor:'a'})),/循環/);
  assert.equal(pages.items.length,100);assert.equal(pages.nextCursor,'a');
  await assert.rejects(pages.load(async()=>({items:rows(101,2)})),/保持上限/);
  await assert.rejects(pages.load(async()=>({items:rows(101,101)})),/応答/);
  assert.equal(pages.items.length,100);
  await pages.load(async()=>({items:[{id:'101'}]}));assert.equal(pages.items.length,101);
});
it('empty pages cannot retain an unlimited cursor history',async()=>{
  const pages=new CursorPages();let requests=0;
  const fetch=async()=>({items:[],next_cursor:String(++requests)});
  for(let n=0;n<1000;n++)await pages.load(fetch);
  await assert.rejects(pages.load(fetch),/ページ数/);
  assert.equal(requests,1000);assert.equal(pages.items.length,0);
  pages.reset();await pages.load(fetch);assert.equal(requests,1001);
});
