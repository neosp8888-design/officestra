import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {setTimeout as delay} from 'node:timers/promises';
import {LocalProviderService} from '../src/local-provider-service.mjs';
import {LocalModelPool,localModelPoolKey} from '../src/local-model-pool.mjs';
import {MEROMERO_MODEL_ID,MEROMERO_HOST_KEY} from '../src/local-model-capabilities.mjs';

const definition={profile:{id:'shared-claude',providerKind:'local',backend:'claude',model:MEROMERO_MODEL_ID,runtime:'llama-cpp-b10982',kvCacheQuantization:'q8_0',reasoning:'off',endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:65536,maxOutputTokens:4096,usageProtocol:'anthropic-normalized-v1'},host:{address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/test-key',hostKeyAlias:'test-host',modelKey:MEROMERO_HOST_KEY,comfyPort:8188}};
const deferred=()=>{let resolve;const promise=new Promise(r=>{resolve=r;});return {promise,resolve};};
test('share actual load configuration across protocol, effort and credential differences',()=>{
  const other={...definition,profile:{...definition.profile,id:'shared-codex',backend:'codex',usageProtocol:'openai-responses-v1',reasoning:'on',maxOutputTokens:8192,credentialEnv:'OFFICESTRA_LOCAL_OTHER_TOKEN',credentialVersion:'v2',endpoint:'http://127.0.0.1:40001'}};
  assert.equal(localModelPoolKey(definition),localModelPoolKey(other));
  assert.notEqual(localModelPoolKey(definition),localModelPoolKey({...other,profile:{...other.profile,contextWindow:32768}}));
  for(const field of ['address','keyPath','hostKeyAlias','user','modelKey'])assert.notEqual(localModelPoolKey(definition),localModelPoolKey({...other,host:{...other.host,[field]:'different'}}),field);
  const qwen={...other,host:{...other.host,modelKey:'qwen3.8-27b'},profile:{...other.profile,model:'officestra-qwen38-27b-uncensored-q4km',reasoning:'medium'}};
  assert.notEqual(localModelPoolKey(definition),localModelPoolKey(qwen));
  const lm={...qwen,profile:{...qwen.profile}};delete lm.profile.runtime;
  assert.notEqual(localModelPoolKey(qwen),localModelPoolKey(lm));
});

async function fixture(t,{idleMs=10000,onRequest=()=>{},release=async()=>{},waitForIdle,retainModels=false}={}){
  const requests=[],specs=[],events=[];let starts=0,releases=0,inflight=0,maxInflight=0;
  const upstream=createServer(async(req,res)=>{
    let raw='';for await(const chunk of req)raw+=chunk;
    const body=JSON.parse(raw);requests.push({url:req.url,body});
    inflight++;maxInflight=Math.max(maxInflight,inflight);
    res.once('close',()=>inflight--);
    if(await onRequest(req,res,body,requests.length)===false)return;
    res.setHeader('content-type','application/json');
    res.end(JSON.stringify(req.url==='/v1/responses'
      ? {object:'response',id:'r',status:'completed',output:[{type:'message',role:'assistant',content:[{type:'output_text',text:body.input}]}],usage:{input_tokens:12,input_tokens_details:{cached_tokens:0},output_tokens:2,total_tokens:14}}
      : {type:'message',id:'m',stop_reason:'end_turn',content:[{type:'text',text:body.messages[0].content}],usage:{input_tokens:12,cache_read_input_tokens:0,output_tokens:2}}));
  });
  await new Promise(r=>upstream.listen(0,'127.0.0.1',r));
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',idleMs,retainModels,broadcast:e=>events.push(e),hostFactory:()=>({start:async()=>{
    starts++;
    return {upstream:`http://127.0.0.1:${upstream.address().port}`,alive:()=>true,waitForIdle,sample:async()=>({vramPct:95,ramPct:50,busy:false,sampledAt:Date.now()}),release:async()=>{releases++;await release();}};
  }})});
  t.after(async()=>{await Promise.all(specs.map(s=>s.release()));await service.shutdown();upstream.closeAllConnections();await new Promise(r=>upstream.close(r));});
  async function launch(backend,mode='gui',reasoning=backend==='claude'?'off':'on',id='shared-'+backend){
    const profile={...definition.profile,id,backend,reasoning,usageProtocol:backend==='claude'?'anthropic-normalized-v1':'openai-responses-v1'};
    const character={id,backend,model:profile.model,effort:'default',fastMode:false,permission:backend==='claude'?'plan':'read-only',localProfile:{...definition,profile}};
    const spec=await service.launch({character,mode,workdir:'/tmp',executable:'/test/'+backend});specs.push(spec);
    const post=(marker,signal)=>fetch(spec.profile.endpoint+(backend==='claude'?'/v1/messages':'/v1/responses'),{method:'POST',signal,headers:{'x-api-key':backend==='claude'?spec.env.ANTHROPIC_API_KEY:spec.env[profile.credentialEnv]},body:JSON.stringify(backend==='claude'?{model:profile.model,max_tokens:32,messages:[{role:'user',content:marker}]}:{model:profile.model,input:marker})});
    return {spec,post};
  }
  return {launch,service,requests,events,counts:()=>({starts,releases,maxInflight})};
}
test('Claude/Codex GUI and terminal share one server while requests and thinking remain separate',async t=>{
  const s=await fixture(t),clients=[];
  for(const mode of ['gui','terminal'])for(const backend of ['claude','codex'])clients.push(await s.launch(backend,mode));
  const results=await Promise.all(clients.map((c,i)=>c.post('PRIVATE_'+i).then(async r=>{assert.equal(r.status,200);return r.json();})));
  results.forEach((r,i)=>{const text=JSON.stringify(r);assert.match(text,new RegExp('PRIVATE_'+i));for(let j=0;j<4;j++)if(j!==i)assert.ok(!text.includes('PRIVATE_'+j));});
  assert.deepEqual(s.counts(),{starts:1,releases:0,maxInflight:1});
  assert.ok(s.events.some(e=>e.error==='같은 모델의 앞선 요청을 기다리는 중'));
  for(const r of s.requests)assert.deepEqual(r.body.chat_template_kwargs,{enable_thinking:r.url==='/v1/responses'});
  const changed=await s.launch('claude','gui','on');
  assert.equal((await changed.post('THINK_ON')).status,200);
  assert.equal(s.requests.at(-1).body.chat_template_kwargs.enable_thinking,true);
  assert.equal(s.counts().starts,1);
});
test('FIFO waiter cancellation and closing its terminal never stop another profile generation',async t=>{
  const entered=deferred(),finish=deferred();t.after(()=>finish.resolve());
  const s=await fixture(t,{onRequest:async(_q,_r,_b,n)=>{if(n===1){entered.resolve();await finish.promise;}}});
  const a=await s.launch('claude','terminal'),b=await s.launch('codex','terminal');
  const first=a.post('FIRST');await entered.promise;
  const c=new AbortController();const queued=b.post('CANCEL_ME',c.signal).catch(()=>null);
  await delay(20);c.abort();await queued;await b.spec.release();
  assert.equal(s.requests.length,1);assert.equal(s.counts().releases,0);
  finish.resolve();assert.equal((await first).status,200);
  const b2=await s.launch('codex','terminal');assert.equal((await b2.post('SECOND')).status,200);
  assert.equal(s.counts().starts,1);
  await a.spec.release();assert.equal(s.counts().releases,0);
  assert.equal((await b2.post('THIRD')).status,200);
  await b2.spec.release();assert.equal(s.counts().releases,1);
});
test('cancelling active inference releases the shared model before a queued peer reloads',async t=>{
  const entered=deferred(),cleaning=deferred(),finish=deferred();t.after(()=>finish.resolve());
  const s=await fixture(t,{onRequest:(_q,_r,_b,n)=>{if(n===3){entered.resolve();return false;}},release:async()=>{cleaning.resolve();await finish.promise;}});
  const a=await s.launch('claude'),b=await s.launch('codex');
  await(await a.post('PRIME_A')).text();await(await b.post('PRIME_B')).text();
  const c=new AbortController();const first=a.post('CANCEL_ACTIVE',c.signal).catch(()=>null);await entered.promise;
  const next=b.post('NEXT');await delay(20);c.abort();await first;await cleaning.promise;
  assert.equal(s.counts().starts,1);assert.equal(s.requests.length,3);
  finish.resolve();assert.equal((await next).status,200);
  assert.equal(s.counts().starts,2);assert.equal(s.requests.length,4);
});
test('shared idle unload waits for the active peer and keeps terminal endpoints reusable',async t=>{
  const entered=deferred(),finish=deferred();t.after(()=>finish.resolve());
  const s=await fixture(t,{idleMs:80,onRequest:async(_q,_r,_b,n)=>{if(n===2){entered.resolve();await finish.promise;}}});
  const a=await s.launch('claude','terminal'),b=await s.launch('codex','terminal');
  await(await a.post('IDLE_SOON')).text();
  const second=b.post('GENERATING');await entered.promise;await delay(150);
  assert.equal(s.counts().releases,0);finish.resolve();await(await second).text();
  await delay(180);assert.equal(s.counts().releases,1);
  assert.equal((await a.post('RELOAD')).status,200);assert.equal(s.counts().starts,2);
});
test('failed shared cleanup blocks all profiles from starting a replacement',async()=>{
  const pool=new LocalModelPool(),group=pool.group(definition),signal=new AbortController().signal;
  const unlock=await pool.acquire(group,signal);let starts=0;
  const resource=await pool.borrow(group,async()=>{starts++;return {alive:()=>true,release:async()=>{throw Error('cleanup failed');}};});
  await assert.rejects(resource.release('Request cancelled'),/cleanup failed/);
  await assert.rejects(unlock(),/cleanup failed/);
  const next=await pool.acquire(group,signal);
  await assert.rejects(pool.borrow(group,async()=>{starts++;}),/cleanup failed/);
  await assert.rejects(next(),/cleanup failed/);assert.equal(starts,1);
});
test('a lost shared tunnel is cleaned before another profile borrows a new resource',async()=>{
  const pool=new LocalModelPool(),group=pool.group(definition),signal=new AbortController().signal;
  let alive=true,starts=0,releases=0;
  const start=async()=>{starts++;return {alive:()=>alive,release:async()=>{releases++;}};};
  let unlock=await pool.acquire(group,signal);const old=await pool.borrow(group,start);await unlock();
  alive=false;unlock=await pool.acquire(group,signal);
  const fresh=await pool.borrow(group,async()=>{alive=true;return start();});
  assert.equal(old.alive(),false);assert.equal(fresh.alive(),true);
  assert.equal(starts,2);assert.equal(releases,1);
  await old.release('idle');assert.equal(fresh.alive(),true);
  await fresh.release();await unlock();assert.equal(releases,2);
});
test('failed cleanup can retry after SSH recovers without discarding ownership early',async()=>{
  const pool=new LocalModelPool(),group=pool.group(definition);
  let attempts=0,fail=true;
  const pending=deferred();
  const record={valid:true,refs:new Set(),resource:{release:async()=>{
    attempts++;if(fail)throw Error('SSH failed');await pending.promise;
  }}};
  group.record=record;
  await assert.rejects(pool.dispose(group,record),/SSH failed/);
  assert.equal(group.record,record);assert.equal(record.valid,false);
  fail=false;
  const a=pool.dispose(group,record),b=pool.dispose(group,record);
  await delay(0);
  assert.equal(attempts,2,'Concurrent retry callers must share one remote cleanup');
  assert.equal(group.record,record,'Ownership must remain until cleanup succeeds');
  pending.resolve();await Promise.all([a,b]);assert.equal(group.record,null);
});

test('explicit model start clears failed bridge only after shared cleanup succeeds',async t=>{
  let broken=true;
  const s=await fixture(t,{retainModels:true,release:async()=>{if(broken)throw Error('SSH failed');}});
  const a=await s.launch('claude');
  await(await a.post('BEFORE')).text();
  const entry=[...s.service.entries.values()][0];
  await entry.bridge.stop('SSH failed');entry.cleanupFailed=true;
  assert.equal(entry.bridge.status.cleanupComplete,false);
  await assert.rejects(s.service.controlModel(entry.definition,'start'),/SSH failed/);
  assert.equal(s.counts().starts,1,'No replacement until cleanup is confirmed');
  broken=false;
  await s.service.controlModel(entry.definition,'start');
  assert.equal(entry.cleanupFailed,false);assert.equal(entry.bridge,null);
  const response=await a.post('AFTER');
  assert.equal(response.status,200);await response.text();
  assert.equal(s.counts().starts,2);
});
test('model queue serves waiters FIFO and removes cancelled requests without replay',async()=>{
  const pool=new LocalModelPool(),group=pool.group(definition),signal=new AbortController().signal;
  const order=[],unlock=await pool.acquire(group,signal),cancel=new AbortController();
  const next=(n,s=signal)=>pool.acquire(group,s).then(async done=>{order.push(n);await done();});
  const a=next(1),b=next(2,cancel.signal).catch(()=>null),c=next(3);
  cancel.abort();await unlock();await Promise.all([a,b,c]);assert.deepEqual(order,[1,3]);
});
test('confirmed generation cancellation retains one loaded server for GUI and terminal resume',async t=>{
  let checks=0;const reached=deferred(),verified=deferred();t.after(()=>verified.resolve());
  const s=await fixture(t,{onRequest:(_q,_r,_b,n)=>{if(n===1){reached.resolve();return false;}},waitForIdle:async()=>{checks++;await verified.promise;}});
  const a=await s.launch('claude'),b=await s.launch('codex','terminal');
  const cancel=new AbortController(),first=a.post('CANCEL',cancel.signal).catch(()=>null);await reached.promise;
  const next=b.post('NEXT');cancel.abort();await first;await a.spec.release();
  await delay(20);assert.equal(s.requests.length,1);assert.equal(checks,1);assert.equal(s.counts().releases,0);
  verified.resolve();assert.equal((await next).status,200);
  const a2=await s.launch('claude');assert.equal((await a2.post('GUI_RESUME')).status,200);
  assert.equal(s.counts().starts,1);assert.equal(s.counts().releases,0);
});
test('cancelled sole bridge stays warm until idle expiry, then unloads exactly once',async t=>{
  const reached=deferred();
  const s=await fixture(t,{idleMs:60,onRequest:()=>{reached.resolve();return false;},waitForIdle:async()=>{}});
  const a=await s.launch('claude'),cancel=new AbortController();
  const first=a.post('CANCEL',cancel.signal).catch(()=>null);await reached.promise;cancel.abort();await first;
  await delay(20);assert.equal(s.counts().releases,0);
  await delay(100);assert.equal(s.counts().releases,1);
  await s.service.shutdown();assert.equal(s.counts().releases,1);
});
test('unconfirmed generation cancellation unloads and reloads before a new request',async t=>{
  const reached=deferred();
  const s=await fixture(t,{onRequest:(_q,_r,_b,n)=>{if(n===1){reached.resolve();return false;}},waitForIdle:async()=>{throw Error('still processing');}});
  const a=await s.launch('claude'),cancel=new AbortController();
  const first=a.post('CANCEL',cancel.signal).catch(()=>null);await reached.promise;cancel.abort();await first;
  assert.equal((await a.post('NEXT')).status,200);
  assert.equal(s.counts().starts,2);assert.equal(s.counts().releases,1);
});
test('warm model with no bridge still releases on ComfyUI reservation or memory guard',async()=>{
  for(const sample of [{busy:true,vramPct:95,ramPct:50},{busy:false,vramPct:98,ramPct:50},{busy:false,vramPct:95,ramPct:78}]){
    const pool=new LocalModelPool({idleMs:1000,warmPollMs:5}),group=pool.group(definition),signal=new AbortController().signal;
    let releases=0;const released=deferred(),unlock=await pool.acquire(group,signal);
    const resource=await pool.borrow(group,async()=>({alive:()=>true,waitForIdle:async()=>{},sample:async()=>sample,release:async()=>{releases++;released.resolve();}}));
    await resource.release('Request cancelled');await unlock();
    await Promise.race([released.promise,delay(100).then(()=>{assert.equal(releases,1);})]);
    await pool.shutdown();assert.equal(releases,1);
  }
});
test('resident direct model survives GUI retirement and terminal close, then releases on shutdown',async t=>{
  const s=await fixture(t,{idleMs:40,retainModels:true});
  const a=await s.launch('claude');await(await a.post('FIRST')).text();await a.spec.release();
  await delay(100);assert.equal(s.service.status().length,0);assert.equal(s.counts().releases,0);
  const b=await s.launch('codex','terminal');await(await b.post('SECOND')).text();await b.spec.release();
  await delay(100);assert.equal(s.counts().starts,1);assert.equal(s.counts().releases,0);
  await s.service.shutdown();assert.equal(s.counts().releases,1);
});
test('request for a different model evicts only idle same-host resources',async()=>{
  const pool=new LocalModelPool({retainModels:true});const signal=new AbortController().signal;
  const a=pool.group(definition),b=pool.group({...definition,profile:{...definition.profile,contextWindow:32768}});
  let releases=0;const start=async()=>({alive:()=>true,sample:async()=>({busy:false,vramPct:95,ramPct:50}),release:async()=>{releases++;}});
  const unlockA=await pool.acquire(a,signal),old=await pool.borrow(a,start);
  const unlockB=await pool.acquire(b,signal);
  // Host arbitration remains responsible for rejecting this attempt while A
  // generates. The pool must not evict an actively leased peer.
  await assert.rejects(pool.borrow(b,async()=>{throw Error('GPU busy');}),/GPU busy/);
  assert.equal(releases,0);await unlockB();await unlockA();
  const next=await pool.acquire(b,signal),fresh=await pool.borrow(b,start);
  assert.equal(releases,1);assert.equal(old.alive(),false);assert.equal(fresh.alive(),true);
  await next();await pool.shutdown();assert.equal(releases,2);
});
test('explicit profile cleanup finds a resident model after its front door retires',async t=>{
  const s=await fixture(t,{idleMs:30,retainModels:true});
  const a=await s.launch('claude');await(await a.post('FIRST')).text();await a.spec.release();
  await delay(80);assert.equal(s.service.status().length,0);assert.equal(s.counts().releases,0);
  await s.service.closeIdleProfile('shared-claude');assert.equal(s.counts().releases,1);
  await s.service.shutdown();assert.equal(s.counts().releases,1);
});

test('manual start preloads without inference; stop unloads shared GUI and terminal model until explicit restart',async t=>{
  const s=await fixture(t,{retainModels:true});
  assert.equal((await s.service.controlModel(definition,'start')).loaded,true);
  const peer={...definition,profile:{...definition.profile,id:'shared-codex',backend:'codex',usageProtocol:'openai-responses-v1',reasoning:'on'}};
  assert.equal(s.service.modelStatus(peer).state,'ready');
  assert.equal(s.requests.length,0);
  const a=await s.launch('claude','gui'),b=await s.launch('codex','terminal');
  await(await a.post('A')).text();await(await b.post('B')).text();
  assert.equal(s.counts().starts,1);
  assert.equal((await s.service.controlModel(definition,'stop')).state,'stopped');
  assert.equal(s.service.modelStatus(peer).state,'stopped');
  assert.equal(s.counts().releases,1);
  for(const c of [a,b])assert.equal((await c.post('BLOCKED')).status,503);
  await assert.rejects(s.launch('claude'),/모델 실행/);
  assert.equal(s.counts().starts,1);assert.equal(s.requests.length,2);
  await s.service.controlModel(definition,'start');
  for(const c of [a,b])assert.equal((await c.post('RESUMED')).status,200);
  assert.equal(s.counts().starts,2);
});

test('stop cancels active and queued shared requests, keeps endpoints, and reports unloaded only after release',async t=>{
  const reached=deferred(),releaseEntered=deferred(),finishRelease=deferred();
  t.after(()=>finishRelease.resolve());
  const s=await fixture(t,{retainModels:true,waitForIdle:async()=>{},
    onRequest:(_q,_r,_b,n)=>{if(n===1){reached.resolve();return false;}},
    release:async()=>{releaseEntered.resolve();await finishRelease.promise;}});
  const a=await s.launch('claude'),b=await s.launch('codex','terminal');
  const first=a.post('ACTIVE').then(r=>r.text()).catch(()=>null);await reached.promise;
  const queued=b.post('QUEUED').then(r=>r.text()).catch(()=>null);await delay(20);
  const stop=s.service.controlModel(definition,'stop');
  await releaseEntered.promise;
  assert.equal(s.service.modelStatus(definition).state,'stopping');
  await assert.rejects(s.service.controlModel(definition,'start'),/처리 중/);
  finishRelease.resolve();await stop;await Promise.all([first,queued]);
  assert.equal(s.requests.length,1);assert.equal(s.counts().releases,1);
  assert.equal(s.service.modelStatus(definition).loaded,false);
  assert.equal(s.service.modelStatus(definition).state,'stopped');
});

test('failed memory release is an error, never stopped or silently restarted',async()=>{
  let releases=0;
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',hostFactory:()=>({start:async()=>({
    alive:()=>true,sample:async()=>({busy:false,vramPct:95,ramPct:50}),release:async()=>{releases++;throw Error('release failed');},
  })})});
  await service.controlModel(definition,'start');
  await assert.rejects(service.controlModel(definition,'stop'),/release failed/);
  assert.equal(service.modelStatus(definition).state,'error');
  await assert.rejects(service.controlModel(definition,'start'),/release failed/);
  assert.equal(releases,2,'A new start retries cleanup, but never starts over an unconfirmed process');
  await assert.rejects(service.shutdown(),/release failed/);
});

test('shutdown cancels a pending manual start and waits for its cleanup',async()=>{
  const entered=deferred();
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',hostFactory:()=>({start:({signal})=>new Promise((_,reject)=>{
    signal.addEventListener('abort',()=>reject(Error('start aborted')),{once:true});entered.resolve();
  })})});
  const started=assert.rejects(service.controlModel(definition,'start'),/start aborted/);
  await entered.promise;
  await assert.rejects(service.closeIdleProfile(definition.profile.id),/처리 중/);
  await service.shutdown();await started;
  assert.equal(service.modelControls.size,0);
});
