import test from 'node:test';
import assert from 'node:assert/strict';
import { localSelectionSettings, selectLocalProfile, setLocalReasoning, setLocalHostAddress, normalizeLocalHostAddress } from '../src/local-profile-selection.mjs';
import { AgentRuntime } from '../src/agent-runtime.mjs';
import { randomUUID } from 'node:crypto';
import pg from 'pg';

const definition={profile:{id:'local-test',providerKind:'local',backend:'claude',model:'qwen-test',endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:32768,maxOutputTokens:4096,usageProtocol:'anthropic-normalized-v1'},host:{address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/key',hostKeyAlias:'test',modelKey:'qwen/test',comfyPort:8188}};
const original={id:'test',backend:'codex',model:'gpt-test',effort:'high',fastMode:true,permission:'workspace-write',config:{executablePath:'/custom/codex',keep:'unchanged'}};
test('IPv4 input rejects commands, ports, URLs, invalid octets and empty values',()=>{
  assert.equal(normalizeLocalHostAddress(' 222.109.147.73 '),'222.109.147.73');
  for(const value of ['',null,123,'host','127.0.0.1:22','http://127.0.0.1','1.2.3.256','01.2.3.4','1.2.3.4;whoami','::1'])assert.throws(()=>normalizeLocalHostAddress(value));
});
test('address save verifies pinned host, preserves session/history and rolls back failures',async()=>{
  const s=fixture();const writes=[];let verified=0,closed=0;let rejectProbe=false;
  const client={query:async(sql,values)=>{
    if(sql.startsWith('SELECT config'))return {rows:[{config:{localProfileId:'local-test',localReasoning:'on',keep:'same'}}]};
    if(sql.startsWith('SELECT definition'))return {rows:[{definition}]};
    writes.push({sql,values});return {rows:[]};
  },release:()=>{}};
  const args={...s,pool:{connect:async()=>client},characterID:'test',address:'222.109.147.73',localProviders:{closeIdleProfile:async()=>{closed++;}},verifyHost:async d=>{
    verified++;assert.equal(d.host.hostKeyAlias,definition.host.hostKeyAlias);assert.equal(d.host.address,'222.109.147.73');
    if(rejectProbe)throw new Error('SSH mismatch');
  }};
  await setLocalHostAddress(args);
  assert.equal(verified,1);assert.equal(closed,1);
  assert.deepEqual(JSON.parse(writes.find(w=>w.sql.startsWith('UPDATE characters')).values[1]),{localProfileId:'local-test',localReasoning:'on',keep:'same',localHostAddress:'222.109.147.73'});
  assert.ok(!writes.some(w=>/DELETE|UPDATE turns|UPDATE cli_sessions|UPDATE local_agent_profiles/.test(w.sql)));
  assert.deepEqual(s.counts(),{ended:0,finalized:0,closed:1});
  writes.length=0;rejectProbe=true;
  await assert.rejects(setLocalHostAddress(args),/SSH mismatch/);
  assert.ok(writes.some(w=>w.sql==='ROLLBACK'));assert.ok(!writes.some(w=>w.sql.startsWith('UPDATE')));
  assert.equal(s.runtime.preparingCharacters.size,0);
  s.runtime.running.set('test',{});await assert.rejects(setLocalHostAddress(args),/작업/);
  s.runtime.running.clear();s.runtime.terminalSessionRegistry.has=()=>true;await assert.rejects(setLocalHostAddress(args),/터미널/);
});
test('reasoning change is session-locked, idle-only and preserves existing settings/history',async()=>{
  const s=fixture();const writes=[];
  const client={query:async(sql,values)=>{
    if(sql.startsWith('SELECT config'))return {rows:[{config:{localProfileId:'local-test',keep:'unchanged'}}]};
    if(sql.startsWith('SELECT definition'))return {rows:[{definition:{...definition,host:{...definition.host,modelKey:'qwen3.8-27b'}}}]};
    writes.push({sql,values});return {rows:[]};
  },release:()=>{}};
  const args={...s,pool:{connect:async()=>client},characterID:'test',reasoning:'on'};
  assert.equal((await setLocalReasoning(args)).reasoning,'on');
  const update=writes.find(x=>x.sql.startsWith('UPDATE characters'));
  assert.deepEqual(JSON.parse(update.values[1]),{localProfileId:'local-test',keep:'unchanged',localReasoning:'on'});
  assert.deepEqual(s.counts(),{ended:0,finalized:0,closed:1});
  assert.ok(!writes.some(x=>/DELETE|UPDATE turns|UPDATE cli_sessions/.test(x.sql)));
  assert.equal(s.runtime.preparingCharacters.size,0);
  s.runtime.running.set('test',{});
  await assert.rejects(setLocalReasoning(args),/작업/);
  s.runtime.running.clear();s.runtime.terminalSessionRegistry.has=()=>true;
  await assert.rejects(setLocalReasoning(args),/터미널/);
  s.runtime.terminalSessionRegistry.has=()=>false;
  await assert.rejects(setLocalReasoning({...args,reasoning:'xhigh'}),/Unsupported/);
});

test('direct runtime persists each verified effort without replacing the session',async()=>{
  const s=fixture();const writes=[];
  const direct={...definition,profile:{...definition.profile,backend:'codex',runtime:'llama-cpp-b10982',model:'officestra-qwen38-27b',contextWindow:65536,kvCacheQuantization:'q8_0',usageProtocol:'openai-responses-v1'},host:{...definition.host,modelKey:'qwen3.8-27b'}};
  const client={query:async(sql,values)=>{
    if(sql.startsWith('SELECT config'))return {rows:[{config:{localProfileId:'direct',keep:'unchanged'}}]};
    if(sql.startsWith('SELECT definition'))return {rows:[{definition:direct}]};
    writes.push({sql,values});return {rows:[]};
  },release:()=>{}};
  for(const reasoning of ['low','medium','xhigh']){
    assert.equal((await setLocalReasoning({...s,pool:{connect:async()=>client},characterID:'test',reasoning})).reasoning,reasoning);
  }
  assert.deepEqual(writes.filter(w=>w.sql.startsWith('UPDATE characters')).map(w=>JSON.parse(w.values[1]).localReasoning),['low','medium','xhigh']);
  assert.ok(!writes.some(w=>/DELETE|UPDATE turns|UPDATE cli_sessions/.test(w.sql)));
  await assert.rejects(setLocalReasoning({...s,pool:{connect:async()=>client},characterID:'test',reasoning:'on'}),/Unsupported/);
});

test('local selection pins measured model and restores exact previous cloud settings/config',()=>{
  const before=structuredClone(original);
  const local=localSelectionSettings(original,definition);
  assert.equal(local.backend,'claude');assert.equal(local.effort,'default');assert.equal(local.fastMode,false);assert.equal(local.permission,'auto');
  assert.equal(local.config.executablePath,undefined);assert.equal(local.config.keep,'unchanged');
  const restored=localSelectionSettings(local,null);
  for(const key of ['backend','model','effort','fastMode','permission','config'])assert.deepEqual(restored[key],original[key]);
  assert.deepEqual(original,before);
  assert.equal(localSelectionSettings(local,definition),null);
  assert.equal(localSelectionSettings(original,null),null);
});

test('local selection uses the profile runner and maps permissions for that CLI',()=>{
  const codexDefinition={...definition,profile:{...definition.profile,id:'local-codex',backend:'codex'}};
  assert.equal(localSelectionSettings(original,codexDefinition).backend,'codex');
  assert.equal(localSelectionSettings(original,codexDefinition).permission,'workspace-write');
  assert.equal(localSelectionSettings({...original,permission:'danger-full-access'},codexDefinition).permission,'danger-full-access');
  assert.equal(localSelectionSettings({...original,permission:'plan'},codexDefinition).permission,'read-only');
  assert.throws(()=>localSelectionSettings(original,{...definition,profile:{...definition.profile,backend:'antigravity'}}),/Unsupported local runner backend/);
});

test('selecting another local profile does not overwrite cloud restore point',()=>{
  const first=localSelectionSettings(original,definition);
  const second=localSelectionSettings(first,{...definition,profile:{...definition.profile,id:'second',model:'other'}});
  assert.deepEqual(second.config.localPreviousSettings,first.config.localPreviousSettings);
  first.config.localHostAddress='222.109.147.73';
  const codex=localSelectionSettings(first,{...definition,profile:{...definition.profile,id:'codex-profile',backend:'codex'}});
  assert.equal(codex.config.localHostAddress,'222.109.147.73');
  assert.throws(()=>localSelectionSettings({config:{localProfileId:'missing'}},null));
});

function fixture({busy=false,terminal=false,fail=false}={}) {
  const queries=[];const events=[];let ended=0,finalized=0,closed=0;
  const client={query:async(sql,values)=>{queries.push({sql,values});
    if(sql.startsWith('SELECT id,backend'))return {rows:[original]};
    if(sql.startsWith('SELECT definition'))return {rows:[{definition}]};
    if(sql.startsWith('UPDATE characters')&&fail)throw new Error('write failed');
    return {rows:[]};},release:()=>{}};
  const runtime={draining:false,running:new Map(busy?[['test',{}]]:[]),preparingCharacters:new Set(),compactingCharacters:new Set(),terminalSessionRegistry:{has:()=>terminal},
    inspectWorkspaceForSessionEnd:async()=>({characterID:'test'}),applyWorkspaceSessionEndPlan:async()=>{ended++;},finalizeWorkspaceSessionEndPlan:async()=>{finalized++;},closeClaudeWorker:()=>{closed++;}};
  return {runtime,pool:{connect:async()=>client},broadcast:e=>events.push(e),queries,events,counts:()=>({ended,finalized,closed})};
}

test('running/terminal employees blocked without touching settings or other employees',async()=>{
  for(const opts of [{busy:true},{terminal:true}]) {
    const s=fixture(opts);await assert.rejects(()=>selectLocalProfile({...s,characterID:'test',profileID:'local-test'}));assert.equal(s.queries.length,0);
  }
});
test('switch atomically ends previous session, retains history and targets one character',async()=>{
  const s=fixture();const result=await selectLocalProfile({...s,characterID:'test',profileID:'local-test'});
  assert.equal(result.newSession,true);assert.deepEqual(s.counts(),{ended:1,finalized:1,closed:1});
  const update=s.queries.find(q=>q.sql.startsWith('UPDATE characters'));assert.equal(update.values[0],'test');
  assert.ok(!s.queries.some(q=>/DELETE FROM (turns|messages|conversations)/.test(q.sql)));
  assert.ok(s.queries.some(q=>q.sql==='COMMIT'));assert.equal(s.runtime.preparingCharacters.size,0);
});
test('failed write rolls back and releases selection gate without finalizing session',async()=>{
  const s=fixture({fail:true});await assert.rejects(()=>selectLocalProfile({...s,characterID:'test',profileID:'local-test'}));
  assert.ok(s.queries.some(q=>q.sql==='ROLLBACK'));assert.equal(s.counts().finalized,0);assert.equal(s.runtime.preparingCharacters.size,0);
});

test('isolated PostgreSQL: select and restore end sessions without deleting history', {skip:process.env.OFFICESTRA_LOCAL_DB_TEST!=='1'}, async()=>{
  const database=new pg.Pool({connectionString:process.env.DATABASE_URL??'postgres://office:office-local@127.0.0.1:54329/office'});
  const client=await database.connect();const schema='selection_'+randomUUID().replaceAll('-','');
  try {
    await client.query('BEGIN');await client.query(`CREATE SCHEMA "${schema}"`);await client.query(`SET LOCAL search_path TO "${schema}",public`);
    await client.query('CREATE TABLE characters(id text PRIMARY KEY,backend text,model text,effort text,fast_mode boolean,permission text,config jsonb,updated_at timestamptz); CREATE TABLE local_agent_profiles(id text PRIMARY KEY,definition jsonb,enabled boolean); CREATE TABLE cli_sessions(id text PRIMARY KEY,character_id text,ended_at timestamptz); CREATE TABLE active_cli_sessions(character_id text,cli_session_id text)');
    await client.query('INSERT INTO characters VALUES($1,$2,$3,$4,$5,$6,$7::jsonb,now())',[original.id,original.backend,original.model,original.effort,original.fastMode,original.permission,JSON.stringify(original.config)]);
    await client.query('INSERT INTO local_agent_profiles VALUES($1,$2::jsonb,true)',[definition.profile.id,JSON.stringify(definition)]);
    await client.query("INSERT INTO cli_sessions VALUES('old-session','test',NULL); INSERT INTO active_cli_sessions VALUES('test','old-session')");
    const proxy={query:(sql,values)=>client.query(sql==='BEGIN'?'SAVEPOINT selection':sql==='COMMIT'?'RELEASE SAVEPOINT selection':sql==='ROLLBACK'?'ROLLBACK TO SAVEPOINT selection':sql,values),release:()=>{}};
    const s=fixture();s.pool={connect:async()=>proxy};
    s.runtime.inspectWorkspaceForSessionEnd=async()=>({characterID:'test',workspace:null});
    s.runtime.applyWorkspaceSessionEndPlan=(c,plan)=>AgentRuntime.prototype.applyWorkspaceSessionEndPlan.call(s.runtime,c,plan);
    await selectLocalProfile({...s,characterID:'test',profileID:definition.profile.id});
    let row=(await client.query("SELECT * FROM characters WHERE id='test'")).rows[0];assert.equal(row.config.localProfileId,definition.profile.id);assert.equal(row.model,definition.profile.model);
    assert.ok((await client.query("SELECT ended_at FROM cli_sessions WHERE id='old-session'")).rows[0].ended_at);
    assert.equal((await client.query('SELECT * FROM active_cli_sessions')).rowCount,0);
    await selectLocalProfile({...s,characterID:'test',profileID:null});
    row=(await client.query("SELECT * FROM characters WHERE id='test'")).rows[0];assert.equal(row.backend,original.backend);assert.deepEqual(row.config,original.config);
    assert.equal((await client.query('SELECT * FROM cli_sessions')).rowCount,1);
  }finally{await client.query('ROLLBACK');client.release();await database.end();}
});
