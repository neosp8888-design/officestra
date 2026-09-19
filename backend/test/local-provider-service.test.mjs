import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile, access } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { LocalProviderService, resolveLocalCharacter, snapshotLocalTurn, validateLocalResume, localResumeCompatible, LOCAL_PROVIDER_SAMPLE_TIMEOUT_MS } from '../src/local-provider-service.mjs';
import { LocalHostBusyError, WindowsLMStudioHost, verifyLocalRuntimeVersion, LOCAL_HOST_LOAD_CONFIG, LOCAL_HOST_MEMORY_BUDGET, LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS, localHostResourceSampleScript, localHostListenerPIDScript, localHostMemoryExceeded, localHostLoadConfig } from '../src/local-provider-host.mjs';
import { characterSettingsRequireNewSession } from '../src/configuration.mjs';
import { AgentRuntime } from '../src/agent-runtime.mjs';
import { TerminalSessionManager } from '../src/terminal-sessions.mjs';

const definition={profile:{id:'local-test',providerKind:'local',backend:'claude',model:'test-model',endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:32768,maxOutputTokens:4096,usageProtocol:'anthropic-normalized-v1'},host:{address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/test-key',hostKeyAlias:'test-host',modelKey:'test/model',comfyPort:8188}};
const character={id:'isolated-validation',backend:'claude',model:definition.profile.model,effort:'default',fastMode:false,permission:'plan',localProfile:definition};
test('local Codex uses authenticated Responses front door, preserves cancellation and never resumes Claude identity',async t=>{
  const codexDefinition={...definition,profile:{...definition.profile,id:'local-codex-test',backend:'codex',usageProtocol:'openai-responses-v1'}};
  const codexCharacter={...character,backend:'codex',permission:'read-only',localProfile:codexDefinition,config:{localProfileId:codexDefinition.profile.id}};
  const queries={query:async()=>({rows:[{definition:codexDefinition}]})};
  const resolved=await resolveLocalCharacter(queries,{...codexCharacter,localProfile:undefined});assert.equal(resolved.localProfile.profile.backend,'codex');
  assert.equal(localResumeCompatible(definition,codexDefinition),false);
  const requests=[];
  const upstream=createServer(async(req,res)=>{let body='';for await(const c of req)body+=c;requests.push({url:req.url,body:JSON.parse(body)});res.setHeader('content-type','application/json');res.end(JSON.stringify({id:'response-test',status:'completed',output:[],usage:{input_tokens:12,output_tokens:2,total_tokens:14}}));});
  await new Promise(r=>upstream.listen(0,'127.0.0.1',r));
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',hostFactory:()=>({start:async()=>({upstream:`http://127.0.0.1:${upstream.address().port}`,sample:async()=>({vramPct:70,ramPct:30,sampledAt:Date.now(),busy:false}),alive:()=>true,release:async()=>{}})})});
  t.after(async()=>{await service.shutdown();upstream.closeAllConnections();await new Promise(r=>upstream.close(r));});
  const spec=await service.launch({character:codexCharacter,mode:'gui',workdir:'/tmp',executable:'/test/codex',prompt:'hello'});
  const catalogArg=spec.args.find(x=>x.startsWith('model_catalog_json='));
  const catalogPath=JSON.parse(catalogArg.slice(catalogArg.indexOf('=')+1));
  const pinned=JSON.parse(await readFile(catalogPath,'utf8'));
  assert.equal(pinned.models[0].slug,character.model);
  assert.equal(pinned.models[0].supports_search_tool,true);
  const baseArg=spec.args.find(x=>x.startsWith('model_providers.')&&x.includes('.base_url='));
  const base=JSON.parse(baseArg.slice(baseArg.indexOf('=')+1));
  const token=spec.env[definition.profile.credentialEnv];assert.ok(token);
  const catalog=await fetch(base+'/models?client_version=0.154.0',{headers:{authorization:`Bearer ${token}`}}).then(r=>r.json());
  assert.equal(catalog.models.length,1);assert.equal(catalog.models[0].slug,character.model);assert.equal(catalog.models[0].context_window,32768);assert.deepEqual(catalog.models[0].service_tiers,[]);
  assert.equal((await fetch(base+'/models')).status,401);assert.equal(requests.length,0);
  const res=await fetch(base+'/responses',{method:'POST',headers:{authorization:`Bearer ${token}`},body:JSON.stringify({model:character.model,input:'hello'})});
  assert.equal(res.status,200);assert.equal((await res.json()).usage.input_tokens,12);assert.equal(requests[0].url,'/v1/responses');assert.equal(requests[0].body.max_output_tokens,undefined);
  assert.equal((await fetch(base+'/responses',{method:'POST',headers:{authorization:`Bearer ${token}`},body:JSON.stringify({model:character.model,input:'hello',max_output_tokens:32769})})).status,400);
  assert.equal(requests.length,1);
  assert.equal((await fetch(base+'/messages',{method:'POST',headers:{authorization:`Bearer ${token}`},body:'{}'})).status,404);
  await spec.release();
  await assert.rejects(access(catalogPath),{code:'ENOENT'});
});
test('64K uses quantized KV only; original 32K load policy stays unchanged',()=>{
  assert.equal(localHostLoadConfig(definition.profile),LOCAL_HOST_LOAD_CONFIG);
  const config=localHostLoadConfig({...definition.profile,contextWindow:65536});
  assert.equal(config.contextLength,65536);assert.equal(config.gpu.ratio,1);
  assert.equal(config.llamaKCacheQuantizationType,'q8_0');assert.equal(config.llamaVCacheQuantizationType,'q8_0');
  assert.equal(config.flashAttention,true);assert.equal(config.gpuStrictVramCap,false);
  assert.throws(()=>localHostLoadConfig({...definition.profile,contextWindow:49152}));
});
test('explicit Codex KV8 retains 64K and GPU policy without changing Claude defaults',()=>{
  const profile={...definition.profile,backend:'codex',usageProtocol:'openai-responses-v1',contextWindow:65536,kvCacheQuantization:'q8_0'};
  const config=localHostLoadConfig(profile);
  assert.equal(config.contextLength,65536);
  assert.equal(config.llamaKCacheQuantizationType,'q8_0');
  assert.equal(config.llamaVCacheQuantizationType,'q8_0');
  assert.equal(config.gpu.ratio,1);
  assert.equal(config.evalBatchSize,128);
  assert.equal(localHostLoadConfig({...definition.profile,contextWindow:65536}).llamaKCacheQuantizationType,'q8_0');
  assert.equal(localHostLoadConfig(definition.profile),LOCAL_HOST_LOAD_CONFIG);
  assert.throws(()=>localHostLoadConfig({...profile,backend:'claude'}),/KV/);
  assert.throws(()=>localHostLoadConfig({...profile,contextWindow:32768}),/KV/);
  assert.throws(()=>localHostLoadConfig({...profile,kvCacheQuantization:'unknown'}),/KV/);
  const original={...definition,profile:{...profile}};delete original.profile.kvCacheQuantization;
  assert.equal(localResumeCompatible(original,{...definition,profile}),false);
});
test('context increase and route changes preserve session; pinned identity changes remain blocked',async()=>{
  const larger={...definition,profile:{...definition.profile,contextWindow:65536}};
  assert.equal(localResumeCompatible(definition,larger),true);
  assert.equal(localResumeCompatible(larger,definition),false);
  for(const field of ['id','providerKind','backend','model','credentialEnv','credentialVersion','endpoint','usageProtocol']){
    assert.equal(localResumeCompatible(definition,{...larger,profile:{...larger.profile,[field]:'changed'}}),false,field);
  }
  assert.equal(localResumeCompatible(definition,{...larger,profile:{...larger.profile,maxOutputTokens:16384}}),true);
  assert.equal(localResumeCompatible(definition,{...larger,host:{...larger.host,address:'other-host'}}),true);
  for(const field of ['hostKeyAlias','user','keyPath','sshPort','modelKey']){
    assert.equal(localResumeCompatible(definition,{...larger,host:{...larger.host,address:'other-host',[field]:'different'}}),false,field);
  }
  const client={query:async()=>({rows:[{provider_kind:'local',provider_snapshot:definition}]})};
  await validateLocalResume(client,'same-session',{...character,localProfile:larger},'same-external-id');
});
test('address-only resume keeps context and rejects an unpinned identity',()=>{
  const changed={...definition,host:{...definition.host,address:'222.109.147.73'}};
  assert.equal(localResumeCompatible(definition,changed),true);
  assert.equal(localResumeCompatible({...definition,host:{...definition.host,hostKeyAlias:''}},{...changed,host:{...changed.host,hostKeyAlias:''}}),false);
});
test('idle profile cleanup uses verified new route, never interrupts active users',async()=>{
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused'});
  const calls=[];
  const e={definition,users:1,active:null,resource:{reconnectAddress:ip=>calls.push(ip)}};
  service.entries.set('test',e);service.close=async entry=>{assert.equal(entry,e);calls.push('closed');};
  await assert.rejects(service.closeIdleProfile(definition.profile.id,'222.109.147.73'),/사용 중/);
  assert.deepEqual(calls,[]);
  e.users=0;await service.closeIdleProfile(definition.profile.id,'222.109.147.73');
  assert.deepEqual(calls,['222.109.147.73','closed']);
});
test('a live 32K entry cannot silently be reused as a 64K model',async t=>{
  const s=await setup(t);
  await assert.rejects(()=>s.service.launch({character:{...character,localProfile:{...definition,profile:{...definition.profile,contextWindow:65536}}},mode:'gui',workdir:'/tmp',executable:'/test/claude'}),/Close existing/);
});
async function setup(t,{busy=0,fail=false,idleMs=10000,vramPct=70,onRequest,onRelease,cleanupWaitMs=15000,mode='terminal'}={}) {
  let starts=0,releases=0,requests=0;
  const server=createServer(async(req,res)=>{for await(const _ of req){} requests++;if(onRequest?.(req,res,requests)===false)return;if(fail){fail=false;res.destroy();return;}res.setHeader('content-type','application/json');res.end(JSON.stringify({type:'message',content:[{type:'text',text:'ok'}],usage:{input_tokens:100,cache_read_input_tokens:20,output_tokens:2}}));});
  await new Promise(r=>server.listen(0,'127.0.0.1',r));
  const events=[];
  const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',idleMs,cleanupWaitMs,retryMs:10,waitMs:200,broadcast:e=>events.push(e),hostFactory:()=>({start:async()=>{
    starts++;if(busy-->0)throw new LocalHostBusyError('busy');
    return {upstream:`http://127.0.0.1:${server.address().port}`,sample:async()=>({vramPct,ramPct:55,busy:false,sampledAt:Date.now()}),alive:()=>true,release:async()=>{releases++;await onRelease?.();}};
  }})});
  const spec=await service.launch({character,mode,workdir:'/tmp',executable:'/test/claude',prompt:'hello'});
  t.after(async()=>{await spec.release();await service.shutdown();server.closeAllConnections();await new Promise(r=>server.close(r));});
  const post=(headers={},signal)=>fetch(spec.env.ANTHROPIC_BASE_URL+'/v1/messages',{method:'POST',signal,headers:{'x-api-key':spec.env.ANTHROPIC_API_KEY,...headers},body:JSON.stringify({model:character.model,max_tokens:32,messages:[{role:'user',content:'hello'}]})});
  return {service,spec,post,events,counts:()=>({starts,releases,requests})};
}
const deferred=()=>{let resolve;const promise=new Promise(r=>{resolve=r;});return {promise,resolve};};
test('terminal cancel then immediate input waits for cleanup; concurrent input still 409',async t=>{
  const received=deferred(),cleaning=deferred(),release=deferred();
  t.after(()=>release.resolve());
  const s=await setup(t,{onRequest:(_q,_r,n)=>{if(n===1){received.resolve();return false;}},onRelease:async()=>{cleaning.resolve();await release.promise;}});
  const controller=new AbortController();const first=s.post({},controller.signal).catch(()=>null);
  await received.promise;assert.equal((await s.post()).status,409);
  controller.abort();await first;await cleaning.promise;
  const next=s.post();await delay(20);assert.equal(s.counts().requests,1);
  release.resolve();assert.equal((await next).status,200);
  assert.equal(s.counts().requests,2);assert.equal(s.counts().starts,2);
});
test('GUI next launch waits for cancelled request cleanup without replaying it',async t=>{
  const received=deferred(),cleaning=deferred(),release=deferred();t.after(()=>release.resolve());
  const s=await setup(t,{mode:'gui',onRequest:(_q,_r,n)=>{if(n===1){received.resolve();return false;}},onRelease:async()=>{cleaning.resolve();await release.promise;}});
  const first=s.post().catch(()=>null);await received.promise;await s.spec.release();await cleaning.promise;
  let launched=false;const pending=s.service.launch({character,mode:'gui',workdir:'/tmp',executable:'/test/claude',prompt:'next'}).then(spec=>{launched=true;return spec;});
  await delay(20);assert.equal(launched,false);release.resolve();const spec=await pending;
  await first;assert.equal((await s.post()).status,200);await spec.release();assert.equal(s.counts().requests,2);
});
test('cleanup wait timeout and cancelled waiter do not start a new inference',async t=>{
  const received=deferred(),cleaning=deferred(),release=deferred();t.after(()=>release.resolve());
  const s=await setup(t,{cleanupWaitMs:50,onRequest:(_q,_r,n)=>{if(n===1){received.resolve();return false;}},onRelease:async()=>{cleaning.resolve();await release.promise;}});
  const c=new AbortController();const first=s.post({},c.signal).catch(()=>null);await received.promise;c.abort();await first;await cleaning.promise;
  const waiter=new AbortController();const cancelled=s.post({},waiter.signal).catch(()=>null);await delay(10);waiter.abort();await cancelled;
  const timeout=await s.post();assert.equal(timeout.status,503);assert.match(await timeout.text(),/cleanup/);assert.equal(s.counts().requests,1);
  release.resolve();await delay(20);assert.equal((await s.post()).status,200);assert.equal(s.counts().requests,2);
});
test('unconfirmed resource release fails closed rather than provisioning again',async t=>{
  const received=deferred();let broken=true;
  const s=await setup(t,{onRequest:(_q,_r,n)=>{if(n===1){received.resolve();return false;}},onRelease:()=>{if(broken)throw new Error('release failed');}});
  const c=new AbortController();const first=s.post({},c.signal).catch(()=>null);await received.promise;c.abort();await first;await delay(30);
  assert.equal((await s.post()).status,503);assert.equal(s.counts().starts,1);broken=false;
});
test('lazy startup, authenticated front door, raw usage normalizes and final release unloads',async t=>{
  const s=await setup(t);assert.equal(s.counts().starts,0);
  assert.equal((await s.post({'x-api-key':'wrong'})).status,401);assert.equal(s.counts().starts,0);
  const body=await(await s.post()).json();assert.equal(body.usage.input_tokens,80);assert.equal(body.usage.cache_read_input_tokens,20);
  await s.spec.release();assert.deepEqual(s.counts(),{starts:1,releases:1,requests:1});assert.deepEqual(s.service.status(),[]);
});
test('production service passes the user-approved GPU 98 budget through to its bridge',async t=>{
  const s=await setup(t,{vramPct:96.12});assert.equal((await s.post()).status,200);
});
test('reasoning changes preserve session identity; other model/profile changes do not',()=>{
  const original={profile:{id:'test',contextWindow:65536,model:'qwen'},host:{modelKey:'qwen3.8-27b'}};
  assert.equal(localResumeCompatible(original,{...original,profile:{...original.profile,reasoning:'on'}}),true);
  assert.equal(localResumeCompatible(original,{...original,profile:{...original.profile,model:'other',reasoning:'on'}}),false);
});
test('host requests full GPU at 32K with no silent partial offload policy',()=>{
  assert.equal(LOCAL_HOST_LOAD_CONFIG.gpu.ratio,1);
  assert.equal(LOCAL_HOST_LOAD_CONFIG.gpuStrictVramCap,false);
  assert.equal(LOCAL_HOST_LOAD_CONFIG.contextLength,32768);
  assert.deepEqual(LOCAL_HOST_MEMORY_BUDGET,{vramGuardPercent:98,ramGuardPercent:77});
  assert.equal(localHostMemoryExceeded({vramPct:94.9,ramPct:76.9}),false);
  assert.equal(localHostMemoryExceeded({vramPct:96.12,ramPct:55}),false);
  assert.equal(localHostMemoryExceeded({vramPct:97.99,ramPct:55}),false);
  assert.equal(localHostMemoryExceeded({vramPct:98,ramPct:55}),true);
  assert.equal(localHostMemoryExceeded({vramPct:90,ramPct:77}),true);
});
test('resource sampling avoids slow WMI inventory and gives SSH cleanup headroom',()=>{
  const script=localHostResourceSampleScript(8188);
  assert.match(script,/Microsoft\.VisualBasic\.Devices\.ComputerInfo/);
  assert.match(script,/GetActiveTcpListeners/);
  assert.doesNotMatch(script,/Get-CimInstance Win32_OperatingSystem/);
  assert.doesNotMatch(script,/Get-NetTCPConnection/);
  assert.equal(LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS,30000);
  assert.ok(LOCAL_PROVIDER_SAMPLE_TIMEOUT_MS>LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS);
  assert.throws(()=>localHostResourceSampleScript(0),/port/);
  const listener=localHostListenerPIDScript(1234);
  assert.match(listener,/netstat -ano -p TCP/);
  assert.match(listener,/:1234/);
  assert.doesNotMatch(listener,/Get-NetTCPConnection/);
  assert.throws(()=>localHostListenerPIDScript(0),/port/);
});
test('ComfyUI/lease busy waits, resumes once available without duplicate inference',async t=>{
  const s=await setup(t,{busy:2});assert.equal((await s.post()).status,200);
  assert.equal(s.counts().starts,3);assert.equal(s.counts().requests,1);assert.ok(s.events.some(e=>e.state==='waiting'));
});
test('idle model unload keeps terminal address stable and next input reloads',async t=>{
  const s=await setup(t,{idleMs:40});const url=s.spec.env.ANTHROPIC_BASE_URL;
  await(await s.post()).text();await delay(100);assert.equal(s.counts().releases,1);
  assert.equal((await s.post()).status,200);assert.equal(s.spec.env.ANTHROPIC_BASE_URL,url);assert.equal(s.counts().starts,2);
});
test('lost inference is not replayed; next user request may recover',async t=>{
  const s=await setup(t,{fail:true});await(await s.post()).text();assert.equal(s.counts().requests,1);
  assert.equal(s.service.status()[0].state,'error');
  assert.equal((await s.post()).status,200);assert.equal(s.counts().requests,2);
});
test('closing a waiting session cancels provisioning and leaves no listeners',async t=>{
  const s=await setup(t,{busy:999});const pending=s.post().catch(()=>null);await delay(25);await s.spec.release();await pending;
  assert.equal(s.counts().requests,0);assert.deepEqual(s.service.status(),[]);
});
test('cloud stays untouched; local profile must explicitly match and snapshot',async()=>{
  let queries=0;const client={query:async()=>{queries++;return {rows:[{definition}]};}};
  const cloud={backend:'claude',config:{}};assert.equal(await resolveLocalCharacter(client,cloud),cloud);assert.equal(queries,0);
  const local={...character,config:{localProfileId:definition.profile.id}};delete local.localProfile;
  await resolveLocalCharacter(client,local);assert.equal(local.localProfile.profile.id,definition.profile.id);
  await snapshotLocalTurn(client,'turn',local);assert.equal(queries,2);
  const routed={...local,config:{...local.config,localHostAddress:'222.109.147.73'}};
  await resolveLocalCharacter(client,routed);
  assert.equal(routed.localProfile.host.address,'222.109.147.73');
  assert.equal(definition.host.address,'127.0.0.1');
  assert.equal(localResumeCompatible(local.localProfile,routed.localProfile),true);
  await assert.rejects(()=>resolveLocalCharacter(client,{...local,effort:'high'}));
});
test('cloud resume cannot silently become local; local resume keeps provider identity',async()=>{
  await assert.rejects(()=>validateLocalResume({query:async()=>({rows:[{provider_kind:'cloud'}]})},'s',character,'external'));
  await validateLocalResume({query:async()=>({rows:[{provider_kind:'local',provider_snapshot:definition}]})},'s',character,'external');
  assert.equal(characterSettingsRequireNewSession({backend:'claude'},{backend:'claude',config:{localProfileId:'local'}}),true);
});
test('recovery record cannot become a remote command; foreign server PID is never stopped',async()=>{
  const host=new WindowsLMStudioHost({host:definition.host,profile:definition.profile,pool:{},stateDirectory:'/tmp/unused'});
  host.owned={model:"bad';Stop-Process",serverPID:1};host.remote=()=>{assert.fail('no remote call for invalid ownership');};
  await assert.rejects(()=>host.cleanupOwned(),/Invalid owned/);
  host.owned={model:'test-model',serverPID:1};host.serverPID=async()=>2;
  await host.cleanupOwned();assert.equal(host.owned,null);
});

test('an unmeasured LM Studio update cannot silently change token normalization',()=>{
  verifyLocalRuntimeVersion({version:'0.4.24',build:1});
  assert.throws(()=>verifyLocalRuntimeVersion({version:'0.4.25',build:1}));
  assert.throws(()=>verifyLocalRuntimeVersion({version:'0.4.24',build:2}));
});

test('GUI dispatch uses local single-process runner and releases on success/failure, not cloud worker',async()=>{
  for(const fail of [false,true]) {
    let launches=0,releases=0;
    const runtime=new AgentRuntime({pool:{},withTransaction:()=>{},workdir:'/tmp',broadcast:()=>{},localProviders:{launch:async o=>{launches++;assert.equal(o.character.localProfile.profile.id,definition.profile.id);return {args:['local'],release:async()=>{releases++;}};}}});
    runtime.executeClaude=()=>assert.fail('cloud worker must not execute');
    runtime.executeSingleProcess=async(_,spec)=>{assert.deepEqual(spec.args,['local']);if(fail)throw new Error('failed');};
    const pending=runtime.execute({character,workdir:'/tmp',prompt:'hello'});
    if(fail)await assert.rejects(pending);else await pending;
    assert.equal(launches,1);assert.equal(releases,1);
  }
});
test('local usage persists actual tokens with null billable price and raw CLI cost audit',async()=>{
  const queries=[];const pool={query:async(text,values)=>{queries.push({text,values});return {rows:[]};}};
  const runtime=new AgentRuntime({pool,withTransaction:()=>{},workdir:'/tmp',broadcast:()=>{}});
  await runtime.persistUsageRecord(pool,{turnID:'t',character,usage:{inputTokens:11098,outputTokens:4,cachedInputTokens:0,reportedCostUsd:0.05559}});
  assert.equal(queries[0].values[1],11098);assert.equal(queries[0].values[5],null);
  assert.equal(JSON.parse(queries[1].values[1]).reportedCostUsd,0.05559);
});
test('terminal launch preserves hooks, event environment and per-turn provider snapshot',async t=>{
  let released=0,begun;
  const s=await setup(t);
  const launch=s.service.launch.bind(s.service);
  const runtime={
    localProviders:{launch:async o=>{const spec=await launch({...o,executable:'/test/claude'});const release=spec.release;return {...spec,release:async()=>{released++;await release();}};}},
    prepareTerminalLaunch:async()=>({character,sessionID:'s',conversationID:'c',externalSessionID:null,workdir:'/tmp'}),
    bindTerminalExternalSession:async()=>{},
    beginTerminalTurn:async o=>{begun=o;return {turnID:'t'};},
    interruptTerminalTurn:async()=>{},
  };
  const manager=new TerminalSessionManager({runtime,broadcast:()=>{}});
  t.after(()=>manager.shutdown());
  const spec=await manager.open(character.id);
  assert.equal(spec.executable,'/test/claude');
  assert.ok(spec.env.OFFICESTRA_TERMINAL_EVENTS_URL);
  const settings=JSON.parse(spec.args[spec.args.indexOf('--settings')+1]);assert.ok(settings.hooks.Stop);assert.ok(settings.hooks.UserPromptSubmit);
  await manager.handleEvent(character.id,{source:'claude',payload:{hook_event_name:'UserPromptSubmit',session_id:'local-session',prompt:'hello'}});
  assert.equal(begun.execution.localProfile.profile.id,definition.profile.id);
  await manager.close(character.id);assert.equal(released,1);
});
