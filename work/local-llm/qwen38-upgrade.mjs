// Explicit Qwen 3.8 upgrade job: download -> hash -> isolated CLI gates -> idle-only switch.
// Never restarts 4317 or deletes the old model/profile/history.
import assert from 'node:assert/strict';
import {readFile,writeFile} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {pool} from '../../backend/src/db.mjs';
import {WindowsLMStudioHost,LOCAL_HOST_MEMORY_BUDGET} from '../../backend/src/local-provider-host.mjs';
const statusPath=new URL('../reports/qwen38-upgrade-status.json',import.meta.url);
const profilePath=new URL('./qwen38-profile.json',import.meta.url);
const logPath=new URL('../reports/qwen38-validation.log',import.meta.url);
const next=JSON.parse(await readFile(profilePath,'utf8'));
const startedAt=new Date().toISOString();
let original,validationDirectory;
async function status(phase,extra={}) {
  await writeFile(statusPath,JSON.stringify({startedAt,updatedAt:new Date().toISOString(),pid:process.pid,
    phase,validationDirectory,...extra},null,2));
  console.log(JSON.stringify({phase,...extra}));
}
async function readState() {
  const c=await pool.connect();
  try {
    await c.query('BEGIN READ ONLY');
    const profile=(await c.query('SELECT definition FROM local_agent_profiles WHERE id=$1',['local-4090'])).rows[0]?.definition;
    const running=(await c.query(`SELECT t.id FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
      WHERE s.character_id=$1 AND t.status='running'`,['right-woman'])).rowCount;
    const assignment=(await c.query("SELECT config->>'localProfileId' AS id FROM characters WHERE id=$1",['right-woman'])).rows[0]?.id;
    return {profile,running,assignment};
  }finally{await c.query('ROLLBACK');c.release();}
}
async function idle() {
  const s=await readState();
  assert.deepEqual(s.profile,original,'Original profile changed during upgrade');
  assert.equal(s.assignment,'local-4090','Employee assignment changed; do not overwrite');
  assert.equal(s.running,0,'Employee is working; switch deferred');
  const local=await fetch('http://127.0.0.1:4317/api/local-profiles').then(r=>r.json());
  assert.equal(local.statuses.length,0,'Local sessions are open; do not interrupt');
  const terminal=await fetch('http://127.0.0.1:4317/api/terminal-sessions').then(r=>r.json());
  assert.ok(!terminal.sessions.some(s=>s.characterId==='right-woman'),'Employee terminal is open');
}
async function put(path,body) {
  const response=await fetch('http://127.0.0.1:4317'+path,{method:'PUT',
    headers:{'content-type':'application/json'},body:JSON.stringify(body),signal:AbortSignal.timeout(30000)});
  const result=await response.json();
  assert.ok(response.ok,JSON.stringify(result));return result;
}
try {
  const initial=await readState();original=initial.profile;assert.ok(original);
  await idle();
  await writeFile(new URL('../reports/qwen38-original-profile.json',import.meta.url),JSON.stringify(original,null,2));
  const host=new WindowsLMStudioHost({...original,pool,stateDirectory:'/tmp/officestra-qwen38-check'});
  const deadline=Date.now()+90*60*1000;
  while(true) {
    const models=JSON.parse(await host.remote('lms ls --json'));
    if(models.some(m=>m.modelKey===next.host.modelKey))break;
    assert.ok(Date.now()<deadline,'Download did not complete within 90 minutes; original retained');
    const info=await host.remote("$j=Get-Content -Raw 'C:\\Users\\pleas\\.cache\\lm-studio\\.internal\\download-jobs-info.json'|ConvertFrom-Json; $x=$j.jobs|Where-Object {$_.jobName -like '*Qwen3.8*'}|Select-Object -Last 1; $x.tasks|ForEach-Object {$_.download|Select-Object filename,progress,status,errorMessage}|ConvertTo-Json -Compress");
    await status('downloading',{downloads:JSON.parse(info)});
    await new Promise(r=>setTimeout(r,60000));
  }
  await status('verifying-hashes');
  const files=[
    ['G:\\llm\\unsloth\\Qwen3.8-27B-GGUF\\Qwen3.8-27B-UD-Q4_K_M.gguf','322e194ff79741c7baa497c240f677f54b201b0efab44ca8e50f122b39123482'],
    ['G:\\llm\\unsloth\\Qwen3.8-27B-GGUF\\mmproj-F16.gguf','cbb841a9ee0636b2ec172f5bb8df2ea8dfeb01e90fe7c6126581d662a0b4e43e'],
  ];
  for(const [path,expected]of files){
    const actual=await host.remote("(Get-FileHash -Algorithm SHA256 -LiteralPath '"+path+"').Hash",{timeout:300000});
    assert.equal(actual.toLowerCase(),expected,'Downloaded file hash mismatch');
  }
  await idle();
  await status('isolated-cli-validation');
  const child=spawn(process.execPath,['work/local-llm/compact-64k-validation.mjs'],{
    cwd:new URL('../../',import.meta.url),env:{...process.env,OFFICESTRA_VALIDATION_PROFILE:profilePath.pathname},
    stdio:['ignore','pipe','pipe']});
  let output='',error='',pending='';
  child.stdout.on('data',d=>{
    output+=d;pending+=d;
    const lines=pending.split('\n');pending=lines.pop();
    for(const line of lines){try{
      const event=JSON.parse(line);
      console.log(JSON.stringify({validation:event.label??(event.loaded?'loaded':'state'),
        directory:event.directory,code:event.code,error:event.error,elapsedMs:event.elapsedMs,
        tools:event.tools,compactions:event.compactions?.length,context:event.model?.contextLength}));
      if(event.loadSample)console.log(JSON.stringify({loadSample:event.loadSample}));
    }catch{}}
  });
  child.stderr.on('data',d=>error+=d);
  const [exitCode]=await once(child,'close');
  await writeFile(logPath,output+'\nSTDERR\n'+error);
  const lines=output.trim().split('\n').flatMap(s=>{try{return [JSON.parse(s)]}catch{return []}});
  validationDirectory=lines.find(x=>x.directory)?.directory;
  assert.equal(exitCode,0,'Isolated validation failed; original model retained');
  assert.ok(validationDirectory);
  const report=JSON.parse(await readFile(validationDirectory+'/report.json','utf8'));
  assert.ok(!report.error&&!report.cleanupError);
  assert.equal(report.cleanup.serverPID,0);
  assert.equal(report.cleanup.models.length,0);
  assert.equal(report.model.contextLength,65536);
  assert.ok(report.samples.length>0);
  assert.ok(Math.max(...report.samples.map(s=>s.vramPct))<LOCAL_HOST_MEMORY_BUDGET.vramGuardPercent);
  if(process.argv.includes('--validate-only')) {
    await status('validated-awaiting-backend-restart',{model:next.host.modelKey,
      peakVram:Math.max(...report.samples.map(s=>s.vramPct)),operatingProfileUnchanged:true});
  } else {
  await idle();
  await status('switching');
  await put('/api/local-profiles',{definition:next,enabled:true});
  const switched=await put('/api/characters/right-woman/local-profile',{profileId:next.profile.id});
  const current=await fetch('http://127.0.0.1:4317/api/local-profiles').then(r=>r.json());
  assert.ok(current.assignments.some(a=>a.characterId==='right-woman'&&a.profileId===next.profile.id));
  await status('completed',{switched,model:next.host.modelKey,contextWindow:65536,
    peakVram:Math.max(...report.samples.map(s=>s.vramPct)),
    tests:report.turns.map(t=>({label:t.label,error:t.error,tools:t.tools,compactions:t.compactions.length})),
    oldModelPreserved:true,backendRestarted:false});
  }
}catch(error){await status('stopped',{error:error.message,originalModelNotDeleted:true});process.exitCode=1;}
finally{await pool.end();}
