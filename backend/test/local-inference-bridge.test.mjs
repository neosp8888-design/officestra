import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { LocalInferenceBridge, normalizeLocalMessageRequest } from '../src/local-inference-bridge.mjs';
import { INCLUSIVE_INPUT_PROFILE } from '../src/local-usage-normalizer.mjs';
const token='isolated-bridge-test-token-32bytes';
test('ToolSearch reference survives resume as compatible text without changing the session or tools',()=>{
  const value={model:'test-model',tools:[{name:'SendMessage',defer_loading:true,input_schema:{type:'object'}}],messages:[{role:'user',content:[{type:'tool_result',tool_use_id:'search-result',content:[{type:'tool_reference',tool_name:'SendMessage'},{type:'text',text:'Keep this text'},{type:'image',source:{type:'base64',data:'image-data'}}]},{type:'text',text:'Tool loaded.'}]}]};
  const before=JSON.stringify(value),result=normalizeLocalMessageRequest(value);
  assert.equal(JSON.stringify(value),before);assert.equal(result.tools,value.tools);
  assert.equal(result.messages[0].content[0].tool_use_id,'search-result');
  assert.equal(result.messages[0].content[0].content[0].type,'text');
  assert.match(result.messages[0].content[0].content[0].text,/SendMessage/);
  assert.equal(result.messages[0].content[0].content[1],value.messages[0].content[0].content[1]);
  assert.equal(result.messages[0].content[0].content[2],value.messages[0].content[0].content[2]);
  assert.equal(result.messages[0].content[1],value.messages[0].content[1]);
  const textOnly={messages:[{role:'user',content:'hi'}]};assert.equal(normalizeLocalMessageRequest(textOnly),textOnly);
  assert.throws(()=>normalizeLocalMessageRequest({...value,tools:[]}),/no matching/);
});
test('message and count_tokens HTTP paths both adapt persisted tool references',async t=>{
  const s=await setup(t,(req,res)=>{
    assert.equal(Number(req.headers['content-length']),Buffer.byteLength(s.seen.at(-1).body));
    const value=JSON.parse(s.seen.at(-1).body);
    assert.equal(value.messages[0].content[0].content[0].type,'text');
    assert.equal(value.tools[0].name,'SendMessage');jsonReply(req,res);
  });
  for(const path of ['/v1/messages?beta=true','/v1/messages/count_tokens']){
    const res=await s.post({tools:[{name:'SendMessage',input_schema:{type:'object'}}],messages:[{role:'user',content:[{type:'tool_result',tool_use_id:'real-shape',content:[{type:'tool_reference',tool_name:'SendMessage'}]}]}]}, {},path);
    assert.equal(res.status,200);await res.text();
  }
  assert.equal(s.seen.length,2);
});
test('cleanup timeout is not proof that remote resources have been released',async()=>{
  let finish;
  const bridge=new LocalInferenceBridge({upstream:'http://127.0.0.1:1234',token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,
    readResources:()=>({vramPct:1,ramPct:1,busy:false,sampledAt:Date.now()}),audit:()=>{},release:()=>new Promise(r=>{finish=r;})});
  await bridge.stop();assert.equal(bridge.status.cleanupComplete,false);assert.equal(bridge.status.state,'fault');
  finish();await new Promise(r=>setImmediate(r));assert.equal(bridge.status.cleanupComplete,true);
});
const sample=()=>({vramPct:70,ramPct:55,sampledAt:Date.now(),busy:false});
const delay=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate){for(let i=0;i<100;i++){if(predicate())return;await delay(10);}assert.fail('condition timed out');}
async function setup(t,handler,options={}){
  const seen=[];const audit=[];let released=0;
  const server=createServer(async(req,res)=>{let body='';for await(const c of req)body+=c;seen.push({body,headers:req.headers,url:req.url});handler(req,res);});
  await new Promise(r=>server.listen(0,'127.0.0.1',r));
  const bridge=new LocalInferenceBridge({upstream:`http://127.0.0.1:${server.address().port}`,token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,
    readResources:sample,audit:r=>audit.push(r),release:async()=>{released++;},pollMs:20,idleMs:10000,...options});
  t.after(async()=>{await bridge.stop();server.closeAllConnections();await new Promise(r=>server.close(r));});
  const url=await bridge.start();
  const post=(body={},headers={},path='/v1/messages')=>fetch(url+path,{method:'POST',headers:{'x-api-key':token,...headers},body:typeof body==='string'?body:JSON.stringify({model:'test-model',...body})});
  return {bridge,url,post,seen,audit,released:()=>released};
}
const jsonReply=(req,res)=>{res.setHeader('content-type','application/json');res.end(JSON.stringify({type:'message',id:'m1',content:[{type:'text',text:'hello'}],usage:{input_tokens:100,cache_read_input_tokens:80,output_tokens:2}}));};

test('auth, browser origins and routes are blocked before upstream',async t=>{
  const s=await setup(t,jsonReply);
  assert.equal((await fetch(s.url+'/health')).status,401);
  assert.equal((await s.post({}, {'x-api-key':'wrong'})).status,401);
  assert.equal((await s.post({}, {origin:'https://example.com'})).status,401);
  assert.equal((await s.post({}, {}, '/v1/models')).status,404);
  assert.equal(s.seen.length,0);
  const health=await(await fetch(s.url+'/health',{headers:{authorization:`Bearer ${token}`}})).json();
  assert.equal(health.state,'ready');assert.equal(health.usageProtocol,'anthropic-normalized-v1');
});
test('JSON usage normalized, raw audited, request exact and credentials never forwarded',async t=>{
  const s=await setup(t,jsonReply);const body=' {"model":"test-model","messages":[{"role":"user","content":"한글"}]} ';
  const res=await s.post(body,{authorization:`Bearer ${token}`,cookie:'private=1','x-secret':'hidden'});
  const result=await res.json();assert.equal(result.usage.input_tokens,20);assert.equal(result.usage.cache_read_input_tokens,80);
  assert.equal(result.content[0].text,'hello');assert.equal(s.audit[0].raw.input_tokens,100);
  assert.equal(s.seen[0].body,body);
  for(const key of ['authorization','x-api-key','cookie','x-secret'])assert.equal(s.seen[0].headers[key],undefined);
});
test('SSE normalized across chunks without changing response text',async t=>{
  const s=await setup(t,(req,res)=>{res.setHeader('content-type','text/event-stream');const event=JSON.stringify({type:'message_start',message:{id:'m',usage:{input_tokens:100,cache_read_input_tokens:80}}});res.write('data: '+event.slice(0,20));res.write(event.slice(20)+'\n\n');res.end('data: {"type":"content_block_delta","delta":{"text":"끝"}}\n\n');});
  const text=await(await s.post({stream:true})).text();assert.match(text,/"input_tokens":20/);assert.match(text,/끝/);assert.equal(s.audit.length,1);
});
test('count_tokens and upstream errors are not usage-normalized',async t=>{
  const s=await setup(t,(req,res)=>{res.setHeader('content-type','application/json');if(req.url.endsWith('count_tokens'))res.end('{"input_tokens":100}');else{res.statusCode=429;res.end('{"error":"busy"}');}});
  assert.deepEqual(await(await s.post({}, {}, '/v1/messages/count_tokens')).json(),{input_tokens:100});
  assert.equal((await s.post()).status,429);assert.equal(s.audit.length,0);
});
test('concurrency one; repeated start shares one listener; stop/release only once',async t=>{
  let finish;const s=await setup(t,(req,res)=>{finish=()=>jsonReply(req,res);});
  assert.equal(await s.bridge.start(),s.url);
  const pending=s.post();await until(()=>s.seen.length===1);
  assert.equal((await s.post()).status,409);finish();await(await pending).text();
  await Promise.all([s.bridge.stop(),s.bridge.stop()]);assert.equal(s.released(),1);
  await assert.rejects(s.bridge.start());
});
test('busy GPU blocks new requests but does not stop someone else workload',async t=>{
  let busy=false;const s=await setup(t,jsonReply,{readResources:()=>({...sample(),busy})});busy=true;
  assert.equal((await s.post()).status,503);assert.equal(s.seen.length,0);assert.equal(s.bridge.status.state,'ready');
});
test('memory guard and stale samples close listener and release owned resources',async t=>{
  let current=sample();const s=await setup(t,jsonReply,{readResources:()=>current});
  current={...sample(),vramPct:77};await until(()=>s.bridge.status.state==='fault');assert.equal(s.released(),1);
});
test('explicit GPU 95 percent budget accepts above 77 then aborts at 95',async t=>{
  let current={...sample(),vramPct:94.9};
  const s=await setup(t,jsonReply,{vramGuardPercent:95,ramGuardPercent:77,readResources:()=>({...current,sampledAt:Date.now()})});
  assert.equal((await s.post()).status,200);
  const health=await(await fetch(s.url+'/health',{headers:{'x-api-key':token}})).json();
  assert.equal(health.vramGuardPercent,95);assert.equal(health.ramGuardPercent,77);
  current={...current,vramPct:95};await until(()=>s.bridge.status.state==='fault');assert.equal(s.released(),1);
});
test('raising VRAM budget does not raise system RAM budget',async t=>{
  let current={...sample(),vramPct:90};
  const s=await setup(t,jsonReply,{vramGuardPercent:95,ramGuardPercent:77,readResources:()=>({...current,sampledAt:Date.now()})});
  current={...current,ramPct:77};await until(()=>s.bridge.status.state==='fault');assert.equal(s.released(),1);
});
test('memory budgets above 95 or non-numeric values are rejected',()=>{
  const options={upstream:'http://127.0.0.1:1234',token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:sample,audit:()=>{}};
  for(const vramGuardPercent of [96,100,NaN,'95'])assert.throws(()=>new LocalInferenceBridge({...options,vramGuardPercent}));
});
test('stale or missing startup sample fails closed',async()=>{
  for(const value of [null,{...sample(),sampledAt:Date.now()-10000},{...sample(),ramPct:80}]){
    let releases=0;const b=new LocalInferenceBridge({upstream:'http://127.0.0.1:1234',token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:()=>value,audit:()=>{},release:async()=>releases++});
    await assert.rejects(b.start());assert.equal(b.status.state,'fault');assert.equal(releases,1);
  }
});
test('startup cancellation never leaves a late listener',async()=>{
  let done,releases=0;const b=new LocalInferenceBridge({upstream:'http://127.0.0.1:1234',token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:()=>new Promise(r=>done=r),audit:()=>{},release:async()=>releases++});
  const started=b.start();await until(()=>done);await b.stop();done(sample());await assert.rejects(started);assert.equal(b.status.address,null);assert.equal(releases,1);
});
test('idle shutdown releases model without requiring another request',async t=>{
  const s=await setup(t,jsonReply,{idleMs:40});await until(()=>s.bridge.status.state==='stopped');assert.equal(s.released(),1);
});
test('timeout cancels upstream and clears active request',async t=>{
  const s=await setup(t,()=>{}, {requestTimeoutMs:50});const r=await s.post();assert.equal(r.status,504);await r.text();await until(()=>!s.bridge.status.active);
});
test('invalid and oversized body never reaches model',async t=>{
  const s=await setup(t,jsonReply,{maxBodyBytes:32});assert.equal((await s.post('not json')).status,400);assert.equal((await s.post('x'.repeat(100))).status,413);assert.equal(s.seen.length,0);
});
test('unknown dialect, weak token and non-loopback destination rejected',()=>{
  const options={upstream:'http://127.0.0.1:1234',token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:sample,audit:()=>{}};
  assert.throws(()=>new LocalInferenceBridge({...options,upstream:'https://example.com'}));
  assert.throws(()=>new LocalInferenceBridge({...options,token:'weak'}));
  assert.throws(()=>new LocalInferenceBridge({...options,usageProfile:'unknown'}));
});

test('actual Claude SDK beta query allowed but unknown queries and model changes rejected',async t=>{
  const s=await setup(t,jsonReply);
  assert.equal((await s.post({}, {}, '/v1/messages?beta=true')).status,200);
  assert.equal((await s.post({}, {}, '/v1/messages?beta=false')).status,404);
  assert.equal((await s.post({model:'another-model'})).status,400);
  assert.equal((await s.post({max_tokens:5000})).status,400);
  assert.equal(s.seen.length,1);
});

test('bad usage semantics fail closed and release owned resources',async t=>{
  const s=await setup(t,(req,res)=>{res.setHeader('content-type','application/json');res.end('{"type":"message","usage":{"input_tokens":1,"cache_read_input_tokens":10}}');});
  assert.equal((await s.post()).status,502);await until(()=>s.bridge.status.state==='fault');assert.equal(s.released(),1);
});

test('memory guard aborts a running request, not just future admissions',async t=>{
  let current=sample();const s=await setup(t,()=>{}, {readResources:()=>current});
  const pending=s.post().catch(()=>null);await until(()=>s.seen.length===1);
  current={...sample(),ramPct:78};await until(()=>s.bridge.status.state==='fault');
  await pending;await until(()=>!s.bridge.status.active);assert.equal(s.released(),1);
});

test('loss of fresh telemetry while ready shuts down',async t=>{
  let current=sample();const s=await setup(t,jsonReply,{readResources:()=>current});
  current={...sample(),sampledAt:Date.now()-20000};await until(()=>s.bridge.status.state==='fault');assert.equal(s.released(),1);
});

test('client cancellation frees the single request slot',async t=>{
  const s=await setup(t,()=>{});const controller=new AbortController();
  const pending=fetch(s.url+'/v1/messages',{method:'POST',headers:{'x-api-key':token},body:'{"model":"test-model"}',signal:controller.signal}).catch(()=>null);
  await until(()=>s.seen.length===1);controller.abort();await pending;await until(()=>!s.bridge.status.active);
  assert.equal(s.bridge.status.state,'ready');
});

test('port conflict cannot close the existing bridge',async t=>{
  const s=await setup(t,jsonReply);let release=0;
  const second=new LocalInferenceBridge({upstream:'http://127.0.0.1:1234',port:Number(new URL(s.url).port),token,model:'test-model',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:sample,audit:()=>{},release:async()=>release++});
  await assert.rejects(second.start(),e=>e.code==='EADDRINUSE');assert.equal(release,1);
  assert.equal((await s.post()).status,200);assert.equal(s.released(),0);
});
