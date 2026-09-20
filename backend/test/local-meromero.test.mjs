import test from 'node:test';
import assert from 'node:assert/strict';
import {MEROMERO_MODEL_ID,MEROMERO_HOST_KEY,MEROMERO_ARTIFACT} from '../src/local-model-capabilities.mjs';
import {normalizeLocalAgentProfile,createLocalAgentLaunch} from '../src/local-agent-profile.mjs';
import {llamaServerArguments,llamaArtifactVerificationScript,WindowsLlamaCppHost} from '../src/local-llama-host.mjs';
import {localCodexModelCatalog,localProfileTitle,localProfileReasoningOptions,localProfileDefaultReasoning,localResumeCompatible} from '../src/local-provider-service.mjs';
import {normalizeLocalResponsesRequest,normalizeLlamaMessagesRequest} from '../src/local-inference-bridge.mjs';
import {LLAMA_RESPONSES_PROFILE} from '../src/local-responses-usage.mjs';
const profile={id:'mero-test',providerKind:'local',backend:'codex',runtime:'llama-cpp-b10982',model:MEROMERO_MODEL_ID,endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:32768,maxOutputTokens:8192,usageProtocol:'openai-responses-v1',kvCacheQuantization:'q8_0',reasoning:'default'};
const host={address:'127.0.0.1',user:'test',sshPort:2222,keyPath:'/tmp/test-key',hostKeyAlias:'test',modelKey:MEROMERO_HOST_KEY,comfyPort:8188};
test('MeroMero pins its verified artifact/context and cannot inherit Qwen MTP or vision',()=>{
 assert.equal(normalizeLocalAgentProfile(profile).contextWindow,32768);
 for(const bad of [{contextWindow:131072},{reasoning:'xhigh'},{kvCacheQuantization:'q4_0'}])assert.throws(()=>normalizeLocalAgentProfile({...profile,...bad}));
 const args=llamaServerArguments(profile);
 assert.equal(args[args.indexOf('-m')+1],MEROMERO_ARTIFACT.root+'\\'+MEROMERO_ARTIFACT.file);
 assert.equal(args.includes('--mmproj'),false);assert.equal(args.includes('draft-mtp'),false);
 const verification=llamaArtifactVerificationScript(profile);
 assert.ok(verification.includes(MEROMERO_ARTIFACT.sha256));assert.ok(!verification.includes('Qwen'));
 assert.throws(()=>new WindowsLlamaCppHost({profile,host:{...host,modelKey:'qwen3.8-27b'},stateDirectory:'/tmp/unused-mero'}),/model key/);
 assert.equal(localResumeCompatible({profile,host},{profile:{...profile,model:'officestra-qwen38-27b-uncensored-q4km'},host}),false);
});
test('MeroMero menu and Codex catalog expose native thinking toggle and text modality',()=>{
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
