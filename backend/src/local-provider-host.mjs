import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { createHash } from 'node:crypto';
import { mkdir, readFile, writeFile, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { LMStudioClient } from '@lmstudio/sdk';

const sleep=ms=>new Promise(r=>setTimeout(r,ms));
export class LocalHostBusyError extends Error {}
// GPU memory budget only: CPU RAM retains its existing safety threshold.
// User-approved tolerance above 95%; this remains a sampled stop threshold,
// not a driver-enforced allocation cap. Preserve CPU RAM protection.
export const LOCAL_HOST_MEMORY_BUDGET=Object.freeze({vramGuardPercent:98,ramGuardPercent:77});
export const LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS=30000;
export const QWEN38_LMSTUDIO_MODEL_KEYS=Object.freeze(['qwen3.8-27b','qwen3.8-27b-uncensored']);
export function isQwen38LMStudioModelKey(value){return QWEN38_LMSTUDIO_MODEL_KEYS.includes(value);}
export const LOCAL_HOST_LOAD_CONFIG=Object.freeze({gpu:Object.freeze({ratio:1}),gpuStrictVramCap:false,contextLength:32768,tryMmap:false,keepModelInMemory:false,evalBatchSize:128,flashAttention:true});
export function localHostLoadConfig(profile) {
  if(profile?.kvCacheQuantization!==undefined){
    if(profile.backend!=='codex'||profile.contextWindow!==65536||profile.kvCacheQuantization!=='q8_0')throw new Error('Unsupported local KV configuration');
    return {...LOCAL_HOST_LOAD_CONFIG,contextLength:65536,llamaKCacheQuantizationType:'q8_0',llamaVCacheQuantizationType:'q8_0'};
  }
  if(profile?.contextWindow===32768)return LOCAL_HOST_LOAD_CONFIG;
  if(profile?.contextWindow===65536)return {...LOCAL_HOST_LOAD_CONFIG,contextLength:65536,llamaKCacheQuantizationType:'q8_0',llamaVCacheQuantizationType:'q8_0'};
  throw new Error('Unsupported local context configuration');
}
export function localHostMemoryExceeded(sample) {
  return sample.vramPct>=LOCAL_HOST_MEMORY_BUDGET.vramGuardPercent||sample.ramPct>=LOCAL_HOST_MEMORY_BUDGET.ramGuardPercent;
}
export function localHostResourceSampleScript(comfyPort) {
  if(!Number.isInteger(comfyPort)||comfyPort<1||comfyPort>65535)throw new TypeError('Invalid ComfyUI port');
  // Win32_OperatingSystem CIM queries can exceed the SSH command deadline on
  // an otherwise idle Windows host. The standard .NET APIs return the same
  // physical-memory and listener facts without waiting on the WMI provider.
  return `$ErrorActionPreference='Stop'; $g=(& nvidia-smi --query-gpu=memory.total,memory.used --format=csv,noheader,nounits) -split ','; Add-Type -AssemblyName Microsoft.VisualBasic; $o=New-Object Microsoft.VisualBasic.Devices.ComputerInfo; $busy=$false; $queueState='offline'; $listener=[System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners() | Where-Object {$_.Port -eq ${comfyPort}} | Select-Object -First 1; if($listener){ try{$q=Invoke-RestMethod -Uri http://127.0.0.1:${comfyPort}/queue -TimeoutSec 2; if($null -eq $q.queue_running -or $null -eq $q.queue_pending){throw 'Invalid queue'}; $busy=($q.queue_running.Count + $q.queue_pending.Count) -gt 0; $queueState='ready'}catch{$busy=$true;$queueState='unavailable'} }; [pscustomobject]@{vramPct=100*[double]$g[1]/[double]$g[0];ramPct=100*(1-[double]$o.AvailablePhysicalMemory/[double]$o.TotalPhysicalMemory);busy=$busy;queueState=$queueState} | ConvertTo-Json -Compress`;
}
export function localHostListenerPIDScript(port) {
  if(!Number.isInteger(port)||port<1||port>65535)throw new TypeError('Invalid listener port');
  // Get-NetTCPConnection can block on the Windows CIM provider. netstat is a
  // read-only native query and gives us the owning PID without that dependency.
  return `$line=netstat -ano -p TCP | Select-String -Pattern '^\\s*TCP\\s+\\S+:${port}\\s+\\S+\\s+LISTENING\\s+(\\d+)\\s*$' | Select-Object -First 1;if($line -and $line.Line -match '\\s+(\\d+)\\s*$'){$Matches[1]}else{0}`;
}
export function verifyLocalRuntimeVersion(value) {
  if(value?.version!=='0.4.24'||value?.build!==1)throw new Error('LM Studio usage protocol requires revalidation after an update');
}
export function validateHost(h) {
  if(!h||!/^[a-zA-Z0-9.-]+$/.test(h.address)||!/^[a-zA-Z0-9_-]+$/.test(h.user)||
    !Number.isInteger(h.sshPort)||h.sshPort<1||h.sshPort>65535||
    typeof h.keyPath!=='string'||!h.keyPath.startsWith('/')||/[\r\n\0]/.test(h.keyPath)||
    typeof h.hostKeyAlias!=='string'||/[\r\n\0]/.test(h.hostKeyAlias)||
    typeof h.modelKey!=='string'||!h.modelKey||/[\r\n\0]/.test(h.modelKey)||
    !Number.isInteger(h.comfyPort)||h.comfyPort<1||h.comfyPort>65535)throw new Error('Invalid local host configuration');
  return {...h};
}
// Windows OpenSSH may pass the remote command through cmd.exe (8191 chars).
// Keep large scripts off that command line; the short PowerShell bootstrap
// reads the exact same Base64 payload from stdin instead.
export function localHostRemoteCommand(script) {
  const encoded=Buffer.from("$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; "+script,'utf16le').toString('base64');
  const prefix='powershell -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand ';
  if(prefix.length+encoded.length<=7000)return {command:prefix+encoded,input:null};
  const bootstrap="$s=[Console]::In.ReadToEnd(); & ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($s))))";
  return {command:prefix+Buffer.from(bootstrap,'utf16le').toString('base64'),input:encoded};
}

export class WindowsLMStudioHost {
  constructor({host,profile,pool,stateDirectory,loadConfig=localHostLoadConfig}) {
    this.host=validateHost(host);this.profile=profile;this.pool=pool;this.directory=stateDirectory;
    this.loadConfig=loadConfig;
    this.key=createHash('sha256').update(`${host.address}:${host.sshPort}`).digest('hex');
    this.statePath=join(stateDirectory,`${this.key}.json`);this.owned=null;this.client=null;this.tunnel=null;
    this.sshArgs=['-o','BatchMode=yes','-o','ConnectTimeout=6','-o','StrictHostKeyChecking=yes',
      '-o',`HostKeyAlias=${host.hostKeyAlias}`,'-i',host.keyPath,'-p',String(host.sshPort),`${host.user}@${host.address}`];
  }
  remote(script,{timeout=15000,signal}={}) {
    const {command,input}=localHostRemoteCommand(script);
    return new Promise((resolve,reject)=>{
      const child=spawn('ssh',[...this.sshArgs,command],{stdio:[input===null?'ignore':'pipe','pipe','pipe']});
      if(input!==null){child.stdin.on('error',()=>{});child.stdin.end(input);}
      let output='',stderr='';
      child.stdout.on('data',d=>{output+=d;if(output.length>1024*1024)child.kill('SIGTERM');});
      child.stderr.on('data',d=>{stderr+=d;if(stderr.length>1024*1024)child.kill('SIGTERM');});
      const abort=()=>child.kill('SIGTERM');signal?.addEventListener('abort',abort,{once:true});if(signal?.aborted)abort();
      let timedOut=false;const timer=setTimeout(()=>{timedOut=true;abort();},timeout);
      child.once('error',reject);child.once('close',code=>{clearTimeout(timer);signal?.removeEventListener('abort',abort);
        if(code!==0)reject(new Error(timedOut?`4090 SSH operation timed out after ${timeout}ms`:'4090 SSH operation failed',{cause:stderr.slice(-2000)}));else resolve(output.trim().replace(/^\uFEFF/,''));});
    });
  }
  async sample() {
    const raw=await this.remote(localHostResourceSampleScript(this.host.comfyPort),{timeout:LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT_MS});
    return {...JSON.parse(raw),sampledAt:Date.now()};
  }
  async serverPID() {
    return Number(await this.remote(localHostListenerPIDScript(1234)));
  }
  async cleanupOwned() {
    const owned=this.owned;if(!owned)return;
    if(!Number.isSafeInteger(owned.serverPID)||owned.serverPID<=0||!/^[a-zA-Z0-9._-]+$/.test(owned.model))throw new Error('Invalid owned resource recovery record');
    if(await this.serverPID()===owned.serverPID) {
      const models=JSON.parse(await this.remote('lms ps --json'));
      if(models.some(m=>(m.identifier??m.id)===owned.model))await this.remote(`lms unload '${owned.model}'`);
      const remaining=JSON.parse(await this.remote('lms ps --json'));
      if(remaining.some(m=>(m.identifier??m.id)===owned.model))throw new Error('Owned model did not unload');
      if(!remaining.length && await this.serverPID()===owned.serverPID)await this.remote('lms server stop');
    }
    await unlink(this.statePath).catch(e=>{if(e.code!=='ENOENT')throw e;});this.owned=null;
  }
  async start({signal}={}) {
    this.client=await this.pool.connect();
    try {
      const lock=await this.client.query('SELECT pg_try_advisory_lock(hashtext($1)) AS locked',[`local-host:${this.key}`]);
      if(!lock.rows[0]?.locked)throw new LocalHostBusyError('4090 is leased by another request');
      await mkdir(this.directory,{recursive:true,mode:0o700});
      try{this.owned=JSON.parse(await readFile(this.statePath,'utf8'));}catch(e){if(e.code!=='ENOENT')throw e;}
      // Only a recorded server PID and model may be recovered after a crash.
      if(this.owned)await this.cleanupOwned();
      const s=await this.sample();if(s.busy||localHostMemoryExceeded(s))throw new LocalHostBusyError('ComfyUI or memory budget is busy');
      const models=JSON.parse(await this.remote('lms ps --json'));if(models.length||await this.serverPID())throw new LocalHostBusyError('Existing LM Studio workload is not owned by OFFICESTRA');
      if(!/^[a-zA-Z0-9._-]+$/.test(this.profile.model))throw new Error('Unsafe model identifier');
      await this.remote('lms server start --port 1234 --bind 127.0.0.1');
      this.owned={model:this.profile.model,serverPID:await this.serverPID()};
      if(!this.owned.serverPID)throw new Error('Owned server PID unavailable');
      await writeFile(this.statePath,JSON.stringify(this.owned),{mode:0o600});
      const probe=createServer();await new Promise(r=>probe.listen(0,'127.0.0.1',r));const port=probe.address().port;await new Promise(r=>probe.close(r));
      this.tunnel=spawn('ssh',[...this.sshArgs.slice(0,-1),'-N','-o','ExitOnForwardFailure=yes','-o','ServerAliveInterval=5','-o','ServerAliveCountMax=2','-L',`127.0.0.1:${port}:127.0.0.1:1234`,this.sshArgs.at(-1)],{stdio:'ignore'});
      let exited=false;this.tunnel.once('error',()=>{exited=true;});this.tunnel.once('exit',()=>{exited=true;});
      let ready=false;for(let i=0;i<30;i++){if(signal?.aborted||exited)throw new Error('SSH tunnel unavailable');try{const r=await fetch(`http://127.0.0.1:${port}/api/v1/models`,{signal:AbortSignal.timeout(500)});if(r.ok){ready=true;break;}}catch{}await sleep(100);}
      if(!ready)throw new Error('SSH tunnel readiness timeout');
      const controller=new AbortController();const abort=()=>controller.abort();signal?.addEventListener('abort',abort,{once:true});if(signal?.aborted)abort();
      const timer=setTimeout(abort,120000);let sampling=false;
      const guard=setInterval(async()=>{if(sampling)return;sampling=true;try{const r=await this.sample();if(localHostMemoryExceeded(r)||r.busy)abort();}catch{abort();}finally{sampling=false;}},1500);
      try{
        this.sdk=new LMStudioClient({baseUrl:`ws://127.0.0.1:${port}`});
        let versionTimer;
        try{verifyLocalRuntimeVersion(await Promise.race([this.sdk.system.getLMStudioVersion(),new Promise((_,reject)=>{versionTimer=setTimeout(()=>reject(new Error('LM Studio version check timed out')),8000);})]));}
        finally{clearTimeout(versionTimer);}
        await this.sdk.llm.load(this.host.modelKey,{identifier:this.profile.model,config:this.loadConfig(this.profile),signal:controller.signal,verbose:false});
        const loaded=await this.sample();
        if(controller.signal.aborted||localHostMemoryExceeded(loaded)||loaded.busy)throw new Error('Local model exceeds memory budget after loading');
      }
      finally{clearTimeout(timer);clearInterval(guard);signal?.removeEventListener('abort',abort);}
      return {upstream:`http://127.0.0.1:${port}`,sample:()=>this.sample(),release:()=>this.release(),alive:()=>!exited,
        // Called only after idle-state and pinned-host verification by settings.
        // Retain the old ownership record/lease while cleaning up at the new route.
        reconnectAddress:address=>{this.host=validateHost({...this.host,address});this.sshArgs[this.sshArgs.length-1]=`${this.host.user}@${this.host.address}`;}};
    }catch(e){await this.release();throw e;}
  }
  async release() {
    let failure;
    try { await this.sdk?.[Symbol.asyncDispose](); } catch {}
    this.sdk=null;
    try{await this.cleanupOwned();}catch(e){failure=e;}
    this.tunnel?.kill('SIGTERM');this.tunnel=null;
    if(this.client){try{await this.client.query('SELECT pg_advisory_unlock(hashtext($1))',[`local-host:${this.key}`]);}finally{this.client.release();this.client=null;}}
    if(failure)throw failure;
  }
}
