import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {setTimeout as delay} from 'node:timers/promises';
import {llamaServerArguments,llamaServerCommand,llamaArtifactVerificationScript,validLlamaOwnership,llamaCleanupScript,llamaOrphanCleanupScript,llamaDirectPortGuardScript,LLAMA_CLEANUP_REMOTE_TIMEOUT_MS,LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT_MS,LLAMA_SERVER_PATH,LLAMA_MODEL_ID,LLAMA_MODEL_ROOT,LLAMA_MODEL_FILE,LLAMA_MODEL_SIZE,LLAMA_MODEL_SHA256,LLAMA_MMPROJ_FILE,LLAMA_MMPROJ_SIZE,LLAMA_MMPROJ_SHA256} from '../src/local-llama-host.mjs';
import {isQwen38LMStudioModelKey,localHostRemoteCommand} from '../src/local-provider-host.mjs';
import {normalizeLocalAgentProfile,createLocalAgentLaunch} from '../src/local-agent-profile.mjs';
import {normalizeLocalResponsesRequest} from '../src/local-inference-bridge.mjs';
import {LocalProviderService,localCodexModelCatalog} from '../src/local-provider-service.mjs';
import {LLAMA_RESPONSES_PROFILE,RESPONSES_INCLUSIVE_PROFILE,extractResponsesUsage} from '../src/local-responses-usage.mjs';
const profile={id:'direct-test',providerKind:'local',backend:'codex',runtime:'llama-cpp-b10982',model:LLAMA_MODEL_ID,endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:65536,maxOutputTokens:4096,usageProtocol:'openai-responses-v1',kvCacheQuantization:'q8_0',reasoning:'medium'};
const host={address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/test-key',hostKeyAlias:'test',modelKey:'qwen3.8-27b',comfyPort:8188};
test('long Windows SSH scripts bypass command-line truncation without changing Unicode or quoting',()=>{
 const scripts=["Write-Output '짧은 명령'",llamaOrphanCleanupScript(profile)+"\n#"+'한글 $x \' " '.repeat(400)+"\nWrite-Output '끝'"];
 for(const [index,script] of scripts.entries()){
  const {command,input}=localHostRemoteCommand(script);
  assert.ok(command.length<7001);
  const commandPayload=command.split(' -EncodedCommand ')[1];
  const decoded=Buffer.from(input??commandPayload,'base64').toString('utf16le');
  assert.ok(decoded.endsWith(script));
  if(index===0)assert.equal(input,null);
  else {assert.ok(input);assert.match(Buffer.from(commandPayload,'base64').toString('utf16le'),/ReadToEnd/);}
 }
});
test('direct server pins 64K KV8 full GPU vision and rejects silent reductions',()=>{
 assert.equal(isQwen38LMStudioModelKey('qwen3.8-27b'),true);
 assert.equal(isQwen38LMStudioModelKey('qwen3.8-27b-uncensored'),true);
 assert.equal(isQwen38LMStudioModelKey('qwen3.6-27b'),false);
 const args=llamaServerArguments(profile);for(const [k,v]of [['-c','65536'],['-ctk','q8_0'],['-ctv','q8_0'],['-ngl','all'],['--fit','off'],['--host','127.0.0.1']])assert.equal(args[args.indexOf(k)+1],v);
 assert.equal(args[args.indexOf('-m')+1],LLAMA_MODEL_ROOT+'\\'+LLAMA_MODEL_FILE);
 assert.equal(args[args.indexOf('--mmproj')+1],LLAMA_MODEL_ROOT+'\\'+LLAMA_MMPROJ_FILE);
 const verification=llamaArtifactVerificationScript();
 for(const value of [LLAMA_MODEL_FILE,String(LLAMA_MODEL_SIZE),LLAMA_MODEL_SHA256,LLAMA_MMPROJ_FILE,String(LLAMA_MMPROJ_SIZE),LLAMA_MMPROJ_SHA256])assert.match(verification,new RegExp(value.replace(/[.*+?^${}()|[\]\\]/g,'\\$&')));
 assert.throws(()=>llamaServerArguments({...profile,kvCacheQuantization:'q4_0'}));assert.throws(()=>normalizeLocalAgentProfile({...profile,contextWindow:32768}));
 assert.throws(()=>llamaServerArguments({...profile,model:'officestra-qwen38-27b'}));
 assert.equal(validLlamaOwnership({pid:12,path:LLAMA_SERVER_PATH,created:'2026-09-15T00:00:00.0000000Z'}),true);
 assert.equal(validLlamaOwnership({pid:12,path:'C:\\other.exe',created:'2026-09-15T00:00:00Z'}),false);
 const cleanup=llamaCleanupScript({pid:12,path:LLAMA_SERVER_PATH,created:'2026-09-15T00:00:00.0000000Z'});
 assert.match(cleanup,/Get-Process -Id 12/);
 assert.match(cleanup,/Stop-Process -Id 12 -Force/);
 assert.match(cleanup,/AddSeconds\(15\)/);
 assert.match(cleanup,/Owned server did not exit/);
 assert.throws(()=>llamaCleanupScript({pid:12,path:'C:\\other.exe',created:'2026-09-15T00:00:00Z'}));
 const command=llamaServerCommand(profile);
 assert.match(command,/llama-server\.exe/);
 assert.match(command,/--port" "18181/);
 const orphanCleanup=llamaOrphanCleanupScript(profile);
 assert.match(orphanCleanup,/Get-Process -Name 'llama-server'/);
 assert.match(orphanCleanup,/\.ExecutablePath -eq/);
 assert.match(orphanCleanup,/\.CommandLine -eq/);
 assert.match(orphanCleanup,/Orphaned direct server did not exit/);
 assert.match(orphanCleanup,/released=\$owned\.Count/);
 assert.equal(LLAMA_CLEANUP_REMOTE_TIMEOUT_MS,45000);
 assert.equal(LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT_MS,60000);
 const portGuard=llamaDirectPortGuardScript();
 assert.match(portGuard,/netstat -ano -p TCP/);
 assert.match(portGuard,/:18181/);
 assert.doesNotMatch(portGuard,/Get-NetTCPConnection/);
});
test('low medium xhigh reach CLI and Responses only for measured direct runtime',()=>{
 for(const effort of ['low','medium','xhigh']){
  const p=normalizeLocalAgentProfile({...profile,reasoning:effort});
  const spec=createLocalAgentLaunch({profile:p,character:{backend:'codex',model:p.model,effort:'default',permission:'read-only',fastMode:false},workdir:'/tmp',prompt:'hello',executable:'/usr/bin/true',baseEnvironment:{[p.credentialEnv]:'test-token'}});
  assert.ok(spec.args.includes(`model_reasoning_effort="${effort}"`));
  assert.equal(normalizeLocalResponsesRequest({reasoning:{effort}},'default',LLAMA_RESPONSES_PROFILE).reasoning.effort,effort);
  assert.throws(()=>normalizeLocalResponsesRequest({},effort,RESPONSES_INCLUSIVE_PROFILE));
 }
 assert.equal(normalizeLocalResponsesRequest({},'default',LLAMA_RESPONSES_PROFILE).reasoning.effort,'xhigh');
 assert.deepEqual(localCodexModelCatalog(profile).models[0].supported_reasoning_levels.map(x=>x.effort),['low','medium','xhigh']);
 const r=extractResponsesUsage({input_tokens:45,output_tokens:16,total_tokens:61,input_tokens_details:{cached_tokens:41}},{profile:LLAMA_RESPONSES_PROFILE});
 assert.equal(r.totalInputTokens,45);assert.equal(r.normalized.input_tokens,4);assert.equal(r.normalized.output_tokens_details,undefined);
});
test('MTP uses the measured native head without changing operating settings',()=>{
 const args=llamaServerArguments(profile);
 assert.deepEqual(args,[
  '-m',LLAMA_MODEL_ROOT+'\\'+LLAMA_MODEL_FILE,
  '--mmproj',LLAMA_MODEL_ROOT+'\\'+LLAMA_MMPROJ_FILE,
  '-c','65536','-ctk','q8_0','-ctv','q8_0','-ngl','all','-fa','on',
  '-np','1','-b','128','-ub','128','--jinja','--host','127.0.0.1',
  '--port','18181','--alias',LLAMA_MODEL_ID,'--load-mode','none','--fit','off',
  '--spec-type','draft-mtp','--spec-draft-n-max','2',
 ]);
 // Reasoning belongs to each request, not to the shared server configuration.
 for(const reasoning of ['default','low','medium','xhigh'])assert.deepEqual(llamaServerArguments({...profile,reasoning}),args);
 const command=llamaServerCommand(profile);
 assert.match(command,/"--spec-type" "draft-mtp" "--spec-draft-n-max" "2"/);
 // Cleanup remains restricted to this exact owned command, never all servers.
 assert.ok(llamaOrphanCleanupScript(profile).includes(command));
 assert.deepEqual(llamaServerArguments({...profile,backend:'claude'}),args);
});
test('native-style disconnect after completed does not mark failure or reload the model',async t=>{
 const upstream=createServer((req,res)=>{req.resume();res.writeHead(200,{'content-type':'text/event-stream'});res.write('data: '+JSON.stringify({type:'response.completed',response:{id:'test',status:'completed',usage:{input_tokens:15,output_tokens:2}}})+'\n\n');setTimeout(()=>res.end(),150).unref();});
 await new Promise(r=>upstream.listen(0,'127.0.0.1',r));
 t.after(()=>{upstream.closeAllConnections();upstream.close();});
 let loads=0,releases=0;const changes=[];
 const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused',broadcast:e=>changes.push(e.state),hostFactory:()=>({start:async()=>{loads++;return {upstream:`http://127.0.0.1:${upstream.address().port}`,sample:async()=>({vramPct:10,ramPct:20,sampledAt:Date.now(),busy:false}),alive:()=>true,release:async()=>{releases++;}};}})});
 t.after(()=>service.shutdown());
 const character={id:'test',backend:'codex',model:profile.model,effort:'default',fastMode:false,permission:'read-only',localProfile:{profile,host}};
 const spec=await service.launch({character,mode:'gui',prompt:'hello',workdir:'/tmp',executable:'/usr/bin/true'});
 t.after(()=>spec.release());
 for(let i=0;i<2;i++){
  const control=new AbortController();
  const response=await fetch(spec.profile.endpoint+'/v1/responses',{method:'POST',headers:{'content-type':'application/json','x-api-key':spec.env[profile.credentialEnv]},body:JSON.stringify({model:profile.model,input:'hello',stream:true}),signal:control.signal});
  assert.equal(response.status,200);const reader=response.body.getReader();const first=await reader.read();assert.match(new TextDecoder().decode(first.value),/response.completed/);control.abort();await reader.cancel().catch(()=>{});await delay(250);
 }
 assert.equal(loads,1);assert.equal(releases,0);assert.equal(changes.includes('error'),false);assert.equal(service.status()[0].state,'ready');
});
