// Opt-in integration probe. Uses the same managed bridge/host and resource lease
// as production, but never starts the OFFICESTRA backend or changes employees.
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, readFile, realpath } from 'node:fs/promises';
import { writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pool } from '../backend/src/db.mjs';
import { LocalProviderService } from '../backend/src/local-provider-service.mjs';
import { LocalInferenceBridge } from '../backend/src/local-inference-bridge.mjs';
import { WindowsLMStudioHost, localHostLoadConfig } from '../backend/src/local-provider-host.mjs';

const cwd=await mkdtemp(join(tmpdir(),'officestra-codex-probe-'));
const terminal=process.env.OFFICESTRA_PROBE_TERMINAL==='1';
console.log(JSON.stringify({probeDirectory:cwd}));
const client=await pool.connect();let definition;
try {
  await client.query('BEGIN READ ONLY');
  definition=(await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1',[process.env.OFFICESTRA_PROBE_PROFILE??'local-4090-qwen38'])).rows[0].definition;
} finally {await client.query('ROLLBACK');client.release();}
const catalog=await fetch('http://127.0.0.1:4317/api/local-profiles').then(r=>r.json());
definition.host.address=catalog.assignments.find(a=>a.characterId==='right-woman').address;
definition.profile={...definition.profile,id:'local-4090-qwen38-codex',backend:'codex',usageProtocol:'openai-responses-v1',reasoning:'default'};
if(process.env.OFFICESTRA_PROBE_LLAMA==='1')definition.profile={...definition.profile,id:'local-4090-qwen38-llamacpp',runtime:'llama-cpp-b10982',contextWindow:65536,kvCacheQuantization:'q8_0',reasoning:process.env.OFFICESTRA_PROBE_EFFORT??'medium'};
const smallContext=process.env.OFFICESTRA_PROBE_32K==='1';
const smallCache=process.env.OFFICESTRA_PROBE_KV4==='1';
if(smallContext||smallCache)throw new Error('This validation requires 64K KV8; no reduced-context/cache fallback');
if(smallContext)definition.profile.contextWindow=32768;
const records=[],samples=[];
const captureOnly=process.env.OFFICESTRA_PROBE_CAPTURE_ONLY==='1';
// Direct-server integration is opt-in and can only target a local SSH tunnel.
// Its parent probe owns the GPU lease, server PID and final cleanup.
const directURL=process.env.OFFICESTRA_PROBE_DIRECT_URL;
if(directURL&&!/^http:\/\/127\.0\.0\.1:[0-9]+$/.test(directURL))throw Error('Direct probe requires a loopback tunnel');
const directEffort=process.env.OFFICESTRA_PROBE_DIRECT_EFFORT??'medium';
if(directURL&&!['low','medium','xhigh'].includes(directEffort))throw Error('Invalid direct probe effort');
const service=new LocalProviderService({pool,stateDirectory:join(process.cwd(),'.office-local-resources'),
  ...(directURL?{hostFactory:o=>{const sampler=new WindowsLMStudioHost(o);return {start:async()=>({upstream:directURL,sample:()=>sampler.sample(),alive:()=>true,release:async()=>{}})};}}:{}),
  ...(captureOnly?{hostFactory:()=>({start:async()=>({upstream:'http://127.0.0.1:9',sample:async()=>({vramPct:0,ramPct:0,sampledAt:Date.now(),busy:false}),alive:()=>true,release:async()=>{}})})}:{}),
  ...(!captureOnly&&(smallContext||smallCache||process.env.OFFICESTRA_PROBE_BATCH32==='1')?{hostFactory:o=>new WindowsLMStudioHost({...o,loadConfig:p=>({...localHostLoadConfig(p),llamaKCacheQuantizationType:smallCache?'q4_0':'q8_0',llamaVCacheQuantizationType:smallCache?'q4_0':'q8_0',...(process.env.OFFICESTRA_PROBE_BATCH32==='1'?{evalBatchSize:32}:{})})})}:{}),
  broadcast:e=>{if(e.state)console.log(JSON.stringify({state:e.state}));},
  bridgeFactory:options=>{const bridge=new LocalInferenceBridge({...options,readResources:async()=>{const s=await options.readResources();samples.push(s);return s;},requestTimeoutMs:45000,audit:r=>records.push(r),observeRequest:r=>{
    // Probe-only budget: exercise one short tool call, never a full 4096-token
    // reasoning run. The production request limit remains unchanged.
    r.max_output_tokens=Math.min(r.max_output_tokens,512);
    if(directURL)r.reasoning={effort:directEffort};
    if(process.env.OFFICESTRA_PROBE_TOOLS==='search')r.tools=r.tools.filter(t=>t.name==='officestra_client_tool_search'||(t.name?.startsWith('office_ns_')&&!t.description?.startsWith('mcp__cua_repl.')));
    if(process.env.OFFICESTRA_PROBE_TOOLS==='cua')r.tools=r.tools.filter(t=>t.description?.startsWith('mcp__cua_repl.'));
    writeFileSync(join(cwd,'request.json'),JSON.stringify(r));
    console.log(JSON.stringify({toolCount:r.tools?.length,hasToolSearch:r.tools?.some(t=>t.name==='officestra_client_tool_search'),requestCharacters:JSON.stringify(r).length,inputItems:r.input?.length}));
    if(captureOnly)throw Object.assign(new Error('Capture complete; no model request sent'),{status:400});
    if(process.env.OFFICESTRA_PROBE_CANCEL==='1'&&!cancelTimer)cancelTimer=setTimeout(stop,1500);
  }});const originalStop=bridge.stop.bind(bridge);bridge.stop=reason=>{if(directURL)console.log(JSON.stringify({bridgeStop:reason??'manual'}));return originalStop(reason);};return bridge;}});
const character={id:'isolated-local-codex-probe',backend:'codex',model:definition.profile.model,effort:'default',fastMode:false,autoCompactPercent:65,permission:'workspace-write',identityPrompt:'You are an isolated local Codex compatibility probe. Only do the requested harmless test. Never manage LM Studio, SSH, GPU resources or other agents.',localProfile:definition};
const prompt=process.argv[2]??'Use the shell tool to run printf CODEX_LOCAL_TOOL_OK, then reply with that exact output. Do not read any other files or run other tools.';
let spec,child,stopped=false,exited=false,timer,cancelTimer,killTimer,stdout='',stderr='';
const killProbe=(signal='SIGTERM')=>{if(child?.pid&&!exited){try{process.kill(-child.pid,signal);}catch(e){if(e.code!=='ESRCH')throw e;}}};
const stop=()=>{
  if(stopped)return;stopped=true;killProbe();
  killTimer=setTimeout(()=>killProbe('SIGKILL'),10000);
  void service.shutdown().catch(e=>console.error('Cleanup failed:',e.message));
};
process.once('SIGTERM',stop);process.once('SIGINT',stop);
try {
  spec=await service.launch({character,mode:terminal?'terminal':'gui',workdir:cwd,prompt,previousSessionID:process.env.OFFICESTRA_PROBE_RESUME??null});
  if(process.env.OFFICESTRA_PROBE_RESUME&&!terminal)spec.args.splice(spec.args.length-1,0,'--skip-git-repo-check');
  console.log(JSON.stringify({executable:spec.executable,backend:spec.backend}));
  if(stopped)throw new Error('Probe cancelled before spawn');
  if(terminal){
    spec.args.push('-c',`projects.${JSON.stringify(await realpath(cwd))}.trust_level="trusted"`);
    const notify=JSON.stringify([process.execPath,'-e',`require('fs').writeFileSync(${JSON.stringify(join(cwd,'notify.json'))},process.argv[1])`]);
    spec.args.push('-c',`notify=${notify}`,'--no-alt-screen',prompt);
  }
  child=terminal?spawn('python3',[join(process.cwd(),'scripts/local-codex-pty-probe.py'),join(cwd,'notify.json'),spec.executable,...spec.args],{cwd,env:spec.env,detached:true,stdio:['ignore','pipe','pipe']}):spawn(spec.executable,spec.args,{cwd,env:spec.env,detached:true,stdio:['ignore','pipe','pipe']});
  child.once('exit',()=>{exited=true;clearTimeout(killTimer);});
  child.stdout.on('data',d=>{stdout+=d;if(terminal)writeFileSync(join(cwd,'terminal-live.log'),stdout);if(stdout.length>8*1024*1024)stop();});
  child.stderr.on('data',d=>{stderr=(stderr+d).slice(-12000);});
  timer=setTimeout(stop,120000);
  const code=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',resolve);});clearTimeout(timer);
  await writeFile(join(cwd,'events.jsonl'),stdout);await writeFile(join(cwd,'stderr.log'),stderr);
  const events=stdout.trim().split('\n').flatMap(line=>{try{return [JSON.parse(line)];}catch{return [];}});
  console.log(JSON.stringify({exitCode:code,resourcePeak:{vramPct:Math.max(...samples.map(s=>s.vramPct)),ramPct:Math.max(...samples.map(s=>s.ramPct))},events:events.map(e=>({type:e.type,thread_id:e.thread_id,message:e.message,error:e.error,item:e.item?{type:e.item.type,tool:e.item.tool,server:e.item.server,status:e.item.status,command:e.item.command,text:e.item.text?.slice(-1200),error:e.item.error}:undefined,usage:e.usage})),audit:records.map(r=>({input:r.totalInputTokens,output:r.raw.output_tokens})),stderr:stderr.slice(-2200)}));
  if(terminal){
    const notification=JSON.parse(await readFile(join(cwd,'notify.json'),'utf8'));
    console.log(JSON.stringify({terminal:true,notification}));
    if(code!==0||notification.type!=='agent-turn-complete')process.exitCode=1;
  }else if(code!==0||!events.some(e=>e.type==='turn.completed'))process.exitCode=1;
} finally {
  clearTimeout(timer);clearTimeout(cancelTimer);killProbe();
  await writeFile(join(cwd,'events.jsonl'),stdout);await writeFile(join(cwd,'stderr.log'),stderr);
  try{await spec?.release();await service.shutdown();}finally{await pool.end();}
  clearTimeout(killTimer);
  process.removeListener('SIGTERM',stop);process.removeListener('SIGINT',stop);
}
