import { createServer, request } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { LocalInferenceBridge } from './local-inference-bridge.mjs';
import { INCLUSIVE_INPUT_PROFILE } from './local-usage-normalizer.mjs';
import { WindowsLMStudioHost, LocalHostBusyError, validateHost, LOCAL_HOST_MEMORY_BUDGET } from './local-provider-host.mjs';
import { normalizeLocalAgentProfile, createLocalAgentLaunch } from './local-agent-profile.mjs';

export function normalizeLocalDefinition(value) {
  return {profile:normalizeLocalAgentProfile(value?.profile),host:validateHost(value?.host)};
}
export async function resolveLocalCharacter(client,character) {
  const id=character.config?.localProfileId;
  if(!id)return character;
  const result=await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true',[id]);
  if(!result.rows[0])throw new Error('Local profile is disabled or missing');
  const definition=normalizeLocalDefinition(result.rows[0].definition);
  if(character.backend!=='claude'||character.model!==definition.profile.model||character.effort!=='default'||character.fastMode)throw new Error('Local employee settings do not match the profile');
  character.localProfile=definition;
  return character;
}
export async function snapshotLocalTurn(client,turnID,character) {
  if(character.localProfile)await client.query("UPDATE turns SET provider_kind='local',provider_snapshot=$2::jsonb WHERE id=$1",[turnID,JSON.stringify(character.localProfile)]);
}
export function localResumeCompatible(previous,next) {
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
  constructor({pool,stateDirectory,broadcast=()=>{},hostFactory=o=>new WindowsLMStudioHost(o),bridgeFactory=o=>new LocalInferenceBridge(o),waitMs=300000,retryMs=2000,idleMs=180000,cleanupWaitMs=15000}) {
    Object.assign(this,{pool,stateDirectory,broadcast,hostFactory,bridgeFactory,waitMs,retryMs,idleMs,cleanupWaitMs});
    this.entries=new Map();this.closed=false;
  }
  status(){return [...this.entries.values()].map(e=>({id:e.definition.profile.id,state:e.state,error:e.error??null,users:e.users,resources:e.resources??null}));}
  change(e,state,error=null){e.state=state;e.error=error;this.broadcast({type:'local.changed',profileId:e.definition.profile.id,state,error});}
  async launch({character,...options}) {
    if(this.closed)throw new Error('Local service is shutting down');
    const definition=normalizeLocalDefinition(character.localProfile);
    const id=definition.profile.id;
    let e=this.entries.get(id);
    if(e&&!isDeepStrictEqual(e.definition,definition))throw new Error('Close existing local sessions before changing their profile');
    if(!e){
      e={definition,users:0,state:'idle',token:randomBytes(32).toString('hex'),bridge:null,closed:false,active:null};
      this.entries.set(id,e);
      e.pending=new Set();
      e.server=createServer((req,res)=>{const task=this.handle(e,req,res).catch(()=>res.destroy());e.pending.add(task);task.finally(()=>e.pending.delete(task));});
      e.server.requestTimeout=360000;e.server.headersTimeout=10000;
      e.ready=new Promise((resolve,reject)=>{e.server.once('error',reject);e.server.listen(0,'127.0.0.1',resolve);});
    }
    await e.ready;
    clearTimeout(e.retire);
    await this.awaitCleanup(e);
    if(e.closed)throw new Error('Local session is closed');
    e.users++;
    let released=false;
    const release=async()=>{if(released)return;released=true;e.users--;if(!e.users){
      if(options.mode==='terminal')await this.close(e);
      else {e.active?.abort.abort();e.retire=setTimeout(()=>this.close(e),this.idleMs);e.retire.unref();}
    }};
    try {
      const profile={...definition.profile,endpoint:`http://127.0.0.1:${e.server.address().port}`};
      return {...createLocalAgentLaunch({...options,character,profile,baseEnvironment:{...(options.baseEnvironment??process.env),[profile.credentialEnv]:e.token}}),release};
    }catch(error){await release();throw error;}
  }
  async awaitCleanup(e,signal) {
    // Only a cancelled request may wait. Genuine concurrent inference remains 409.
    const previous=e.active;
    if(previous?.abort.signal.aborted){
      this.change(e,'draining','이전 로컬 요청을 정리하는 중');
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
      try{
        this.change(e,'starting');
        const resource=await host.start({signal});
        e.resource=resource;
        e.bridge=this.bridgeFactory({upstream:resource.upstream,token:e.token,model:e.definition.profile.model,usageProfile:INCLUSIVE_INPUT_PROFILE,...LOCAL_HOST_MEMORY_BUDGET,
          readResources:async()=>{if(!resource.alive())throw new Error('SSH tunnel disconnected');const sample=await resource.sample();e.resources=sample;return sample;},
          audit:()=>{},release:async()=>{await resource.release();if(e.state!=='error')this.change(e,'idle');},idleMs:this.idleMs,sampleTimeoutMs:12000,sampleMaxAgeMs:15000,pollMs:3000});
        const address=await e.bridge.start();
        if(signal.aborted||e.closed){await e.bridge.stop();throw new Error('Local startup cancelled');}
        e.cleanupFailed=false;this.change(e,'ready');return address;
      }catch(error){
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
    if(req.method!=='POST'||!/^\/v1\/messages(?:\/count_tokens)?(?:\?beta=true)?$/.test(req.url)){reply(404,'Unsupported route');return;}
    const abort=new AbortController();
    res.once('close',()=>{if(!res.writableFinished)abort.abort();});
    try{await this.awaitCleanup(e,abort.signal);}catch(error){reply(503,error.message);return;}
    if(e.active||e.closed){reply(409,'Local inference is busy');return;}
    let settled;
    const active={abort,done:new Promise(resolve=>{settled=resolve;})};
    e.active=active;
    const timer=setTimeout(()=>abort.abort(),this.waitMs+240000);
    try{
      const url=await this.ensure(e,abort.signal);
      await new Promise((resolve,reject)=>{
        const headers={'content-type':'application/json','x-api-key':e.token};
        for(const key of ['anthropic-version','anthropic-beta','content-length'])if(req.headers[key])headers[key]=req.headers[key];
        const upstream=request(`${url}${req.url}`,{method:'POST',headers,signal:abort.signal},response=>{
          if(response.statusCode>=500)this.change(e,'error','로컬 연결 실패 — 다음 요청에서 다시 연결합니다');
          res.writeHead(response.statusCode,{'content-type':response.headers['content-type']??'application/json'});
          response.pipe(res);response.once('error',reject);response.once('aborted',()=>reject(new Error('Local response interrupted')));response.once('end',resolve);
        });
        upstream.once('error',reject);req.once('error',reject);req.pipe(upstream);
      });
    }catch(error){
      abort.abort();
      try{await e.bridge?.stop('connection failed');e.cleanupFailed=!!e.bridge&&!e.bridge.status.cleanupComplete;}
      catch{e.cleanupFailed=true;}
      this.change(e,'error',e.cleanupFailed?'로컬 자원 정리 완료를 확인하지 못했습니다':'로컬 연결 실패 — 다음 요청에서 다시 연결합니다');reply(503,e.error);
    }finally{clearTimeout(timer);if(e.active===active)e.active=null;settled();}
  }
  async close(e){
    if(e.closing)return e.closing;
    clearTimeout(e.retire);
    e.closed=true;e.active?.abort.abort();
    e.closing=(async()=>{await e.ready.catch(()=>{});e.server.closeAllConnections();await new Promise(r=>e.server.close(r));await Promise.all([...e.pending]);await e.bridge?.stop();this.entries.delete(e.definition.profile.id);this.broadcast({type:'local.changed',profileId:e.definition.profile.id,state:'closed'});})();
    return e.closing;
  }
  async shutdown(){this.closed=true;await Promise.all([...this.entries.values()].map(e=>this.close(e)));}
}
