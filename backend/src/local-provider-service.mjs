import { createServer, request } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { pipeline } from 'node:stream';
import { setTimeout as delay } from 'node:timers/promises';
import { mkdtemp, writeFile, unlink, rmdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { LocalInferenceBridge } from './local-inference-bridge.mjs';
import { INCLUSIVE_INPUT_PROFILE, LLAMA_MESSAGES_PROFILE } from './local-usage-normalizer.mjs';
import { RESPONSES_INCLUSIVE_PROFILE, LLAMA_RESPONSES_PROFILE } from './local-responses-usage.mjs';
import { WindowsLlamaCppHost } from './local-llama-host.mjs';
import { WindowsLMStudioHost, LocalHostBusyError, validateHost, LOCAL_HOST_MEMORY_BUDGET, LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS } from './local-provider-host.mjs';
import { normalizeLocalAgentProfile, createLocalAgentLaunch } from './local-agent-profile.mjs';
import { isMeroMero, MEROMERO_MODEL_ID, directReasoningOptions, directDefaultReasoning, directCodexReasoning } from './local-model-capabilities.mjs';
import { isQwen38LMStudioModelKey } from './local-provider-host.mjs';
import { LocalModelPool, localModelPoolKey } from './local-model-pool.mjs';

// Historical presentation only: never rewrite stored execution settings or
// infer an old turn's effort from the employee's current selection.
export const LOCAL_TURN_EFFORT_SQL = `CASE
  WHEN t.provider_kind = 'local' AND t.effort = 'default'
    AND t.provider_snapshot->'profile'->>'runtime' = 'llama-cpp-b10982'
  THEN CASE
    WHEN t.provider_snapshot->'profile'->>'model' = '${MEROMERO_MODEL_ID}'
      THEN CASE WHEN t.provider_snapshot->'profile'->>'reasoning' = 'on' THEN 'on' ELSE 'off' END
    WHEN t.provider_snapshot->'profile'->>'reasoning' IN ('low','medium','xhigh')
      THEN t.provider_snapshot->'profile'->>'reasoning'
    WHEN COALESCE(t.provider_snapshot->'profile'->>'reasoning','default') = 'default' THEN 'xhigh'
    ELSE t.effort END
  ELSE t.effort END`;
export const LOCAL_PROVIDER_SAMPLE_TIMEOUT_MS=LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS+5000;

export function normalizeLocalDefinition(value) {
  return {profile:normalizeLocalAgentProfile(value?.profile),host:validateHost(value?.host)};
}
export function localProfileTitle({profile,host}) {
  if(isMeroMero(profile))return 'G4 MeroMero 31B Uncensored Heretic · llama.cpp';
  // The host key is a routing identifier, not necessarily the loaded model.
  const name=profile.model==='officestra-qwen38-27b-uncensored-q4km'
    ? 'qwen3.8-27b-uncensored' : host.modelKey.split('/').at(-1);
  return name+(profile.runtime==='llama-cpp-b10982'?' · llama.cpp · MTP':' · LM Studio');
}
export function localProfileReasoningOptions({profile,host}){
  return profile.runtime==='llama-cpp-b10982'?directReasoningOptions(profile):isQwen38LMStudioModelKey(host.modelKey)?['default','on','off']:[];
}
export function localProfileDefaultReasoning({profile}){
  return profile.runtime==='llama-cpp-b10982'?directDefaultReasoning(profile):'default';
}
export function localCodexModelCatalog(profile) {
  // Codex asks its custom provider for model metadata, not LM Studio's generic
  // OpenAI models list. Advertise only this pinned model, never cloud tiers.
  return {models:[{
    slug:profile.model,display_name:profile.model,description:`Local ${isMeroMero(profile)?'MeroMero':'Qwen'} on your 4090`,
    default_reasoning_level:profile.runtime==='llama-cpp-b10982'?directCodexReasoning({...profile,reasoning:'default'}):null,
    supported_reasoning_levels:isMeroMero(profile)?[{effort:'none',description:'Thinking off'},{effort:'medium',description:'Thinking on (native toggle)'}]:profile.runtime==='llama-cpp-b10982'?['low','medium','xhigh'].map(e=>({effort:e,description:`Qwen ${e}`})):[],shell_type:'unified_exec',
    visibility:'list',supported_in_api:true,priority:1,additional_speed_tiers:[],service_tiers:[],
    availability_nux:null,upgrade:null,
    base_instructions:`You are a coding assistant running a local ${isMeroMero(profile)?'MeroMero':'Qwen'} model through Codex. Use the available tools to complete the user request. Follow tool documentation and do not invent tool results.`,
    model_messages:null,default_reasoning_summary:'none',support_verbosity:false,
    default_verbosity:null,apply_patch_tool_type:'freeform',web_search_tool_type:'text',
    truncation_policy:{mode:'tokens',limit:10000},supports_image_detail_original:false,
    context_window:profile.contextWindow,max_context_window:profile.contextWindow,
    effective_context_window_percent:100,experimental_supported_tools:[],
    input_modalities:isMeroMero(profile)?['text']:['text','image'],supports_search_tool:true,supports_experimental_context:false,
  }]};
}
export async function resolveLocalCharacter(client,character) {
  const id=character.config?.localProfileId;
  if(!id)return character;
  const result=await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true',[id]);
  if(!result.rows[0])throw new Error('Local profile is disabled or missing');
  const definition=normalizeLocalDefinition(result.rows[0].definition);
  if(character.config?.localHostAddress)definition.host=validateHost({...definition.host,address:character.config.localHostAddress});
  if(character.config?.localReasoning)definition.profile=normalizeLocalAgentProfile({...definition.profile,reasoning:character.config.localReasoning});
  if(character.backend!==definition.profile.backend||character.model!==definition.profile.model||character.effort!=='default'||character.fastMode)throw new Error('Local employee settings do not match the profile');
  character.localProfile=definition;
  return character;
}
export async function snapshotLocalTurn(client,turnID,character) {
  if(character.localProfile)await client.query("UPDATE turns SET provider_kind='local',provider_snapshot=$2::jsonb WHERE id=$1",[turnID,JSON.stringify(character.localProfile)]);
}
export function localResumeCompatible(previous,next) {
  // Address is a route, not session identity. Every connection still verifies
  // the pinned SSH host key; user/key/model/hostKeyAlias changes remain blocked.
  const samePinnedHost=typeof previous?.host?.hostKeyAlias==='string' && previous.host.hostKeyAlias.length>0 && previous.host.hostKeyAlias===next?.host?.hostKeyAlias;
  const strip=d=>d?{...d,host:samePinnedHost?Object.fromEntries(Object.entries(d.host??{}).filter(([k])=>k!=='address')):d.host,profile:Object.fromEntries(Object.entries(d.profile??{}).filter(([k])=>!['reasoning','maxOutputTokens'].includes(k)))}:d;
  previous=strip(previous);next=strip(next);
  if(isDeepStrictEqual(previous,next))return true;
  // A measured window increase does not change model/account/session identity.
  // Do not reuse this for live entries: their already-loaded model stays pinned.
  if(previous?.profile?.contextWindow!==32768||next?.profile?.contextWindow!==65536)return false;
  return isDeepStrictEqual({...previous,profile:{...previous.profile,contextWindow:65536}},next);
}
export async function validateLocalResume(client,sessionID,character,externalID) {
  if(!character.localProfile||!externalID)return;
  const previous=await client.query('SELECT provider_kind,provider_snapshot FROM turns WHERE cli_session_id=$1 ORDER BY started_at DESC LIMIT 1',[sessionID]);
  const row=previous.rows[0];
  if(row?.provider_kind!=='local'||!localResumeCompatible(row.provider_snapshot,character.localProfile))throw new Error('Start a new session before switching between cloud and local providers');
}

// Stable front door outlives the model. Idle unload must not invalidate an
// interactive CLI's ANTHROPIC_BASE_URL. Only the next request may reconnect;
// an interrupted inference is never replayed.
export class LocalProviderService {
  constructor({pool,stateDirectory,broadcast=()=>{},hostFactory=o=>o.profile.runtime==='llama-cpp-b10982'?new WindowsLlamaCppHost(o):new WindowsLMStudioHost(o),bridgeFactory=o=>new LocalInferenceBridge(o),waitMs=300000,retryMs=2000,idleMs=180000,cleanupWaitMs=15000,retainModels=true}) {
    Object.assign(this,{pool,stateDirectory,broadcast,hostFactory,bridgeFactory,waitMs,retryMs,idleMs,cleanupWaitMs});
    this.entries=new Map();this.models=new LocalModelPool({idleMs,retainModels});this.modelControls=new Set();this.closed=false;
  }
  status(){return [...this.entries.values()].map(e=>({id:e.key,profileId:e.definition.profile.id,state:e.state,error:e.error??null,users:e.users,resources:e.resources??null}));}
  modelStatus(definition) {
    const group=this.models.groups.get(localModelPoolKey(definition));
    const entries=[...this.entries.values()].filter(e=>e.group===group);
    const loaded=!!(group?.record?.valid&&group.record.resource.alive());
    const error=group?.controlError??entries.find(e=>e.state==='error')?.error??null;
    const state=group?.controlState??(error?'error':group?.paused?'stopped':entries.some(e=>e.state==='starting')?'starting':entries.some(e=>e.state==='waiting')?'waiting':loaded?'ready':'idle');
    return {state,error,loaded,users:entries.reduce((sum,e)=>sum+e.users,0),active:!!group?.locked,resources:entries.find(e=>e.resources)?.resources??null};
  }
  controlModel(rawDefinition,action) {
    const task=this.applyModelControl(rawDefinition,action);
    this.modelControls.add(task);
    task.then(()=>this.modelControls.delete(task),()=>this.modelControls.delete(task));
    return task;
  }
  async applyModelControl(rawDefinition,action) {
    if(this.closed)throw new Error('Local service is shutting down');
    if(!['start','stop'].includes(action))throw new Error('Unsupported model action');
    const definition=normalizeLocalDefinition(rawDefinition),group=this.models.group(definition);
    if(group.controlState)throw new LocalHostBusyError('모델 실행 또는 중지 처리 중입니다.');
    group.controlState=action==='start'?'starting':'stopping';group.controlError=null;
    const controller=new AbortController();group.controlAbort=action==='start'?controller:null;
    // Gate both existing CLI front doors and newly opened ones until an explicit
    // start succeeds. Stopping preserves their URLs and conversation identity.
    group.paused=true;
    const changed=()=>this.broadcast({type:'local.changed',profileId:definition.profile.id});
    changed();
    let unlock;
    try {
      if(action==='stop') {
        const entries=[...this.entries.values()].filter(e=>e.group===group);
        for(const e of entries)for(const controller of e.controllers)controller.abort();
        await Promise.all(entries.flatMap(e=>[...e.pending]));
        unlock=await this.models.acquire(group,new AbortController().signal);
        for(const e of entries){await e.bridge?.stop();e.bridge=null;e.resource=null;e.cleanupFailed=false;this.change(e,'idle');}
        if(group.record)await this.models.dispose(group,group.record);
      } else {
        unlock=await this.models.acquire(group,controller.signal);
        const host=this.hostFactory({...definition,pool:this.pool,stateDirectory:this.stateDirectory});
        const resource=await this.models.borrow(group,()=>host.start({signal:controller.signal}));
        await resource.release();
        if(group.record)this.models.keepWarm(group,group.record);
        group.paused=false;
        for(const e of this.entries.values())if(e.group===group)this.change(e,'idle');
      }
    } catch(error) {
      group.controlError=error.message;
      throw error;
    } finally {
      try{await unlock?.();}
      finally{group.controlState=null;group.controlAbort=null;changed();}
    }
    return this.modelStatus(definition);
  }
  change(e,state,error=null){e.state=state;e.error=error;this.broadcast({type:'local.changed',profileId:e.definition.profile.id,state,error});}
  async launch({character,...options}) {
    if(this.closed)throw new Error('Local service is shutting down');
    const definition=normalizeLocalDefinition(character.localProfile);
    if(this.models.group(definition).paused)throw new Error('로컬 모델이 중지되어 있습니다. 모델 실행 버튼을 눌러주세요.');
    const id=`${definition.profile.id}:${definition.profile.reasoning??'default'}`;
    let e=this.entries.get(id);
    if(e?.closing){await e.closing;e=this.entries.get(id);}
    if(e&&!isDeepStrictEqual(e.definition,definition))throw new Error('Close existing local sessions before changing their profile');
    if(!e){
      e={key:id,definition,group:this.models.group(definition),controllers:new Set(),users:0,state:'idle',token:randomBytes(32).toString('hex'),bridge:null,closed:false,active:null};
      this.entries.set(id,e);
      e.pending=new Set();
      e.server=createServer((req,res)=>{const task=this.handle(e,req,res).catch(()=>res.destroy());e.pending.add(task);task.finally(()=>e.pending.delete(task));});
      e.server.requestTimeout=0;e.server.headersTimeout=10000;
      e.ready=new Promise((resolve,reject)=>{e.server.once('error',reject);e.server.listen(0,'127.0.0.1',resolve);});
    }
    await e.ready;
    clearTimeout(e.retire);
    await this.awaitCleanup(e);
    if(e.closed)throw new Error('Local session is closed');
    e.users++;
    let released=false,catalogDirectory,catalogPath;
    const release=async()=>{if(released)return;released=true;
      // Temporary metadata cleanup must not strand a GPU lease if the file was
      // already removed by the OS. The live process has exited before release.
      if(catalogPath)await unlink(catalogPath).catch(error=>{if(error.code!=='ENOENT')console.warn('Local catalog cleanup failed:',error.code);});
      if(catalogDirectory)await rmdir(catalogDirectory).catch(error=>{if(error.code!=='ENOENT')console.warn('Local catalog directory cleanup failed:',error.code);});
      e.users--;if(!e.users){
      if(options.mode==='terminal')await this.close(e);
      else {for(const controller of e.controllers)if(controller!==e.active?.abort||!e.active.completed)controller.abort();e.retire=setTimeout(()=>{this.close(e).catch(error=>this.change(e,'error',error.message));},this.idleMs);e.retire.unref();}
    }};
    try {
      const profile={...definition.profile,endpoint:`http://127.0.0.1:${e.server.address().port}`};
      if(profile.backend==='codex'){
        // A shared fresh cloud models_cache.json can suppress provider refresh.
        // Pin metadata per process; never modify the user's shared Codex home.
        catalogDirectory=await mkdtemp(join(tmpdir(),'officestra-local-catalog-'));
        catalogPath=join(catalogDirectory,'models.json');
        await writeFile(catalogPath,JSON.stringify(localCodexModelCatalog(profile)),{mode:0o600});
      }
      return {...createLocalAgentLaunch({...options,character,profile,catalogPath,baseEnvironment:{...(options.baseEnvironment??process.env),[profile.credentialEnv]:e.token}}),release};
    }catch(error){await release();throw error;}
  }
  async awaitCleanup(e,signal) {
    // Completed HTTP streams may still be draining when Codex starts its next
    // tool round. Wait for their cleanup, but reject concurrent inference.
    const previous=e.active;
    if(previous&&(previous.completed||previous.abort.signal.aborted)){
      if(!previous.completed)this.change(e,'draining','이전 로컬 요청을 정리하는 중');
      let timer,onAbort;
      try{await Promise.race([previous.done,new Promise((_,reject)=>{
        timer=setTimeout(()=>reject(new Error('Local cleanup is still pending')),this.cleanupWaitMs);
        onAbort=()=>reject(new Error('Local request cancelled'));
        signal?.addEventListener('abort',onAbort,{once:true});if(signal?.aborted)onAbort();
      })]);}finally{clearTimeout(timer);signal?.removeEventListener('abort',onAbort);}
    }
    if(signal?.aborted||e.closed)throw new Error('Local request cancelled');
    if(e.cleanupFailed && !e.bridge?.status.cleanupComplete)throw new Error('Local resource cleanup is not confirmed');
  }
  async ensure(e,signal) {
    if(e.bridge?.status.state==='ready') {
      const sample=await e.resource.sample();
      if(!sample.busy&&e.resource.alive())return e.bridge.status.address;
    }
    // Await cleanup before taking a new advisory lock on the same host.
    if(e.bridge){await e.bridge.stop();if(!e.bridge.status.cleanupComplete)throw new Error('Local resource cleanup is not confirmed');}
    const deadline=Date.now()+this.waitMs;
    while(!signal.aborted&&!e.closed){
      const host=this.hostFactory({host:e.definition.host,profile:e.definition.profile,pool:this.pool,stateDirectory:this.stateDirectory});
      let resource;
      try{
        resource=await this.models.borrow(e.group,()=>{this.change(e,'starting');return host.start({signal});});
        e.resource=resource;
        e.bridge=this.bridgeFactory({upstream:resource.upstream,token:e.token,model:e.definition.profile.model,reasoning:e.definition.profile.reasoning??'default',usageProtocol:e.definition.profile.usageProtocol,usageProfile:e.definition.profile.runtime==='llama-cpp-b10982'?(e.definition.profile.backend==='codex'?LLAMA_RESPONSES_PROFILE:LLAMA_MESSAGES_PROFILE):e.definition.profile.backend==='codex'?RESPONSES_INCLUSIVE_PROFILE:INCLUSIVE_INPUT_PROFILE,...LOCAL_HOST_MEMORY_BUDGET,
          contextWindow:e.definition.profile.contextWindow,maxOutputTokens:e.definition.profile.maxOutputTokens,
          onResponseCompleted:()=>{if(e.active)e.active.completed=true;},
          readResources:async()=>{if(!resource.alive())throw new Error('SSH tunnel disconnected');const sample=await resource.sample();e.resources=sample;return sample;},
          audit:()=>{},release:async reason=>{await resource.release(reason);if(e.state!=='error')this.change(e,'idle');},idleMs:this.idleMs,sampleTimeoutMs:LOCAL_PROVIDER_SAMPLE_TIMEOUT_MS,sampleMaxAgeMs:15000,pollMs:3000});
        const address=await e.bridge.start();
        if(signal.aborted||e.closed){await e.bridge.stop();throw new Error('Local startup cancelled');}
        e.cleanupFailed=false;this.change(e,'ready');return address;
      }catch(error){
        if(resource)await resource.release('Local startup failed');
        if(!(error instanceof LocalHostBusyError)||Date.now()>=deadline)throw error;
        this.change(e,'waiting','ComfyUI 또는 다른 로컬 작업 종료를 기다리는 중');
        await delay(this.retryMs,undefined,{signal});
      }
    }
    throw new Error('Local request cancelled');
  }
  async handle(e,req,res) {
    const reply=(code,message)=>{req.resume();if(res.headersSent){res.destroy();return;}if(!res.destroyed){res.writeHead(code,{'content-type':'application/json'});res.end(JSON.stringify({error:message}));}};
    const token=req.headers['x-api-key']??String(req.headers.authorization??'').replace(/^Bearer /,'');
    const a=Buffer.from(String(token)),b=Buffer.from(e.token);
    if(req.headers.origin||a.length!==b.length||!timingSafeEqual(a,b)){reply(401,'Unauthorized');return;}
    if(e.definition.profile.backend==='codex'&&req.method==='GET'&&/^\/v1\/models(?:\?client_version=[0-9A-Za-z._-]+)?$/.test(req.url)) {
      res.writeHead(200,{'content-type':'application/json','cache-control':'no-store'});
      res.end(JSON.stringify(localCodexModelCatalog(e.definition.profile)));return;
    }
    const supported=e.definition.profile.backend==='codex'?req.url==='/v1/responses':/^\/v1\/messages(?:\/count_tokens)?(?:\?beta=true)?$/.test(req.url);
    if(req.method!=='POST'||!supported){reply(404,'Unsupported route');return;}
    const abort=new AbortController();let active,unlock;
    e.controllers.add(abort);
    res.once('close',()=>{if(!res.writableFinished&&!active?.completed)abort.abort();});
    let settled;
    try{
      if(e.group.paused)throw new Error('로컬 모델이 중지되어 있습니다. 모델 실행 버튼을 눌러주세요.');
      await this.awaitCleanup(e,abort.signal);
      unlock=await this.models.acquire(e.group,abort.signal,()=>{if(!e.active)this.change(e,'waiting','같은 모델의 앞선 요청을 기다리는 중');});
      await this.awaitCleanup(e,abort.signal);
      if(e.group.paused)throw new Error('로컬 모델이 중지되어 있습니다. 모델 실행 버튼을 눌러주세요.');
      active={abort,done:new Promise(resolve=>{settled=resolve;})};
      e.active=active;
      const url=await this.ensure(e,abort.signal);
      this.change(e,'ready');
      await new Promise((resolve,reject)=>{
        const headers={'content-type':'application/json','x-api-key':e.token};
        for(const key of ['anthropic-version','anthropic-beta','content-length'])if(req.headers[key])headers[key]=req.headers[key];
        const upstream=request(`${url}${req.url}`,{method:'POST',headers,signal:abort.signal},response=>{
          if(response.statusCode>=500)this.change(e,'error','로컬 연결 실패 — 다음 요청에서 다시 연결합니다');
          res.writeHead(response.statusCode,{'content-type':response.headers['content-type']??'application/json'});
          pipeline(response,res,error=>error?reject(error):resolve());
        });
        upstream.once('error',reject);req.once('error',reject);req.pipe(upstream);
      });
    }catch(error){
      if(active?.completed)return;
      // A cancelled queue waiter owns no inference or bridge. In particular it
      // must never stop the request currently using this shared model.
      if(!active){if(!e.active&&!e.closed&&abort.signal.aborted)this.change(e,e.bridge?.status.state==='ready'?'ready':'idle');reply(503,error.message);return;}
      const cancelled=abort.signal.aborted;
      abort.abort();
      try{await e.bridge?.stop(cancelled?'Request cancelled':'connection failed');e.cleanupFailed=!!e.bridge&&!e.bridge.status.cleanupComplete;}
      catch{e.cleanupFailed=true;}
      if(cancelled&&!e.cleanupFailed){this.change(e,'idle');reply(503,'Local request cancelled');return;}
      const detail=error instanceof Error?error.message:String(error);
      this.change(e,'error',e.cleanupFailed?'로컬 자원 정리 완료를 확인하지 못했습니다':`로컬 연결 실패: ${detail}`);reply(503,e.error);
    }finally{
      try{await unlock?.();}catch{e.cleanupFailed=true;this.change(e,'error','로컬 자원 정리 완료를 확인하지 못했습니다');}
      if(e.active===active)e.active=null;
      e.controllers.delete(abort);settled?.();
    }
  }
  async close(e,{retireModel=false}={}){
    if(e.closing)return e.closing;
    clearTimeout(e.retire);
    e.closed=true;for(const controller of e.controllers)controller.abort();
    e.closing=(async()=>{await e.ready.catch(()=>{});e.server.closeAllConnections();await new Promise(r=>e.server.close(r));await Promise.all([...e.pending]);await e.bridge?.stop();this.entries.delete(e.key);if((retireModel||!e.group.retain)&&![...this.entries.values()].some(other=>other.group===e.group))await this.models.retire(e.group);this.broadcast({type:'local.changed',profileId:e.definition.profile.id,state:'closed'});})();
    return e.closing;
  }
  async closeIdleProfile(profileID, verifiedAddress) {
    const entries=[...this.entries.values()].filter(e=>e.definition.profile.id===profileID);
    const groups=[...this.models.groups.values()].filter(group=>group.profileIds.has(profileID));
    if(groups.some(group=>group.controlState))throw new LocalHostBusyError('모델 실행 또는 중지 처리 중입니다.');
    if(entries.some(e=>e.users||e.active))throw new LocalHostBusyError('로컬 연결을 사용 중입니다. 작업을 마치고 터미널을 닫아주세요.');
    if(verifiedAddress&&groups.some(group=>group.locked))throw new LocalHostBusyError('같은 모델의 다른 요청이 끝난 뒤 주소를 변경하세요.');
    for(const e of entries){
      if(verifiedAddress)e.resource?.reconnectAddress?.(verifiedAddress);
      await this.close(e,{retireModel:true});
    }
    for(const group of groups){
      if(verifiedAddress)group.record?.resource.reconnectAddress?.(verifiedAddress);
      await this.models.retire(group);
    }
  }
  async shutdown(){
    this.closed=true;
    for(const group of this.models.groups.values())group.controlAbort?.abort();
    await Promise.allSettled([...this.modelControls]);
    await Promise.all([...this.entries.values()].map(e=>this.close(e)));
    await this.models.shutdown();
  }
}
