import {spawn} from 'node:child_process';
import {createServer} from 'node:net';
import {mkdir,readFile,writeFile,unlink} from 'node:fs/promises';
import {WindowsLMStudioHost,LocalHostBusyError,localHostMemoryExceeded,validateHost} from './local-provider-host.mjs';
import { isMeroMero, MEROMERO_ARTIFACT, MEROMERO_HOST_KEY } from './local-model-capabilities.mjs';

// Pinned, separately installed runtime. Never substitutes for LM Studio files.
export const LLAMA_SERVER_PATH='G:\\llm\\officestra-probes\\llama-b10982-cuda124\\bin\\llama-server.exe';
export const LLAMA_SERVER_SHA256='FFAEE576AD271EDE87B92A7D8C3863DC8F331BBAC666EE00099C250E8809E743';
export const LLAMA_MODEL_ID='officestra-qwen38-27b-uncensored-q4km';
export const LLAMA_MODEL_ROOT='G:\\llm\\jonathancoletti\\Qwen3.8-27B-Uncensored-GGUF';
export const LLAMA_MODEL_FILE='Qwen3.8-27B-Uncensored-Q4_K_M.gguf';
export const LLAMA_MODEL_SIZE=16810714528;
export const LLAMA_MODEL_SHA256='4C5E2DB039E9325AC7724C8846C71356A24AD1CDFA28002D73ECB6BE645F9675';
export const LLAMA_MMPROJ_FILE='mmproj-Qwen3.8-27B-Uncensored-F16.gguf';
export const LLAMA_MMPROJ_SIZE=927606912;
export const LLAMA_MMPROJ_SHA256='5AC423F8A29059DC24E51BC6A43E9380DCD57A9347F28B62591E0B3F60B7081C';
export const LLAMA_CLEANUP_REMOTE_TIMEOUT_MS=45000;
export const LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT_MS=60000;
const pause=ms=>new Promise(r=>setTimeout(r,ms));
// HTTP disconnect is only a cancellation request. Confirm the sole generation
// slot is idle twice before retaining the loaded model for another request.
export async function waitForLlamaIdle(base,{timeoutMs=5000,pollMs=100,fetchImpl=fetch,alive=()=>true}={}) {
 const deadline=Date.now()+timeoutMs;let idle=0;
 while(Date.now()<deadline){
  if(!alive())throw Error('SSH tunnel disconnected');
  const response=await fetchImpl(base+'/slots',{signal:AbortSignal.timeout(Math.max(1,Math.min(1000,deadline-Date.now())))});
  if(!response.ok)throw Error('Direct slot state unavailable');
  const slots=await response.json();
  if(!Array.isArray(slots)||slots.length!==1||typeof slots[0].is_processing!=='boolean')throw Error('Direct slot state invalid');
  idle=slots[0].is_processing?0:idle+1;
  if(idle===2)return;
  await pause(pollMs);
 }
 throw Error('Direct generation did not stop');
}
const ps=s=>"'"+String(s).replaceAll("'","''")+"'";
const artifactPath=file=>LLAMA_MODEL_ROOT+'\\'+file;
const artifactCheck=(file,size,sha,label)=>`if(!(Test-Path -LiteralPath ${ps(artifactPath(file))})){throw '${label} is not installed'};$f=Get-Item -LiteralPath ${ps(artifactPath(file))};if($f.Length -ne ${size}){throw '${label} size changed; revalidation required'};if((Get-FileHash -LiteralPath ${ps(artifactPath(file))} -Algorithm SHA256).Hash -ne '${sha}'){throw '${label} changed; revalidation required'}`;
export function llamaArtifactVerificationScript(profile) {
 if(isMeroMero(profile)){
  const a=MEROMERO_ARTIFACT,p=ps(a.root+'\\'+a.file);
  return `if(!(Test-Path -LiteralPath ${p})){throw 'Pinned MeroMero model is not installed'};$f=Get-Item -LiteralPath ${p};if($f.Length -ne ${a.size}){throw 'Pinned model size changed; revalidation required'};if((Get-FileHash -LiteralPath ${p} -Algorithm SHA256).Hash -ne '${a.sha256}'){throw 'Pinned model changed; revalidation required'}`;
 }
 return [artifactCheck(LLAMA_MODEL_FILE,LLAMA_MODEL_SIZE,LLAMA_MODEL_SHA256,'Pinned model'),artifactCheck(LLAMA_MMPROJ_FILE,LLAMA_MMPROJ_SIZE,LLAMA_MMPROJ_SHA256,'Pinned mmproj')].join(';');
}
// Installation/probe tooling retains full hashes. Ordinary startup checks only
// metadata, avoiding a complete read of the 18 GB model before loading it.
export function llamaStartupArtifactScript(profile) {
 const files=isMeroMero(profile)
  ? [[MEROMERO_ARTIFACT.root+'\\'+MEROMERO_ARTIFACT.file,MEROMERO_ARTIFACT.size]]
  : [[artifactPath(LLAMA_MODEL_FILE),LLAMA_MODEL_SIZE],[artifactPath(LLAMA_MMPROJ_FILE),LLAMA_MMPROJ_SIZE]];
 return `if(!(Test-Path -LiteralPath ${ps(LLAMA_SERVER_PATH)})){throw 'llama.cpp runtime is not installed'};`+files.map(([path,size])=>`$f=Get-Item -LiteralPath ${ps(path)} -ErrorAction Stop;if($f.Length -ne ${size}){throw 'Model file size mismatch'}`).join(';');
}
export function llamaServerArguments(profile) {
 if(isMeroMero(profile)){
  if(!['codex','claude'].includes(profile.backend)||![32768,65536].includes(profile.contextWindow)||profile.kvCacheQuantization!=='q8_0')throw Error('Unsupported MeroMero runtime configuration');
  // Text-only 64K/KV8 is measured through 61K input on the 4090. Retain 32K
  // compatibility for historical snapshots; Gemma4 has no native MTP head.
  return ['-m',MEROMERO_ARTIFACT.root+'\\'+MEROMERO_ARTIFACT.file,'-c',String(profile.contextWindow),'-ctk','q8_0','-ctv','q8_0','-ngl','all','-fa','on','-np','1','-b','512','-ub','128','--jinja','--host','127.0.0.1','--port','18181','--alias',profile.model,'--load-mode','none','--fit','off'];
 }
 if(profile.runtime!=='llama-cpp-b10982'||!['codex','claude'].includes(profile.backend)||profile.contextWindow!==65536||profile.kvCacheQuantization!=='q8_0'||profile.model!==LLAMA_MODEL_ID)throw Error('Unsupported direct runtime configuration');
 // Measured with the same 64K/KV8/vision profile: native MTP accelerates
 // generation without lowering effort or substituting another draft model.
 return ['-m',artifactPath(LLAMA_MODEL_FILE),'--mmproj',artifactPath(LLAMA_MMPROJ_FILE),'-c','65536','-ctk','q8_0','-ctv','q8_0','-ngl','all','-fa','on','-np','1','-b','128','-ub','128','--jinja','--host','127.0.0.1','--port','18181','--alias',profile.model,'--load-mode','none','--fit','off','--spec-type','draft-mtp','--spec-draft-n-max','2'];
}
export function llamaServerCommand(profile) {
 return [LLAMA_SERVER_PATH,...llamaServerArguments(profile)].map(s=>'"'+s+'"').join(' ');
}
export function validLlamaOwnership(record) {
 return Number.isSafeInteger(record?.pid)&&record.pid>0&&record.path===LLAMA_SERVER_PATH&&typeof record.created==='string'&&/^\d{4}-\d\d-\d\dT[0-9:.]+Z$/.test(record.created);
}
export function llamaCleanupScript(record) {
 if(!validLlamaOwnership(record))throw Error('Invalid direct server ownership record');
 // Stop-Process may return before llama-server has actually released its
 // memory. Keep the recovery record until the exact owned process disappears,
 // so a backend restart cannot turn a still-running child into an orphan.
 return `$ErrorActionPreference='Stop';$process=Get-Process -Id ${record.pid} -ErrorAction SilentlyContinue;if($process){$identity=Get-CimInstance Win32_Process -Filter "ProcessId=${record.pid}";if($identity){if($identity.ExecutablePath -ne ${ps(record.path)} -or $identity.CreationDate.ToUniversalTime().ToString('o') -ne ${ps(record.created)}){throw 'Owned server identity changed'};Stop-Process -Id ${record.pid} -Force;$deadline=(Get-Date).AddSeconds(15);do{Start-Sleep -Milliseconds 100;$process=Get-Process -Id ${record.pid} -ErrorAction SilentlyContinue}while($process -and (Get-Date) -lt $deadline);if($process){throw 'Owned server did not exit'}}};'released'`;
}
export function llamaOrphanCleanupScript(profile) {
 const command=llamaServerCommand(profile);
 // A cancellation can close SSH after WMI has created the process but before
 // its PID reaches the Mac. Reclaim only exact OFFICESTRA path+command matches;
 // a different process on the same port remains untouched and blocks startup.
 return `$ErrorActionPreference='Stop';$owned=@();$candidates=@(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue);foreach($candidate in $candidates){$p=Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.Id)";if($p -and $p.ExecutablePath -eq ${ps(LLAMA_SERVER_PATH)} -and $p.CommandLine -eq ${ps(command)}){$owned+=$p}};foreach($p in $owned){$pidToStop=[int]$p.ProcessId;Stop-Process -Id $pidToStop -Force;$deadline=(Get-Date).AddSeconds(15);do{Start-Sleep -Milliseconds 100;$current=Get-Process -Id $pidToStop -ErrorAction SilentlyContinue}while($current -and (Get-Date) -lt $deadline);if($current){throw 'Orphaned direct server did not exit'}};@{released=$owned.Count}|ConvertTo-Json -Compress`;
}
export function llamaDirectPortGuardScript() {
 return `$listener=netstat -ano -p TCP | Select-String -Pattern '^\\s*TCP\\s+\\S+:18181\\s+\\S+\\s+LISTENING\\s+\\d+\\s*$' | Select-Object -First 1;if($listener){throw 'Direct port occupied'}`;
}
export class WindowsLlamaCppHost extends WindowsLMStudioHost {
 constructor(options){super(options);llamaServerArguments(this.profile);if(this.host.modelKey!==(isMeroMero(this.profile)?MEROMERO_HOST_KEY:'qwen3.8-27b'))throw Error('Unsupported model key');this.statePath=this.statePath.replace(/\.json$/,'.llama.json');}
 async cleanupOwned(){
  if(!this.owned)return;
  const r=this.owned;
  // PID reuse cannot authorize stopping another program or a later instance.
  await this.remote(llamaCleanupScript(r),{timeout:LLAMA_CLEANUP_REMOTE_TIMEOUT_MS});
  await unlink(this.statePath).catch(e=>{if(e.code!=='ENOENT')throw e;});this.owned=null;
 }
 async start({signal}={}){
  this.client=await this.pool.connect();let guard,sampling=false;
  const controller=new AbortController();const abort=()=>controller.abort();signal?.addEventListener('abort',abort,{once:true});if(signal?.aborted)abort();
  try{
   const locked=(await this.client.query('SELECT pg_try_advisory_lock(hashtext($1)) AS locked',[`local-host:${this.key}`])).rows[0]?.locked;
   if(!locked)throw new LocalHostBusyError('4090 is leased by another request');
   await mkdir(this.directory,{recursive:true,mode:0o700});
   try{this.owned=JSON.parse(await readFile(this.statePath,'utf8'));}catch(e){if(e.code!=='ENOENT')throw e;}
   if(this.owned)await this.cleanupOwned();
   let [sample,studioPID]=await Promise.all([this.sample(),this.serverPID()]);
   if(!sample.busy&&sample.vramPct>20){
    await this.remote(llamaOrphanCleanupScript(this.profile),{timeout:LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT_MS});
    sample=await this.sample();
   }
   if(sample.busy||localHostMemoryExceeded(sample)||sample.vramPct>20)throw new LocalHostBusyError('GPU already in use');
   if(studioPID){
    const d=JSON.parse(await this.remote("Invoke-RestMethod 'http://127.0.0.1:1234/api/v1/models' -TimeoutSec 3 | ConvertTo-Json -Depth 8 -Compress"));
    if(!Array.isArray(d.models)||d.models.some(m=>!Array.isArray(m.loaded_instances)||m.loaded_instances.length))throw new LocalHostBusyError('LM Studio model is in use');
   }
   if(controller.signal.aborted)throw Error('Direct startup cancelled');
   const cmd=llamaServerCommand(this.profile);
   // WMI avoids Windows OpenSSH waiting for a detached Start-Process child.
   // Do not pass the caller's cancellation signal until the PID has been
   // captured locally. Otherwise WMI can leave a live child without a recovery
   // record when SSH is terminated between Create and its JSON response.
   const owned=JSON.parse(await this.remote(`$ErrorActionPreference='Stop';$null=& {${llamaOrphanCleanupScript(this.profile)}};${llamaDirectPortGuardScript()};${llamaStartupArtifactScript(this.profile)};$r=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{CommandLine=${ps(cmd)};CurrentDirectory=${ps(LLAMA_SERVER_PATH.slice(0,LLAMA_SERVER_PATH.lastIndexOf('\\')))}};if($r.ReturnValue -ne 0){throw 'Direct server launch failed'};$p=Get-CimInstance Win32_Process -Filter "ProcessId=$($r.ProcessId)";@{pid=[int]$r.ProcessId;path=$p.ExecutablePath;created=$p.CreationDate.ToUniversalTime().ToString('o')} | ConvertTo-Json -Compress`,{timeout:60000}));
   if(!validLlamaOwnership(owned))throw Error('Direct server identity unavailable');this.owned=owned;
   await writeFile(this.statePath,JSON.stringify(owned),{mode:0o600});
   if(controller.signal.aborted)throw Error('Direct startup cancelled');
   const socket=createServer();await new Promise(r=>socket.listen(0,'127.0.0.1',r));const port=socket.address().port;await new Promise(r=>socket.close(r));
   this.tunnel=spawn('ssh',[...this.sshArgs.slice(0,-1),'-N','-o','ExitOnForwardFailure=yes','-o','ServerAliveInterval=5','-o','ServerAliveCountMax=2','-L',`127.0.0.1:${port}:127.0.0.1:18181`,this.sshArgs.at(-1)],{stdio:'ignore'});
   let exited=false;this.tunnel.on('error',()=>{exited=true;});this.tunnel.on('exit',()=>{exited=true;});
   guard=setInterval(async()=>{if(sampling)return;sampling=true;try{const s=await this.sample();if(s.busy||localHostMemoryExceeded(s))abort();}catch{abort();}finally{sampling=false;}},2000);
   const base=`http://127.0.0.1:${port}`;let ready=false;const deadline=Date.now()+120000;
   while(!controller.signal.aborted&&!exited&&Date.now()<deadline){try{ready=(await fetch(base+'/health',{signal:AbortSignal.timeout(1000)})).ok;}catch{}if(ready)break;await pause(500);}
   if(!ready||controller.signal.aborted||exited)throw Error('Direct server readiness or memory guard failed');
   const props=await fetch(base+'/props',{signal:AbortSignal.timeout(3000)}).then(r=>r.json());
   if(props.default_generation_settings?.n_ctx!==this.profile.contextWindow||props.total_slots!==1)throw Error('Direct server context mismatch');
   const finalSample=await this.sample();
   if(finalSample.busy||localHostMemoryExceeded(finalSample)||controller.signal.aborted)throw Error('Direct server memory guard failed');
   return {upstream:base,sample:()=>this.sample(),alive:()=>!exited,waitForIdle:()=>waitForLlamaIdle(base,{alive:()=>!exited}),release:()=>this.release(),reconnectAddress:address=>{this.host=validateHost({...this.host,address});this.sshArgs[this.sshArgs.length-1]=`${this.host.user}@${address}`;}};
  }catch(e){await this.release();throw e;}
  finally {clearInterval(guard);while(sampling)await pause(50);signal?.removeEventListener('abort',abort);}
 }
}
