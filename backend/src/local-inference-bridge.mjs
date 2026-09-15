// Authenticated loopback bridge; never starts or restarts the office backend.
// The caller supplies owned remote resources through an idempotent release hook.
import { createServer, request } from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { pipeline } from 'node:stream';
import { createUsageNormalizer, createUsageSSETransform } from './local-usage-normalizer.mjs';
import { normalizeResponsesUsage, createResponsesUsageSSETransform, RESPONSES_INCLUSIVE_PROFILE, LLAMA_RESPONSES_PROFILE, verifiedResponsesProfile } from './local-responses-usage.mjs';
import { adaptLocalResponsesTools } from './local-responses-tools.mjs';

const error=(status,message)=>Object.assign(new Error(message),{status});
export function normalizeLocalResponsesRequest(value,reasoning='default',usageProfile=RESPONSES_INCLUSIVE_PROFILE) {
  const direct=usageProfile===LLAMA_RESPONSES_PROFILE;
  if(!(direct?['default','on','off','low','medium','xhigh']:['default','on','off']).includes(reasoning))throw error(400,'Local Responses reasoning level is not verified');
  if(value.max_output_tokens!==undefined&&(!Number.isSafeInteger(value.max_output_tokens)||value.max_output_tokens<1||value.max_output_tokens>4096))throw error(400,'Unverified output budget');
  // The LM Studio default must not silently exceed the measured output budget.
  // Codex accepts a `default` effort override, LM Studio's Responses enum does
  // not. A local 'default' selection means no upstream reasoning override; also
  // prevent a cloud effort inherited from shared Codex config from leaking in.
  const {reasoning:ignoredReasoning,reasoning_effort,service_tier,...request}=value;
  // Codex resume appends developer/environment updates between earlier turns.
  // Qwen's Jinja template only accepts system text at the beginning. Move only
  // actual system/developer message objects (never text inside user/tool data)
  // to the instructions field, retaining their role labels and update order.
  if(Array.isArray(request.input)){
    const lifted=request.input.filter(i=>i?.type==='message'&&['system','developer'].includes(i.role));
    if(lifted.length){
      const textOf=i=>{
        if(typeof i.content==='string')return i.content;
        if(!Array.isArray(i.content)||i.content.some(b=>!['input_text','text'].includes(b?.type)||typeof b.text!=='string'))throw error(400,'Unsupported local instruction content');
        return i.content.map(b=>b.text).join('\n');
      };
      if(request.instructions!==undefined&&request.instructions!==null&&typeof request.instructions!=='string')throw error(400,'Unsupported local instructions');
      request.instructions=[request.instructions??'',...lifted.map((i,n)=>`${i.role.toUpperCase()} INSTRUCTIONS (update ${n+1}; later updates take precedence within the same role):\n${textOf(i)}`)].filter(Boolean).join('\n\n');
      request.input=request.input.filter(i=>!lifted.includes(i));
    }
  }
  let effort=reasoning==='off'?'none':undefined;
  if(direct){
    effort=value.reasoning?.effort??reasoning;
    if(['default','on'].includes(effort))effort='xhigh';
    if(effort==='off')effort='none';
    if(!['none','low','medium','xhigh'].includes(effort))throw error(400,'Unsupported Qwen reasoning effort');
  }
  return {...request,...(effort?{reasoning:{effort}}:{}),max_output_tokens:value.max_output_tokens??4096};
}
export function applyLocalReasoning(value,reasoning='default') {
  if(!['default','on','off'].includes(reasoning))throw new TypeError('Unsupported local reasoning option');
  if(reasoning==='default')return value;
  const {output_config,reasoning_effort,chat_template_kwargs,...rest}=value;
  // Measured LM Studio Anthropic adapter: effort levels do not alter Qwen's
  // prompt, but thinking enabled/disabled does. Never advertise ignored levels.
  return {...rest,...(output_config?{output_config:Object.fromEntries(Object.entries(output_config).filter(([k])=>k!=='effort'))}:{}),
    thinking:reasoning==='on'?{type:'enabled',budget_tokens:1024}:{type:'disabled'}};
}
// Claude ToolSearch persists tool_reference blocks in tool results. LM Studio's
// measured Anthropic dialect accepts text there instead. Keep the tool schema,
// result ID and all other blocks intact; only adapt the reference on the wire.
export function normalizeLocalMessageRequest(value) {
  if(!Array.isArray(value.messages))return value;
  const names=new Set((Array.isArray(value.tools)?value.tools:[]).map(tool=>tool?.name));
  let changed=false;
  const messages=value.messages.map(message=>{
    if(!Array.isArray(message?.content))return message;
    let messageChanged=false;
    const content=message.content.map(block=>{
      if(block?.type!=='tool_result'||!Array.isArray(block.content))return block;
      let blockChanged=false;
      const result=block.content.map(item=>{
        if(item?.type!=='tool_reference')return item;
        if(typeof item.tool_name!=='string'||!names.has(item.tool_name))throw error(400,'Tool reference has no matching tool definition');
        blockChanged=true;
        return {type:'text',text:`Available tool: ${item.tool_name}. Its definition is included in the tools list.`};
      });
      if(!blockChanged)return block;
      changed=true;messageChanged=true;return {...block,content:result};
    });
    return messageChanged?{...message,content}:message;
  });
  return changed?{...value,messages}:value;
}
function safeEqual(a,b){const x=Buffer.from(a),y=Buffer.from(b);return x.length===y.length&&timingSafeEqual(x,y);}
function bounded(promise,ms){let timer;return Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(error(503,'Resource sampler timed out')),ms);})]).finally(()=>clearTimeout(timer));}
function readBody(req,limit,signal){return new Promise((resolve,reject)=>{
  let size=0;const chunks=[];
  const data=chunk=>{size+=chunk.length;if(size>limit){req.removeListener('data',data);req.resume();reject(error(413,'Body too large'));}else chunks.push(chunk);};
  req.on('data',data);req.once('end',()=>resolve(Buffer.concat(chunks)));
  req.once('error',reject);req.once('aborted',()=>reject(error(499,'Client disconnected')));
  const abort=()=>{req.removeListener('data',data);req.resume();reject(error(499,'Request cancelled'));};
  signal?.addEventListener('abort',abort,{once:true});
  req.once('end',()=>signal?.removeEventListener('abort',abort));
  if(signal?.aborted)abort();
});}

export class LocalInferenceBridge {
  #server;#starting;#stopping;#monitor;#idle;#release;#token;#upstream;#audit;#sample;#options;
  #active=null;#state='idle';#failure=null;#address=null;#sampling=false;#cleanupComplete=false;
  constructor({upstream,token,model,usageProfile,usageProtocol='anthropic-normalized-v1',reasoning='default',readResources,audit,observeRequest=()=>{},onResponseCompleted=()=>{},release=async()=>{},port=0,
    guardPercent=77,vramGuardPercent=guardPercent,ramGuardPercent=guardPercent,sampleMaxAgeMs=5000,sampleTimeoutMs=3000,pollMs=1000,idleMs=180000,
    requestTimeoutMs=240000,maxBodyBytes=8*1024*1024}) {
    const url=new URL(upstream);
    if(url.protocol!=='http:'||!['127.0.0.1','[::1]'].includes(url.hostname)||!url.port||url.username||url.password||url.pathname!=='/'||url.search||url.hash)throw new TypeError('Upstream must be a loopback URL');
    if(typeof token!=='string'||Buffer.byteLength(token)<24||/[\r\n\0]/.test(token))throw new TypeError('Dedicated bridge token of at least 24 bytes required');
    if(typeof model!=='string'||!model||/[\r\n\0]/.test(model))throw new TypeError('Pinned local model is required');
    if(typeof readResources!=='function'||typeof audit!=='function'||typeof release!=='function')throw new TypeError('Sampler, synchronous audit and release hooks required');
    if(!Number.isInteger(port)||port<0||port>65535||
      ![guardPercent,ramGuardPercent].every(n=>Number.isFinite(n)&&n>0&&n<=95)||
      !Number.isFinite(vramGuardPercent)||vramGuardPercent<=0||vramGuardPercent>98)throw new TypeError('Invalid port or memory guard');
    for(const n of [sampleMaxAgeMs,sampleTimeoutMs,pollMs,idleMs,requestTimeoutMs,maxBodyBytes])if(!Number.isSafeInteger(n)||n<=0)throw new TypeError('Invalid limits');
    if(!['anthropic-normalized-v1','openai-responses-v1'].includes(usageProtocol))throw new TypeError('Unsupported local usage protocol');
    if(usageProtocol==='openai-responses-v1') {
      if(!verifiedResponsesProfile(usageProfile))throw new TypeError('Explicit verified Responses usage profile required');
    } else createUsageNormalizer({profile:usageProfile});
    if(usageProtocol==='openai-responses-v1')normalizeLocalResponsesRequest({},reasoning,usageProfile);else applyLocalReasoning({},reasoning);
    this.#upstream=url;this.#token=token;this.#sample=readResources;this.#audit=audit;this.#release=release;
    this.#options={port,model,reasoning,usageProtocol,observeRequest,onResponseCompleted,guardPercent,vramGuardPercent,ramGuardPercent,sampleMaxAgeMs,sampleTimeoutMs,pollMs,idleMs,requestTimeoutMs,maxBodyBytes,usageProfile};
  }
  get status(){return {state:this.#state,active:this.#active!==null,address:this.#address,failure:this.#failure,cleanupComplete:this.#cleanupComplete};}
  async #resources(){
    const s=await bounded(Promise.resolve().then(()=>this.#sample()),this.#options.sampleTimeoutMs);
    if(!s||![s.vramPct,s.ramPct,s.sampledAt].every(Number.isFinite)||s.vramPct<0||s.vramPct>100||s.ramPct<0||s.ramPct>100||Date.now()-s.sampledAt>this.#options.sampleMaxAgeMs||s.sampledAt>Date.now()+1000)throw error(503,'Resource sample unavailable or stale');
    if(s.vramPct>=this.#options.vramGuardPercent||s.ramPct>=this.#options.ramGuardPercent)throw error(503,'Memory budget guard');
    return s;
  }
  start(){if(['starting','ready'].includes(this.#state))return this.#starting;if(this.#state!=='idle')return Promise.reject(error(409,'Bridge cannot be restarted'));this.#state='starting';this.#starting=this.#start();return this.#starting;}
  async #start(){try{
    const sample=await this.#resources();if(sample.busy===true)throw error(503,'GPU reserved by another workload');
    if(this.#state!=='starting')throw error(503,'Startup cancelled');
    this.#server=createServer((req,res)=>{this.#handle(req,res).catch(()=>res.destroy());});
    this.#server.requestTimeout=this.#options.requestTimeoutMs;this.#server.headersTimeout=Math.min(10000,this.#options.requestTimeoutMs);
    await new Promise((resolve,reject)=>{this.#server.once('error',reject);this.#server.listen(this.#options.port,'127.0.0.1',resolve);});
    if(this.#state!=='starting'){this.#server.closeAllConnections();await new Promise(r=>this.#server.close(r));throw error(503,'Startup cancelled');}
    this.#address=`http://127.0.0.1:${this.#server.address().port}`;this.#state='ready';
    this.#monitor=setInterval(()=>{if(this.#sampling)return;this.#sampling=true;this.#resources().catch(e=>this.stop(e.message)).finally(()=>{this.#sampling=false;});},this.#options.pollMs);this.#monitor.unref();this.#armIdle();return this.#address;
  }catch(e){await this.stop(e.message);throw e;}}
  #armIdle(){clearTimeout(this.#idle);if(this.#state==='ready'&&!this.#active){this.#idle=setTimeout(()=>this.stop('idle'),this.#options.idleMs);this.#idle.unref();}}
  stop(reason='manual'){
    if(this.#stopping)return this.#stopping;this.#state='stopping';this.#failure=['manual','idle'].includes(reason)?null:reason;
    clearInterval(this.#monitor);clearTimeout(this.#idle);this.#active?.abort.abort();
    this.#stopping=(async()=>{if(this.#server){this.#server.closeAllConnections();await new Promise(r=>this.#server.close(()=>r()));}try{await bounded(Promise.resolve().then(()=>this.#release()).then(()=>{this.#cleanupComplete=true;}),10000);}catch{this.#failure='Owned resource cleanup failed';}this.#address=null;this.#state=this.#failure?'fault':'stopped';})();return this.#stopping;
  }
  async #handle(req,res){
    const reply=(status,message)=>{if(res.destroyed||res.writableEnded)return;if(res.headersSent){res.destroy();return;}res.writeHead(status,{'content-type':'application/json','cache-control':'no-store'});res.end(JSON.stringify({error:message}));};
    const auth=req.headers.authorization;
    if(req.headers.origin||!((typeof auth==='string'&&safeEqual(auth,`Bearer ${this.#token}`))||(typeof req.headers['x-api-key']==='string'&&safeEqual(req.headers['x-api-key'],this.#token)))){req.resume();reply(401,'Unauthorized');return;}
    if(req.method==='GET'&&req.url==='/health'){res.writeHead(200,{'content-type':'application/json','cache-control':'no-store'});res.end(JSON.stringify({state:this.#state,active:!!this.#active,usageProtocol:this.#options.usageProtocol,guardPercent:this.#options.guardPercent,vramGuardPercent:this.#options.vramGuardPercent,ramGuardPercent:this.#options.ramGuardPercent}));return;}
    // Installed Claude SDK uses ?beta=true. Allow only that known query,
    // not arbitrary query routing, redirects or caller-selected upstream URLs.
    const route=req.url.replace(/\?beta=true$/,'');
    const responses=this.#options.usageProtocol==='openai-responses-v1';
    if(req.method!=='POST'||!(responses?req.url==='/v1/responses':['/v1/messages','/v1/messages/count_tokens'].includes(route))){req.resume();reply(404,'Unsupported route');return;}
    if(this.#state!=='ready'){req.resume();reply(503,'Bridge unavailable');return;}
    if(this.#active){req.resume();reply(409,'Local inference is busy');return;}
    const active={abort:new AbortController()};this.#active=active;clearTimeout(this.#idle);
    const timer=setTimeout(()=>{reply(504,'Local inference timed out');active.abort.abort();},this.#options.requestTimeoutMs);
    res.once('close',()=>{if(!res.writableFinished&&!active.completed)active.abort.abort();});
    try{
      const sample=await this.#resources();if(sample.busy===true)throw error(503,'GPU reserved by another workload');
      const body=await readBody(req,this.#options.maxBodyBytes,active.abort.signal);
      let parsed;try{parsed=JSON.parse(body);}catch{throw error(400,'Invalid JSON');}
      if(!parsed||typeof parsed!=='object'||Array.isArray(parsed))throw error(400,'Invalid JSON object');
      if(parsed.model!==this.#options.model)throw error(400,'Model does not match local profile');
      if(parsed.max_tokens!==undefined&&(!Number.isSafeInteger(parsed.max_tokens)||parsed.max_tokens<=0||parsed.max_tokens>4096))throw error(400,'Unverified output budget');
      if(active.abort.signal.aborted)throw error(503,'Request cancelled');
      const adapter=responses?adaptLocalResponsesTools(normalizeLocalResponsesRequest(parsed,this.#options.reasoning,this.#options.usageProfile)):null;
      const normalized=adapter?adapter.request:applyLocalReasoning(normalizeLocalMessageRequest(parsed),this.#options.reasoning);
      this.#options.observeRequest(normalized);
      const outgoing=normalized===parsed?body:Buffer.from(JSON.stringify(normalized));
      const headers={'content-type':'application/json','accept-encoding':'identity','content-length':outgoing.length};
      for(const key of ['anthropic-version','anthropic-beta','accept'])if(typeof req.headers[key]==='string')headers[key]=req.headers[key];
      await new Promise((resolve,reject)=>{
        const up=request(new URL(req.url,this.#upstream),{method:'POST',headers,signal:active.abort.signal},response=>{
          const responseHeaders={'content-type':response.headers['content-type']||'application/json','cache-control':'no-store'};
          if(response.statusCode!==200||route.endsWith('/count_tokens')){res.writeHead(response.statusCode,responseHeaders);pipeline(response,res,e=>e?reject(e):resolve());return;}
          if(response.headers['content-encoding']&&response.headers['content-encoding']!=='identity'){response.destroy();reject(error(502,'Unexpected upstream encoding'));return;}
          const options={profile:this.#options.usageProfile,onCompleted:()=>{active.completed=true;this.#options.onResponseCompleted();},onUsage:record=>{const result=this.#audit(record);if(result?.then){Promise.resolve(result).catch(()=>{});throw new Error('Usage audit must be synchronous');}}};
          if(String(responseHeaders['content-type']).includes('text/event-stream')){res.writeHead(200,responseHeaders);const transforms=responses?[adapter.stream(),createResponsesUsageSSETransform(options)]:[createUsageSSETransform(options)];pipeline(response,...transforms,res,e=>e?reject(e):resolve());}
          else{readBody(response,this.#options.maxBodyBytes).then(raw=>{let result=JSON.parse(raw);if(responses){const record=normalizeResponsesUsage(result,options);if(record)options.onUsage(record);result=adapter.restoreResponse(result);}else result=createUsageNormalizer(options)(result);res.writeHead(200,responseHeaders);res.end(JSON.stringify(result));resolve();}).catch(reject);}
        });up.once('error',reject);up.end(outgoing);
      });
    }catch(e){if(!active.completed){reply(e.status||502,e.status?e.message:'Local upstream failed');if(!e.status&&!active.abort.signal.aborted)await this.stop('Local upstream or usage failure');}}
    finally{
      clearTimeout(timer);
      // Disconnecting HTTP does not prove that a local runtime has stopped GPU
      // generation. Release the owned model on cancellation/timeouts instead of
      // retaining it through the ordinary idle period.
      if(active.abort.signal.aborted)await this.stop('Request cancelled');
      if(this.#active===active)this.#active=null;
      this.#armIdle();
    }
  }
}
