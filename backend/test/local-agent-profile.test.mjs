import test from 'node:test';
import assert from 'node:assert/strict';
import { createLocalAgentLaunch, normalizeLocalAgentProfile, localTerminalExecutable } from '../src/local-agent-profile.mjs';
import { buildArguments, executionEnvironment } from '../src/agent-runtime.mjs';

const profile={id:'local-4090',providerKind:'local',backend:'claude',model:'officestra-local-smoke',
  endpoint:'http://127.0.0.1:41235',credentialEnv:'OFFICESTRA_LOCAL_4090_TOKEN',
  credentialVersion:'v1',contextWindow:32768,maxOutputTokens:4096,usageProtocol:'anthropic-normalized-v1'};
const character={id:'isolated-validation',backend:'claude',model:profile.model,effort:'default',
  fastMode:false,permission:'plan',autoCompactPercent:30,identityPrompt:'Keep the identity instructions.'};
const base={PATH:'/usr/bin',OFFICESTRA_LOCAL_4090_TOKEN:'test-local-secret',
  OFFICESTRA_LOCAL_OTHER_TOKEN:'other-secret',ANTHROPIC_API_KEY:'cloud-secret',
  ANTHROPIC_BASE_URL:'https://cloud.example',CLAUDE_CODE_USE_BEDROCK:'1',
  CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:'40',CLAUDE_CODE_MAX_CONTEXT_TOKENS:'200000',
  DISABLE_AUTO_COMPACT:'1',DISABLE_COMPACT:'1',OPENAI_API_KEY:'openai-secret',
  CODEX_HOME:'/tmp/shared-codex-home',OFFICESTRA_PLUGIN_MARKER:'preserved'};
const build=(overrides={})=>createLocalAgentLaunch({profile,character,workdir:'/tmp/local-validation',baseEnvironment:base,executable:'/usr/local/bin/claude',...overrides});
const codexProfile={...profile,id:'local-4090-codex',backend:'codex',
  usageProtocol:'openai-responses-v1'};
const codexCharacter={...character,backend:'codex',permission:'danger-full-access'};
const buildCodex=(overrides={})=>createLocalAgentLaunch({profile:codexProfile,
  character:codexCharacter,workdir:'/tmp/local-validation',baseEnvironment:base,
  executable:'/usr/local/bin/codex',...overrides});

test('PTY executable is absolute and never resolved through relative PATH entries',()=>{
  assert.equal(localTerminalExecutable('/test/codex',{}),'/test/codex');
  assert.equal(localTerminalExecutable('true',{PATH:'.:/usr/bin'}),'/usr/bin/true');
  assert.throws(()=>localTerminalExecutable('codex',{PATH:'.:relative'}),/not found/);
});
test('direct Claude supports the pinned server in GUI and terminal without changing legacy profiles',()=>{
 const direct={...profile,id:'claude-direct',runtime:'llama-cpp-b10982',model:'officestra-qwen38-27b-uncensored-q4km',contextWindow:65536,kvCacheQuantization:'q8_0'};
 for(const mode of ['gui','terminal'])for(const reasoning of ['low','medium','xhigh']){
  const r=build({mode,profile:{...direct,reasoning},character:{...character,model:direct.model}});
  assert.equal(r.profile.reasoning,reasoning);
  assert.equal(r.profile.backend,'claude');
  assert.equal(r.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,'65536');
  assert.equal(r.env.CLAUDE_CODE_MAX_OUTPUT_TOKENS,'4096');
  assert.equal(r.args.includes('--effort'),false);
  assert.ok(r.args.includes('--setting-sources'));
 }
 assert.throws(()=>normalizeLocalAgentProfile({...direct,contextWindow:32768}),/Direct runtime/);
 assert.throws(()=>normalizeLocalAgentProfile({...direct,kvCacheQuantization:'q4_0'}),/Direct runtime/);
});

test('explicit 64K KV8 is preserved for both Codex launch modes; KV4 is rejected',()=>{
  const kv4={...codexProfile,contextWindow:65536,kvCacheQuantization:'q8_0'};
  for(const mode of ['gui','terminal']){
    const result=buildCodex({mode,profile:kv4});
    assert.equal(result.profile.kvCacheQuantization,'q8_0');
    assert.ok(result.args.includes('model_context_window=65536'));
    assert.notEqual(result.signature,buildCodex({mode,profile:{...codexProfile,contextWindow:65536}}).signature);
  }
  assert.equal(normalizeLocalAgentProfile(profile).kvCacheQuantization,undefined);
  for(const invalid of [{...kv4,backend:'claude',usageProtocol:'anthropic-normalized-v1'},
    {...kv4,contextWindow:32768},{...kv4,kvCacheQuantization:'q4_0'},{...kv4,kvCacheQuantization:null}]){
    assert.throws(()=>normalizeLocalAgentProfile(invalid),/KV/);
  }
});

function configArguments(args) {
  const values=new Map();
  for(let i=0;i<args.length;i++)if(args[i]==='-c') {
    const config=String(args[++i]);
    const split=config.indexOf('=');
    values.set(config.slice(0,split),config.slice(split+1));
  }
  return values;
}
test('direct Qwen GUI and terminal display the mirrored summary once without changing selected effort',()=>{
 const direct={...codexProfile,model:'officestra-qwen38-27b-uncensored-q4km',runtime:'llama-cpp-b10982',contextWindow:65536,kvCacheQuantization:'q8_0'};
 for(const mode of ['gui','terminal'])for(const reasoning of ['low','medium','xhigh']){
  const configs=configArguments(buildCodex({mode,profile:{...direct,reasoning},character:{...codexCharacter,model:direct.model}}).args);
  assert.equal(configs.get('show_raw_agent_reasoning'),'false');
  assert.equal(configs.get('hide_agent_reasoning'),'false');
  assert.equal(configs.get('model_reasoning_summary'),'"detailed"');
  assert.equal(configs.get('model_reasoning_effort'),JSON.stringify(reasoning));
 }
});
test('GUI and terminal reasoning selections are pinned in profile and worker identity',()=>{
  for(const mode of ['gui','terminal','persistent']) {
    const on=build({mode,profile:{...profile,reasoning:'on'}});
    const off=build({mode,profile:{...profile,reasoning:'off'}});
    assert.equal(on.profile.reasoning,'on');assert.equal(off.profile.reasoning,'off');
    assert.notEqual(on.signature,off.signature);
    assert.throws(()=>build({mode,profile:{...profile,reasoning:'high'}}));
  }
});

test('local output budget is configurable beyond 4096 but cannot exceed context',()=>{
  assert.equal(
    normalizeLocalAgentProfile({...codexProfile,maxOutputTokens:16384})
      .maxOutputTokens,
    16384,
  );
  for(const maxOutputTokens of [0,65537,1.5,'8192']) {
    assert.throws(()=>normalizeLocalAgentProfile({
      ...codexProfile,
      contextWindow:65536,
      maxOutputTokens,
    }),/output budget/);
  }
});

test('64K launch uses the measured window for GUI and terminal without disabling compaction or tools',()=>{
  for(const mode of ['gui','terminal']){
    const original=build({mode,previousSessionID:'preserved-session'});
    const larger=build({mode,previousSessionID:'preserved-session',profile:{...profile,contextWindow:65536}});
    assert.equal(larger.profile.contextWindow,65536);
    assert.equal(larger.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,'65536');
    assert.equal(larger.env.CLAUDE_CODE_MAX_OUTPUT_TOKENS,'4096');
    assert.equal(larger.env.DISABLE_AUTO_COMPACT,undefined);
    assert.equal(larger.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,'30');
    assert.deepEqual(larger.args,original.args);
    assert.notEqual(larger.signature,original.signature);
  }
});

test('Codex GUI and terminal use only process-local Responses provider overrides',()=>{
  for(const mode of ['gui','terminal']) {
    const result=buildCodex({mode,profile:{...codexProfile,contextWindow:65536}});
    const configs=configArguments(result.args);
    assert.equal(result.backend,'codex');
    assert.equal(result.profile.usageProtocol,'openai-responses-v1');
    assert.equal(result.profile.maxOutputTokens,4096);
    assert.equal(configs.get('model'),JSON.stringify(codexProfile.model));
    assert.equal(configs.get('model_provider'),JSON.stringify('officestra_local'));
    assert.equal(configs.get('model_reasoning_effort'),JSON.stringify('default'));
    assert.equal(configs.get('features.fast_mode'),'false');
    assert.equal(configs.get('service_tier'),JSON.stringify('default'));
    assert.equal(configs.get('model_providers.officestra_local.name'),JSON.stringify('OFFICESTRA Local'));
    assert.equal(configs.get('model_providers.officestra_local.base_url'),JSON.stringify(`${codexProfile.endpoint}/v1`));
    assert.equal(configs.get('model_providers.officestra_local.wire_api'),JSON.stringify('responses'));
    assert.equal(configs.get('model_providers.officestra_local.env_key'),JSON.stringify(codexProfile.credentialEnv));
    assert.equal(configs.get('model_providers.officestra_local.requires_openai_auth'),'false');
    assert.equal(configs.get('model_providers.officestra_local.supports_websockets'),'false');
    assert.equal(configs.get('model_providers.officestra_local.request_max_retries'),'0');
    assert.equal(configs.get('model_providers.officestra_local.stream_max_retries'),'0');
    assert.equal(configs.get('model_context_window'),'65536');
    assert.equal(configs.get('model_auto_compact_token_limit'),'19660');
    assert.equal(configs.get('model_auto_compact_token_limit_scope'),JSON.stringify('total'));
    assert.equal(configs.get('model_reasoning_summary'),'"detailed"');
    assert.equal(configs.get('show_raw_agent_reasoning'),'true');
    assert.equal(configs.get('hide_agent_reasoning'),'false');
    assert.equal(result.args.includes('--ignore-user-config'),false);
    assert.ok([...configs.keys()].some(key=>key==='developer_instructions'));
    if(mode==='terminal')assert.ok([...configs.keys()].some(key=>key==='notify'));
    if(mode==='terminal')assert.equal(result.args[result.args.indexOf('-C')+1],'/tmp/local-validation');
  }
});

test('Codex local credential is isolated while shared Codex home and plugins remain available',()=>{
  const result=buildCodex();
  assert.equal(result.env[codexProfile.credentialEnv],'test-local-secret');
  assert.equal(result.env.OPENAI_API_KEY,undefined);
  assert.equal(result.env.ANTHROPIC_API_KEY,undefined);
  assert.equal(result.env.OFFICESTRA_LOCAL_OTHER_TOKEN,undefined);
  assert.equal(result.env.CODEX_HOME,base.CODEX_HOME);
  assert.equal(result.env.OFFICESTRA_PLUGIN_MARKER,'preserved');
  assert.ok(!JSON.stringify({profile:result.profile,args:result.args,signature:result.signature})
    .includes('test-local-secret'));
});

test('Codex GUI and terminal reuse native fresh and resume argument shapes',()=>{
  const gui=buildCodex({prompt:'--config-looking-user-prompt'});
  assert.equal(gui.args[0],'exec');
  assert.equal(gui.args.at(-1),'--config-looking-user-prompt');
  assert.equal(gui.args.filter(value=>value==='--config-looking-user-prompt').length,1);
  const resumedGUI=buildCodex({previousSessionID:'codex-session'});
  assert.deepEqual(resumedGUI.args.slice(0,4),['exec','resume','codex-session','--json']);
  const terminal=buildCodex({mode:'terminal'});
  assert.notEqual(terminal.args[0],'exec');
  const resumedTerminal=buildCodex({mode:'terminal',previousSessionID:'codex-session'});
  assert.deepEqual(resumedTerminal.args.slice(0,2),['resume','codex-session']);
});

test('persistent mode and mismatched Codex profile contracts fail closed',()=>{
  assert.throws(()=>buildCodex({mode:'persistent'}),/Persistent local Codex/);
  assert.throws(()=>normalizeLocalAgentProfile({...codexProfile,usageProtocol:'anthropic-normalized-v1'}));
  assert.throws(()=>normalizeLocalAgentProfile({...profile,usageProtocol:'openai-responses-v1'}));
  assert.throws(()=>buildCodex({character:{...codexCharacter,permission:'plan'}}));
  assert.throws(()=>buildCodex({character:{...codexCharacter,backend:'claude'}}));
  assert.throws(()=>buildCodex({character:{...codexCharacter,effort:'high'}}));
  assert.throws(()=>buildCodex({character:{...codexCharacter,fastMode:true}}));
});

for(const mode of ['gui','persistent','terminal']) for(const previousSessionID of [null,'test-session']) {
  test(`${mode}: ${previousSessionID?'resume':'fresh'} keeps permissions/identity and local defaults`,()=>{
    const result=build({mode,previousSessionID});
    assert.equal(result.env.ANTHROPIC_BASE_URL,profile.endpoint);
    assert.equal(result.env.CLAUDE_CODE_USE_GATEWAY,'1');
    assert.equal(result.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,'32768');
    assert.equal(result.env.CLAUDE_CODE_MAX_OUTPUT_TOKENS,'4096');
    assert.equal(result.env.API_TIMEOUT_MS,'2147483647');
    assert.equal(result.env.CLAUDE_ENABLE_BYTE_WATCHDOG,'0');
    assert.equal(result.env.CLAUDE_ENABLE_STREAM_WATCHDOG,'0');
    assert.equal(result.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,'30');
    assert.equal(result.env.DISABLE_AUTO_COMPACT,undefined);
    assert.equal(result.env.DISABLE_COMPACT,undefined);
    assert.equal(result.args.includes('--effort'),false);
    assert.equal(result.args[result.args.indexOf('--permission-mode')+1],'plan');
    assert.ok(result.args.some(a=>a.includes('Keep the identity instructions.')));
    assert.equal(result.args.includes('--resume'),previousSessionID!==null);
    if(previousSessionID)assert.equal(result.args[result.args.indexOf('--resume')+1],previousSessionID);
    assert.equal(result.args.includes('-p'),mode!=='terminal');
    const settings=JSON.parse(result.args[result.args.indexOf('--settings')+1]);
    assert.equal(settings.fastMode,false);
    if(mode==='terminal') {
      assert.ok(settings.hooks.UserPromptSubmit);
      assert.ok(settings.hooks.Stop);
      assert.ok(settings.statusLine);
    } else assert.equal(result.args[result.args.indexOf('--prompt-suggestions')+1],'false');
  });
}

test('credentials are isolated to the spawned environment; parent and other providers unchanged',()=>{
  const before=structuredClone(base), c=structuredClone(character);
  const result=build();
  assert.equal(result.env.ANTHROPIC_API_KEY,'test-local-secret');
  assert.equal(result.env.CLAUDE_CODE_USE_BEDROCK,undefined);
  assert.equal(result.env.OPENAI_API_KEY,undefined);
  assert.equal(result.env.OFFICESTRA_LOCAL_OTHER_TOKEN,undefined);
  assert.equal(result.env.OFFICESTRA_LOCAL_4090_TOKEN,undefined);
  assert.deepEqual(base,before);assert.deepEqual(character,c);
  assert.ok(!JSON.stringify({profile:result.profile,args:result.args,signature:result.signature}).includes('test-local-secret'));
  assert.equal(executionEnvironment({...character,backend:'codex'},base),base);
});

test('profile changes invalidate worker signature without secret leakage',()=>{
  const original=build().signature;
  for(const patch of [{credentialVersion:'v2'},{endpoint:'http://127.0.0.1:41236'},{id:'local-second'},{credentialEnv:'OFFICESTRA_LOCAL_OTHER_TOKEN'}]) {
    assert.notEqual(build({profile:{...profile,...patch}}).signature,original);
  }
  assert.notEqual(build({workdir:'/tmp/other'}).signature,original);
  assert.notEqual(build({executable:'/other/claude'}).signature,original);
  assert.notEqual(build({character:{...character,identityPrompt:'changed'}}).signature,original);
  assert.equal(build({baseEnvironment:{...base,OFFICESTRA_LOCAL_4090_TOKEN:'rotated'}}).signature,original);
});

test('invalid endpoints, secret-bearing configs, unverified context and absent credentials fail closed',()=>{
  for(const endpoint of ['https://example.com','http://localhost:1234','http://127.0.0.1','http://u:p@127.0.0.1:1234','http://127.0.0.1:1234/?token=x']) {
    assert.throws(()=>normalizeLocalAgentProfile({...profile,endpoint}));
  }
  assert.throws(()=>normalizeLocalAgentProfile({...profile,apiKey:'secret'}));
  assert.throws(()=>normalizeLocalAgentProfile({...profile,contextWindow:200000}));
  assert.throws(()=>normalizeLocalAgentProfile({...profile,usageProtocol:'raw-lmstudio'}));
  assert.throws(()=>build({baseEnvironment:{}}));
  assert.throws(()=>build({character:{...character,backend:'codex'}}));
  assert.throws(()=>build({character:{...character,model:'other'}}));
  assert.throws(()=>build({character:{...character,effort:'high'}}));
  assert.throws(()=>build({character:{...character,fastMode:true}}));
});

test('existing cloud argument builder is unchanged',()=>{
  const args=buildArguments({character:{...character,model:'claude-opus-5',effort:'high',fastMode:true},prompt:'hello'});
  assert.equal(args[args.indexOf('--effort')+1],'high');
  assert.equal(args[args.indexOf('--prompt-suggestions')+1],'true');
  assert.equal(JSON.parse(args[args.indexOf('--settings')+1]).fastMode,true);
});

test('prompt and identity text that resemble CLI flags are preserved byte-for-byte',()=>{
  for(const prompt of ['--effort','--prompt-suggestions','--settings','--model']) {
    const result=build({prompt,character:{...character,identityPrompt:'--effort'}});
    assert.equal(result.args[0],'-p');assert.equal(result.args[1],prompt);
    assert.equal(result.args[result.args.indexOf('--permission-mode')+1],'plan');
    assert.ok(result.args[result.args.indexOf('--append-system-prompt')+1].startsWith('--effort'));
  }
});
