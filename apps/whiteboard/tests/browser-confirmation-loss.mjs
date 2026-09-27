import { chromium, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { decodeEnvelope } from '../../../sdk/typescript/src/client.ts';
const [ui,core,instanceId,container,db]=process.argv.slice(2);
if(!ui||!core||!container||!db||!/^[-a-f0-9]{36}$/.test(instanceId??''))throw Error('explicit isolated fixture required');
const browser=await chromium.launch({headless:true,executablePath:process.env.SDK04_CHROME??'C:/Program Files/Google/Chrome/Application/chrome.exe'});
const context=await browser.newContext();
let dropOperation='',dropped=false;
const sent=[];
try {
  const page=await context.newPage();
  await page.routeWebSocket(`${core.replace(/^http/,'ws')}/**`,route=>{
    const server=route.connectToServer();
    route.onMessage(message=>{
      const envelope=decodeEnvelope(new Uint8Array(message));
      if(envelope.payload.case==='entityCommand')sent.push(envelope.payload.value);
      server.send(message);
    });
    server.onMessage(message=>{
      const envelope=decodeEnvelope(new Uint8Array(message));
      if(!dropped&&envelope.payload.case==='entityCommand'&&envelope.payload.value.operation===dropOperation){dropped=true;return;}
      route.send(message);
    });
  });
  await page.goto(ui);await page.locator('#server').fill(core);await page.locator('#authMethod').selectOption('guest');
  await page.locator('#login').click();await expect(page.locator('#connection')).toHaveText('認証済み');
  await page.locator('#load').click();await expect(page.locator(`#instances option[value="${instanceId}"]`)).toHaveCount(1);
  await page.locator('#instances').selectOption(instanceId);await page.locator('#join').click();await expect(page.locator('#syncState')).toHaveText('同期完了');
  const rows=()=>JSON.parse(execFileSync('docker',['exec',container,'psql','-U',db,'-d',db,'-tAc',`SELECT coalesce(json_agg(json_build_object('id',e.id,'revision',e.revision,'note',convert_from(c.payload,'UTF8')::json)), '[]') FROM persistent_entities e LEFT JOIN persistent_entity_components c ON c.entity_id=e.id AND c.component_key='com.orbisync.whiteboard.note' WHERE e.instance_id='${instanceId}'`],{encoding:'utf8'}).trim());
  await expect(page.locator('.note')).toHaveCount(0);
  for(const [index,operation] of ['spawn','update'].entries()){
    dropOperation=operation;dropped=false;const before=sent.length;
    await page.locator('#add').click();
    await expect(page.locator('#pendingNotes button')).toHaveText('再送',{timeout:15000});
    if(!dropped)throw Error('confirmation was not intercepted');
    // The sender must never claim success from its echoed spawn or unconfirmed update.
    await expect(page.locator('.note')).toHaveCount(index);
    const first=sent.slice(before).find(command=>command.operation===operation);
    const persisted=rows().find(row=>row.id===first.entityId);
    if(operation==='spawn'&&(persisted.revision!==1||persisted.note!==null))throw Error('spawn fabricated custom persistence');
    if(operation==='update'&&(persisted.revision!==2||persisted.note.text!=='ここにアイデアを書く'))throw Error('lost receipt should retain actual persisted body');
    dropOperation='';await page.locator('#pendingNotes button').click();
    await expect(page.locator('#pendingNotes')).toHaveText('');await expect(page.locator('.note')).toHaveCount(index+1);
    const requests=sent.slice(before).filter(command=>command.operation===operation);
    if(requests.length!==2||requests[0].commandId!==requests[1].commandId)throw Error('retry changed command identity');
    const confirmed=rows().find(row=>row.id===first.entityId);
    if(confirmed.revision!==2||confirmed.note.locked!==false||!confirmed.note.color)throw Error('retry reapplied mutation or lost full custom payload');
  }
  console.log(JSON.stringify({pass:true,realCore:true,droppedReceipts:['spawn','update'],noFalseSuccess:true,sameIdRetry:true,fullCustomPersisted:true}));
} finally {await context.close();await browser.close();}
