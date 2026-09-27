import { chromium, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
const [ui,core,instanceId,container,db]=process.argv.slice(2);
if(!ui||!core||!container||!db||!/^[-a-f0-9]{36}$/.test(instanceId??''))throw Error('usage: browser-sync.mjs <UI> <Core> <instance UUID> <isolated container> <DB>');
const browser=await chromium.launch({headless:true,executablePath:process.env.SDK04_CHROME??'C:/Program Files/Google/Chrome/Application/chrome.exe'});
const contexts=[];
async function join(method,name){
 const context=await browser.newContext();contexts.push(context);
 await context.addInitScript(()=>{window.sdk04Sockets=[];const Original=window.WebSocket;window.WebSocket=class extends Original{constructor(...args){super(...args);window.sdk04Sockets.push(this);}};});
 const page=await context.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));
 await page.goto(ui);await page.locator('#server').fill(core);await page.locator('#authMethod').selectOption(method);
 if(name)await page.locator('#displayName').fill(name);
 await page.locator('#login').click();await expect(page.locator('#connection')).toHaveText('認証済み');
 await page.locator('#load').click();await expect(page.locator(`#instances option[value="${instanceId}"]`)).toHaveCount(1);
 await page.locator('#instances').selectOption(instanceId);await page.locator('#join').click();await expect(page.locator('#syncState')).toHaveText('同期完了');
 return {page,errors};
}
function rows(){return JSON.parse(execFileSync('docker',['exec',container,'psql','-U',db,'-d',db,'-tAc',`SELECT coalesce(json_agg(json_build_object('id',e.id,'revision',e.revision,'note',convert_from(c.payload,'UTF8')::json)), '[]') FROM persistent_entities e JOIN persistent_entity_components c ON c.entity_id=e.id WHERE e.instance_id='${instanceId}' AND c.component_key='com.orbisync.whiteboard.note'`],{encoding:'utf8'}).trim());}
try{
 const a=await join('guest');await expect(a.page.locator('.note')).toHaveCount(0);
 await a.page.locator('#add').click();await expect(a.page.locator('.note')).toHaveCount(1);await expect(a.page.locator('#pendingNotes')).toHaveText('');
 const persisted=rows();if(persisted.length!==1||persisted[0].revision!==2||persisted[0].note.text!=='ここにアイデアを書く'||persisted[0].note.color!=='yellow')throw Error('two-step persisted state differs');
 const b=await join('name_only','SDK04 peer');await expect(b.page.locator('.note textarea')).toHaveValue(persisted[0].note.text);await expect(b.page.locator('.note')).toHaveClass(/yellow/);
 await b.page.locator('.note textarea').fill('別contextの本文');await b.page.locator('#boardInfo').click();await expect(a.page.locator('.note textarea')).toHaveValue('別contextの本文');
 if(rows()[0].note.text!=='別contextの本文')throw Error('peer edit not persisted');
 await a.page.evaluate(()=>window.sdk04Sockets.at(-1).close(4001,'SDK04 reconnect test'));
 await expect.poll(()=>a.page.evaluate(()=>window.sdk04Sockets.length),{timeout:15000}).toBeGreaterThan(1);
 await expect(a.page.locator('#syncState')).toHaveText('同期完了',{timeout:15000});await expect(a.page.locator('.note textarea')).toHaveValue('別contextの本文');
 await b.page.locator('[aria-label="削除"]').click();await expect(a.page.locator('.note')).toHaveCount(0);await expect(b.page.locator('.note')).toHaveCount(0);
 const c=await join('guest');await expect(c.page.locator('.note')).toHaveCount(0);if(rows().length!==0)throw Error('deleted entity persisted');
 for(const participant of [a,b,c])expect(participant.errors).toEqual([]);
 console.log(JSON.stringify({pass:true,browserContexts:contexts.length,initialTwoStepPersistence:true,lateJoinBodyColor:true,peerEdit:true,resumeReconnect:true,deleteFreshJoin:true,signedHook:'pending'}));
}finally{await Promise.all(contexts.map(context=>context.close()));await browser.close();}
