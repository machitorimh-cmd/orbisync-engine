import https from 'node:https';
import fs from 'node:fs';
import crypto from 'node:crypto';
import { decide } from './rule.mjs';
const secret=process.env.ORBI_EXTENSION_SECRET_CORE_STATE_LOCK;
if(!secret)throw Error('ORBI_EXTENSION_SECRET_CORE_STATE_LOCK is required');
const reply=(res,status,value)=>{res.writeHead(status,{'content-type':'application/json'});res.end(JSON.stringify(value));};
https.createServer({cert:fs.readFileSync(process.env.HOOK_CERT),key:fs.readFileSync(process.env.HOOK_KEY)},(req,res)=>{
 if(req.method!=='POST'||req.url!=='/precommit')return reply(res,404,{decision:'deny'});
 const chunks=[];let size=0;
 req.on('data',chunk=>{size+=chunk.length;if(size>256*1024){req.destroy();return;}chunks.push(chunk);});
 req.on('error',()=>{});
 req.on('end',()=>{
  const raw=Buffer.concat(chunks);
  const timestamp=req.headers['x-orbisync-timestamp'];
  const event=req.headers['x-orbisync-event-id'];
  const sig=req.headers['x-orbisync-signature'];
  if(typeof timestamp!=='string'||typeof event!=='string'||typeof sig!=='string'
    ||!/^\d+$/.test(timestamp)||Math.abs(Date.now()/1000-Number(timestamp))>300)return reply(res,401,{decision:'deny'});
  const expected=Buffer.from('sha256='+crypto.createHmac('sha256',secret).update(`${timestamp}.${event}.`).update(raw).digest('hex'));
  const actual=Buffer.from(sig);
  if(expected.length!==actual.length||!crypto.timingSafeEqual(expected,actual))return reply(res,401,{decision:'deny'});
  try{return reply(res,200,decide(JSON.parse(raw.toString('utf8')).payload));}
  catch{return reply(res,400,{decision:'deny'});}
 });
}).listen(Number(process.env.HOOK_PORT||8844),'127.0.0.1',()=>console.log('Core-state rule ready'));
