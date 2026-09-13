import {readFile,writeFile} from 'node:fs/promises';
import {pool} from '../../backend/src/db.mjs';
import {WindowsLMStudioHost,localHostMemoryExceeded} from '../../backend/src/local-provider-host.mjs';
import {LocalInferenceBridge} from '../../backend/src/local-inference-bridge.mjs';
import {INCLUSIVE_INPUT_PROFILE} from '../../backend/src/local-usage-normalizer.mjs';
import {createLocalAgentLaunch} from '../../backend/src/local-agent-profile.mjs';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
const definition=JSON.parse(await readFile(new URL('./qwen38-profile.json',import.meta.url)));
const host=new WindowsLMStudioHost({...definition,pool,stateDirectory:'/tmp/officestra-q38-reasoning'});
const results=[];
let bridge;
try {
 const resource=await host.start({signal:AbortSignal.timeout(120000)});
 console.log('loaded');
 for(const field of ['thinking']) {
  for(const effort of ['disabled','enabled']) {
   if(localHostMemoryExceeded(await host.sample()))throw Error('memory guard');
   const token=randomBytes(32).toString('hex');
   bridge=new LocalInferenceBridge({upstream:resource.upstream,token,model:definition.profile.model,reasoning:effort==='enabled'?'on':'off',usageProfile:INCLUSIVE_INPUT_PROFILE,readResources:resource.sample,audit:()=>{},vramGuardPercent:98,ramGuardPercent:77,sampleTimeoutMs:15000,sampleMaxAgeMs:15000,pollMs:3000});
   const address=await bridge.start();
   const extra={thinking:{type:'disabled'}};
   const body={model:definition.profile.model,max_tokens:2048,temperature:0,messages:[{role:'user',content:'What is 2 + 3? Answer briefly.'}],...extra};
   const r=await fetch(address+'/v1/messages',{method:'POST',headers:{'content-type':'application/json','x-api-key':token},body:JSON.stringify(body),signal:AbortSignal.timeout(90000)});
   const value=await r.text();results.push({field,effort,status:r.status,value});console.log(JSON.stringify(results.at(-1)));
   assert.equal(r.status,200);const parsed=JSON.parse(value);assert.equal(parsed.content.some(c=>c.type==='thinking'),effort==='enabled');
   if(effort==='enabled') {
    const profile={...definition.profile,endpoint:address,reasoning:'on'};
    const launch=createLocalAgentLaunch({profile,character:{id:'isolated',backend:'claude',model:profile.model,effort:'default',fastMode:false,permission:'bypassPermissions',identityPrompt:'Isolated validation. Do not modify any file or access any network.'},mode:'gui',workdir:'/tmp',prompt:'Use Bash to execute exactly printf THINKING_TOOL_OK and then answer briefly. Do nothing else.',baseEnvironment:{...process.env,[profile.credentialEnv]:token}});
    const child=spawn(launch.executable,launch.args,{env:launch.env,cwd:launch.cwd,stdio:['ignore','pipe','pipe']});let output='',err='';child.stdout.on('data',d=>output+=d);child.stderr.on('data',d=>err+=d);const timer=setTimeout(()=>child.kill('SIGTERM'),90000);const [code]=await once(child,'close');clearTimeout(timer);
    const events=output.split('\n').flatMap(l=>{try{return[JSON.parse(l)]}catch{return[]}});const final=events.findLast(e=>e.type==='result');const tools=events.flatMap(e=>e.message?.content??[]).filter(c=>c.type==='tool_use').map(c=>c.name);
    const result={cli:true,code,error:final?.is_error,tools,final:final?.result,stderr:err.slice(-500)};results.push(result);console.log(JSON.stringify(result));assert.equal(code,0);assert.ok(final&&!final.is_error);assert.ok(tools.includes('Bash'));
   }
   await bridge.stop();bridge=null;
  }
 }
}catch(e){console.log(JSON.stringify({error:e.message}));process.exitCode=1;}
finally{await bridge?.stop();await host.release();console.log(JSON.stringify({cleanup:{serverPID:await host.serverPID(),models:JSON.parse(await host.remote('lms ps --json'))}}));await pool.end();await writeFile(new URL('../reports/qwen38-reasoning-probe.json',import.meta.url),JSON.stringify(results,null,2));}
