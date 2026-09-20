import test from 'node:test';
import assert from 'node:assert/strict';
import {MEROMERO_MODEL_ID,MEROMERO_HOST_KEY,MEROMERO_ARTIFACT,MEROMERO_VISION_ARTIFACT,MEROMERO_26B_MODEL_ID,MEROMERO_26B_HOST_KEY,MEROMERO_26B_ARTIFACT,MEROMERO_26B_VISION_ARTIFACT} from '../src/local-model-capabilities.mjs';
import {normalizeLocalAgentProfile,createLocalAgentLaunch} from '../src/local-agent-profile.mjs';
import {llamaServerArguments,llamaArtifactVerificationScript,llamaStartupArtifactScript,WindowsLlamaCppHost} from '../src/local-llama-host.mjs';
import {LocalProviderService,localCodexModelCatalog,localProfileTitle,localProfileReasoningOptions,localProfileDefaultReasoning,localResumeCompatible} from '../src/local-provider-service.mjs';
import {normalizeLocalResponsesRequest,normalizeLlamaMessagesRequest} from '../src/local-inference-bridge.mjs';
import {LLAMA_RESPONSES_PROFILE} from '../src/local-responses-usage.mjs';
const profile={id:'mero-test',providerKind:'local',backend:'codex',runtime:'llama-cpp-b10982',model:MEROMERO_MODEL_ID,endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:32768,maxOutputTokens:8192,usageProtocol:'openai-responses-v1',kvCacheQuantization:'q8_0',reasoning:'default'};
const host={address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/test-key',hostKeyAlias:'test',modelKey:MEROMERO_HOST_KEY,comfyPort:8188};
test('26B uses its pinned GPU projector and an image-compatible microbatch, isolated from 31B',()=>{
 const p={...profile,model:MEROMERO_26B_MODEL_ID,contextWindow:65536},h={...host,modelKey:MEROMERO_26B_HOST_KEY};
 assert.equal(normalizeLocalAgentProfile(p).contextWindow,65536);
 const args=llamaServerArguments(p);
 assert.equal(args[args.indexOf('-m')+1],MEROMERO_26B_ARTIFACT.root+'\\'+MEROMERO_26B_ARTIFACT.file);
 assert.equal(args[args.indexOf('--mmproj')+1],MEROMERO_26B_VISION_ARTIFACT.root+'\\'+MEROMERO_26B_VISION_ARTIFACT.file);
 assert.ok(args.includes('--mmproj-offload'));assert.equal(args.includes('--no-mmproj-offload'),false);
 assert.equal(args[args.indexOf('-b')+1],'1024');assert.equal(args[args.indexOf('-ub')+1],'1024');
 assert.equal(args[args.indexOf('-t')+1],'4');assert.equal(args[args.indexOf('-ngl')+1],'all');
 assert.ok(llamaArtifactVerificationScript(p).includes(MEROMERO_26B_VISION_ARTIFACT.sha256));
 assert.ok(llamaStartupArtifactScript(p).includes(String(MEROMERO_26B_ARTIFACT.size)));
 assert.match(localProfileTitle({profile:p,host:h}),/26B A4B/);
 assert.equal(localResumeCompatible({profile,host},{profile:p,host:h}),false);
 assert.throws(()=>new WindowsLlamaCppHost({profile:p,host,stateDirectory:'/tmp/unused-26b'}),/model key/);
 for(const backend of ['claude','codex'])for(const mode of ['gui','terminal']){
  const selected={...p,backend,reasoning:'on',usageProtocol:backend==='claude'?'anthropic-normalized-v1':'openai-responses-v1'};
  const spec=createLocalAgentLaunch({profile:selected,mode,character:{backend,model:p.model,effort:'default',permission:backend==='codex'?'read-only':'plan',fastMode:false},workdir:'/tmp',prompt:'hello',executable:'/usr/bin/true',baseEnvironment:{[p.credentialEnv]:'test-token'}});
  assert.equal(spec.profile.model,p.model);
  assert.deepEqual(localProfileReasoningOptions({profile:selected,host:h}),['default','off','on']);
 }
 assert.equal(normalizeLocalResponsesRequest({model:p.model},'on',LLAMA_RESPONSES_PROFILE).chat_template_kwargs.enable_thinking,true);
 assert.equal(normalizeLlamaMessagesRequest({model:p.model},'off').chat_template_kwargs.enable_thinking,false);
 assert.deepEqual(localCodexModelCatalog(p).models[0].input_modalities,['text','image']);
});
test('31B stays text-only on GPU and never requires or loads a projector',()=>{
 assert.equal(normalizeLocalAgentProfile(profile).contextWindow,32768);
 for(const bad of [{contextWindow:131072},{reasoning:'xhigh'},{kvCacheQuantization:'q4_0'}])assert.throws(()=>normalizeLocalAgentProfile({...profile,...bad}));
 const args=llamaServerArguments(profile);
 assert.equal(args[args.indexOf('-m')+1],MEROMERO_ARTIFACT.root+'\\'+MEROMERO_ARTIFACT.file);
 assert.equal(args.includes('--mmproj'),false);
 assert.equal(args.includes('--no-mmproj-offload'),false);assert.equal(args[args.indexOf('-ngl')+1],'all');assert.equal(args.includes('draft-mtp'),false);
 const verification=llamaArtifactVerificationScript(profile);
 assert.ok(verification.includes(MEROMERO_ARTIFACT.sha256));assert.ok(!verification.includes('Qwen'));
 assert.ok(!verification.includes(MEROMERO_VISION_ARTIFACT.sha256));
 assert.ok(!llamaStartupArtifactScript(profile).includes(String(MEROMERO_VISION_ARTIFACT.size)));
 assert.throws(()=>new WindowsLlamaCppHost({profile,host:{...host,modelKey:'qwen3.8-27b'},stateDirectory:'/tmp/unused-mero'}),/model key/);
 assert.equal(localResumeCompatible({profile,host},{profile:{...profile,model:'officestra-qwen38-27b-uncensored-q4km'},host}),false);
});
test('31B menu and Codex catalog expose native thinking toggle and text-only modality',()=>{
 const d={profile,host};
 assert.deepEqual(localProfileReasoningOptions(d),['default','off','on']);assert.equal(localProfileDefaultReasoning(d),'off');
 assert.match(localProfileTitle(d),/MeroMero/);assert.doesNotMatch(localProfileTitle(d),/MTP/);
 const m=localCodexModelCatalog(profile).models[0];
 assert.deepEqual(m.input_modalities,['text']);assert.equal(m.default_reasoning_level,'none');assert.deepEqual(m.supported_reasoning_levels.map(v=>v.effort),['none','medium']);
});
test('both runners and GUI/terminal launch with the selected model and native reasoning',()=>{
 for(const contextWindow of [32768,65536])for(const backend of ['claude','codex'])for(const mode of ['gui','terminal'])for(const reasoning of ['default','off','on']){
  const p={...profile,contextWindow,backend,reasoning,usageProtocol:backend==='codex'?'openai-responses-v1':'anthropic-normalized-v1'};
  const spec=createLocalAgentLaunch({profile:p,mode,character:{backend,model:p.model,effort:'default',permission:backend==='codex'?'read-only':'plan',fastMode:false},workdir:'/tmp',prompt:'hello',executable:'/usr/bin/true',baseEnvironment:{[p.credentialEnv]:'test-token'}});
  assert.ok(spec.args.includes(p.model)||spec.args.includes(`model="${p.model}"`));
  if(backend==='codex')assert.ok(spec.args.includes(`model_reasoning_effort="${reasoning==='on'?'medium':'none'}"`));
  else {assert.equal(spec.env.ANTHROPIC_DEFAULT_SONNET_MODEL,p.model);assert.equal(spec.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,String(contextWindow));}
  const args=llamaServerArguments(p);assert.equal(args[args.indexOf('-c')+1],String(contextWindow));
  assert.equal(localCodexModelCatalog(p).models[0].context_window,contextWindow);
 }
});
test('verified MeroMero context increase preserves session identity, reasoning and KV8',()=>{
 const previous={profile,host},next={profile:{...profile,contextWindow:65536},host};
 assert.equal(normalizeLocalAgentProfile(next.profile).kvCacheQuantization,'q8_0');
 assert.equal(localResumeCompatible(previous,next),true);
 assert.equal(localResumeCompatible(next,previous),false);
});
test('Responses and Messages use enable_thinking, strip cloud settings, and reject fake levels',()=>{
 for(const r of ['default','off','on']){
  const enabled=r==='on';
  const response=normalizeLocalResponsesRequest({model:profile.model,service_tier:'priority',chat_template_kwargs:{reasoning_effort:'xhigh'}},r,LLAMA_RESPONSES_PROFILE,32768);
  assert.deepEqual(response.chat_template_kwargs,{enable_thinking:enabled});assert.equal(response.reasoning.effort,enabled?'medium':'none');assert.equal(response.service_tier,undefined);
  const message=normalizeLlamaMessagesRequest({model:profile.model,thinking:{type:'adaptive'},output_config:{effort:'high'}},r);
  assert.deepEqual(message.chat_template_kwargs,{enable_thinking:enabled});assert.equal(message.thinking,undefined);assert.equal(message.output_config,undefined);
 }
 assert.equal(normalizeLocalResponsesRequest({model:profile.model,reasoning:{effort:'medium'}},'off',LLAMA_RESPONSES_PROFILE).chat_template_kwargs.enable_thinking,true);
 assert.throws(()=>normalizeLocalResponsesRequest({model:profile.model,reasoning:{effort:'xhigh'}},'off',LLAMA_RESPONSES_PROFILE),/MeroMero/);
 assert.throws(()=>normalizeLlamaMessagesRequest({model:profile.model},'medium'),/Unsupported/);
});

test('Claude image tool results retain text and IDs while images reach Gemma vision, including replayed parallel calls',()=>{
 const image=n=>({type:'image',source:{type:'base64',media_type:'image/png',data:n}});
 const request={model:MEROMERO_26B_MODEL_ID,messages:[{role:'user',content:[
  {type:'tool_result',tool_use_id:'one',is_error:false,content:[{type:'text',text:'original tool text'},image('a'),image('b')]},
  {type:'tool_result',tool_use_id:'two',content:[image('c')]},
 ]},{role:'user',content:'Continue.'}]};
 const before=structuredClone(request),out=normalizeLlamaMessagesRequest(request,'off');
 assert.deepEqual(request,before);
 assert.equal(out.messages[0].content[0].tool_use_id,'one');assert.equal(out.messages[0].content[0].is_error,false);
 assert.equal(out.messages[0].content[0].content[0].text,'original tool text');
 assert.equal(out.messages[0].content[1].tool_use_id,'two');
 assert.deepEqual(out.messages[1].content.filter(b=>b.type==='image'),[image('a'),image('b'),image('c')]);
 assert.equal(out.messages[2].content,'Continue.');
 assert.deepEqual(normalizeLlamaMessagesRequest(out,'off'),out);
});

test('31B rejects new image inputs and tool images, but resumes old image histories without pixels',()=>{
 for(const responses of [false,true]){
  const image=responses?{type:'input_image',image_url:'data:image/png;base64,secret'}:{type:'image',source:{type:'base64',media_type:'image/png',data:'secret'}};
  const normalize=value=>responses?normalizeLocalResponsesRequest(value,'off',LLAMA_RESPONSES_PROFILE):normalizeLlamaMessagesRequest(value,'off');
  const key=responses?'input':'messages';
  const current={model:profile.model,[key]:[{role:'user',content:[image]}]};
  assert.throws(()=>normalize(current),error=>error.status===400&&/31B.*텍스트 전용.*26B/.test(error.message));
  const tool=responses?{type:'function_call_output',call_id:'one',output:[image]}:{role:'user',content:[{type:'tool_result',tool_use_id:'one',content:[image]}]};
  assert.throws(()=>normalize({model:profile.model,[key]:[{role:'user',content:'Read image'},tool]}),/텍스트 전용/);
  const historical={...current,[key]:[...current[key],tool,{role:'assistant',content:'Earlier answer'},{role:'user',content:'이제 텍스트로 계속하자.'}]};
  const before=structuredClone(historical),result=normalize(historical);
  assert.deepEqual(historical,before);assert.doesNotMatch(JSON.stringify(result),/secret|input_image|"type":"image"/);
  assert.match(JSON.stringify(result),/이전 이미지 생략/);
 }
});

test('31B image attachments fail before loading GPU resources for either runner',async()=>{
 const service=new LocalProviderService({pool:{},stateDirectory:'/tmp/unused-mero-attachments',hostFactory:()=>{throw Error('Must not load GPU');}});
 try{
  for(const backend of ['claude','codex'])for(const mode of ['gui','terminal']){
   const p={...profile,backend,usageProtocol:backend==='claude'?'anthropic-normalized-v1':'openai-responses-v1'};
   await assert.rejects(service.launch({character:{backend,localProfile:{profile:p,host}},mode,attachments:[{path:'/tmp/image.webp'}]}),/31B.*텍스트 전용.*26B/);
  }
  assert.equal(service.entries.size,0);
 }finally{await service.shutdown();}
});
