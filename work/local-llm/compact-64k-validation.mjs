// Explicit end-to-end CLI gate in a new temporary session. Production history
// and settings are never changed; tool execution is confined to test fixtures.
import {readFile,writeFile,mkdtemp,mkdir} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {spawn} from 'node:child_process';
import {randomBytes,createHash} from 'node:crypto';
import {once} from 'node:events';
import {pool} from '../../backend/src/db.mjs';
import {WindowsLMStudioHost,LOCAL_HOST_MEMORY_BUDGET} from '../../backend/src/local-provider-host.mjs';
import {LocalInferenceBridge} from '../../backend/src/local-inference-bridge.mjs';
import {INCLUSIVE_INPUT_PROFILE} from '../../backend/src/local-usage-normalizer.mjs';
import {createLocalAgentLaunch} from '../../backend/src/local-agent-profile.mjs';
const directory=await mkdtemp(join(tmpdir(),'officestra-compact64-'));
const definition=JSON.parse(await readFile(process.env.OFFICESTRA_VALIDATION_PROFILE ?? new URL('./4090-profile.json',import.meta.url),'utf8'));
definition.profile.contextWindow=65536;
const host=new WindowsLMStudioHost({...definition,pool,stateDirectory:join(directory,'ownership')});
// Isolated experiment only; production host settings are unchanged.
if(process.env.OFFICESTRA_VALIDATION_BATCH){
  const batch=Number(process.env.OFFICESTRA_VALIDATION_BATCH);
  if(![32,64,128].includes(batch))throw new Error('Invalid validation batch');
  let sdk;
  Object.defineProperty(host,'sdk',{configurable:true,get(){return sdk;},set(value){
    sdk=value;
    if(sdk){const load=sdk.llm.load.bind(sdk.llm);sdk.llm.load=(key,options)=>load(key,{...options,config:{...options.config,evalBatchSize:batch}});}
  }});
}
const report={directory,turns:[],usage:[],samples:[]};let bridge,sessionID,phase='load';
const sampleHost=host.sample.bind(host);
host.sample=async()=>{
  try{
    const sample=await sampleHost();report.samples.push({...sample,phase,at:Date.now()});
    if(phase==='load')console.log(JSON.stringify({loadSample:sample}));
    return sample;
  }catch(error){report.sampleError=error.message;throw error;}
};
console.log(JSON.stringify({directory}));
async function run(prompt,label){
  phase=label;
  const spec=createLocalAgentLaunch({profile:definition.profile,character:{id:'isolated-validation',backend:'claude',model:definition.profile.model,effort:'default',fastMode:false,permission:'bypassPermissions',identityPrompt:`격리 검증입니다. ${directory} 안의 파일만 수정하세요. 네트워크/다른 직원/프로젝트/서비스/커밋/푸시 조작 금지. 결과는 한국어로 쓰세요.`},workdir:directory,prompt,previousSessionID:sessionID??null,baseEnvironment:{...process.env,[definition.profile.credentialEnv]:token}});
  const child=spawn(spec.executable,spec.args,{cwd:directory,env:spec.env,stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='';child.stdout.on('data',d=>stdout+=d);child.stderr.on('data',d=>stderr+=d);
  const start=Date.now(),timer=setTimeout(()=>child.kill('SIGTERM'),360000);
  const [code]=await once(child,'close');clearTimeout(timer);
  await writeFile(join(directory,label+'.jsonl'),stdout,{mode:0o600});await writeFile(join(directory,label+'.stderr'),stderr,{mode:0o600});
  const events=stdout.trim().split('\n').map(s=>{try{return JSON.parse(s);}catch{return {};}});
  const result=events.findLast(e=>e.type==='result');sessionID=result?.session_id??events.find(e=>e.session_id)?.session_id??sessionID;
  const row={label,code,elapsedMs:Date.now()-start,sessionID,error:result?.is_error,result:result?.result,usage:result?.usage,compactions:events.filter(e=>e.subtype==='compact_boundary').map(e=>e.compact_metadata??e.compactMetadata),tools:events.flatMap(e=>e.message?.content??[]).filter(c=>c.type==='tool_use').map(c=>c.name)};
  row.assistantText=events.filter(e=>e.type==='assistant').flatMap(e=>e.message?.content??[]).filter(c=>c.type==='text').map(c=>c.text).join('\n');
  const reads=new Set(events.filter(e=>e.type==='assistant').flatMap(e=>e.message?.content??[]).filter(c=>c.type==='tool_use'&&c.name==='Read').map(c=>c.id));
  row.successfulReads=events.filter(e=>e.type==='user').flatMap(e=>e.message?.content??[]).filter(c=>c.type==='tool_result'&&reads.has(c.tool_use_id)&&!c.is_error).length;
  report.turns.push(row);console.log(JSON.stringify(row));
  if(code!==0||!result||result.is_error)throw new Error(label+' CLI failed');
  return row;
}
const token=randomBytes(32).toString('hex');
try{
  const state=await fetch('http://127.0.0.1:4317/api/local-profiles').then(r=>r.json());
  if(state.statuses.some(s=>!['idle','error'].includes(s.state)))throw new Error('Production local model busy');
  const resource=await host.start({signal:AbortSignal.timeout(120000)});
  const model=await host.sdk.llm.model(definition.profile.model);
  report.model=await model.getModelInfo();
  if(report.model.contextLength!==65536)throw new Error('Runtime context is not 64K');
  console.log(JSON.stringify({loaded:true,model:report.model}));
  bridge=new LocalInferenceBridge({upstream:resource.upstream,token,model:definition.profile.model,usageProfile:INCLUSIVE_INPUT_PROFILE,...LOCAL_HOST_MEMORY_BUDGET,readResources:async()=>{const s=await resource.sample();report.samples.push(s);return s;},audit:r=>report.usage.push({phase,input:r.totalInputTokens}),release:resource.release,sampleTimeoutMs:15000,sampleMaxAgeMs:20000,pollMs:3000,requestTimeoutMs:240000});
  definition.profile.endpoint=await bridge.start();
  await writeFile(join(directory,'math.mjs'),'export const sum = (a,b) => a-b;\n');
  await writeFile(join(directory,'text.mjs'),'export const normalize = s => s.toLowerCase();\n');
  await writeFile(join(directory,'list.mjs'),'export const unique = xs => xs;\n');
  await writeFile(join(directory,'check.mjs'),`import assert from 'node:assert/strict';import {sum} from './math.mjs';import {normalize} from './text.mjs';import {unique} from './list.mjs';assert.equal(sum(2,3),5);assert.equal(normalize(' AB '),'ab');assert.deepEqual(unique([1,1,2]),[1,2]);console.log('ALL_PASS');\n`);
  const facts='반드시 다음 다섯 값을 기억하세요: 프로젝트=푸른보리, 확인코드=HOP-7319, 기준포트=4317, 대상폴더=임시검증폴더, 정책=커밋금지. 긴 재고 내용은 반복 자료이므로 나중에 요약할 때 개별 재고는 버리고 이 다섯 값과 작업 지침만 보존하세요.';
  const prepared=await run(`${facts} Read 도구의 file_path에는 반드시 다음 두 절대경로를 그대로 사용하세요: ${join(directory,'math.mjs')} 그리고 ${join(directory,'check.mjs')}. 읽어 어떤 코드인지 확인만 하세요. 아직 수정/명령 실행하지 마세요. 확인했다고 짧게 답하세요.`,'prepare-read');
  if(prepared.successfulReads<2)throw new Error('Pre-compaction successful Read was not exercised');
  const rows=Array.from({length:3600},(_,i)=>`${i}: ${createHash('sha256').update('fixture-'+i).digest('hex').slice(0,32)}\n`);
  let lo=0,hi=rows.length;
  while(lo<hi){const mid=Math.ceil((lo+hi)/2);if((await model.tokenize(rows.slice(0,mid).join(''))).length<=40000)lo=mid;else hi=mid-1;}
  const filler=rows.slice(0,lo).join('');report.fillerTokens=(await model.tokenize(filler)).length;
  await run(`${facts}\n<reference-data>\n${filler}</reference-data>\n${facts}\n지금은 도구를 쓰지 말고 '기억했습니다' 한 줄로만 답하세요.`,'seed');
  const coding=await run('앞서 지정한 다섯 값을 먼저 써주세요. 이어서 현재 폴더의 check.mjs와 math.mjs/text.mjs/list.mjs를 읽고 node check.mjs를 실행하여 실패를 확인하세요. 세 구현을 수정해 모든 검증을 통과시키고 node check.mjs를 다시 실행하세요. 다른 파일/네트워크/서비스는 건드리지 마세요. 결과를 한국어로 짧게 보고하세요.','after-compact');
  const verify=spawn(process.execPath,['check.mjs'],{cwd:directory,stdio:['ignore','pipe','pipe']});let output='';verify.stdout.on('data',d=>output+=d);verify.stderr.on('data',d=>output+=d);const [code]=await once(verify,'close');report.fixtureCheck={code,output};
  if(code!==0)throw new Error('Coding fixture failed');
  for(const value of ['푸른보리','HOP-7319','4317','임시검증폴더','커밋'])if(!coding.assistantText.includes(value))throw new Error('Memory fact missing: '+value);
  const resumed=await run('다섯 기억 값을 다시 쓰고 node check.mjs를 다시 실행하여 ALL_PASS인지 확인한 뒤 한국어 한 문장으로 보고하세요. 다른 작업은 하지 마세요.','resume');
  for(const value of ['푸른보리','HOP-7319','4317','임시검증폴더','커밋'])if(!resumed.assistantText.includes(value))throw new Error('Resumed memory fact missing: '+value);
  if(!report.turns.some(t=>t.compactions.length))throw new Error('Automatic compaction was not observed');
  if(report.turns.some(t=>t.compactions.length>1))throw new Error('Repeated compaction in one short work turn');
  if(report.turns.at(-1).compactions.length)throw new Error('Compaction immediately repeated on resume');
}catch(error){report.error=error.message;process.exitCode=1;}
finally{await bridge?.stop();await host.release();report.cleanup={serverPID:await host.serverPID(),models:JSON.parse(await host.remote('lms ps --json'))};await pool.end();await writeFile(join(directory,'report.json'),JSON.stringify(report,null,2),{mode:0o600});console.log(JSON.stringify({directory,error:report.error,cleanup:report.cleanup}));}
