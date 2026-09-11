import test from 'node:test';
import assert from 'node:assert/strict';
import { createLocalAgentLaunch, normalizeLocalAgentProfile } from '../src/local-agent-profile.mjs';
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
  DISABLE_AUTO_COMPACT:'1',DISABLE_COMPACT:'1',OPENAI_API_KEY:'openai-secret'};
const build=(overrides={})=>createLocalAgentLaunch({profile,character,workdir:'/tmp/local-validation',baseEnvironment:base,executable:'/usr/local/bin/claude',...overrides});

test('64K launch uses the measured window for GUI and terminal without disabling compaction or tools',()=>{
  for(const mode of ['gui','terminal']){
    const original=build({mode,previousSessionID:'preserved-session'});
    const larger=build({mode,previousSessionID:'preserved-session',profile:{...profile,contextWindow:65536}});
    assert.equal(larger.profile.contextWindow,65536);
    assert.equal(larger.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,'65536');
    assert.equal(larger.env.CLAUDE_CODE_MAX_OUTPUT_TOKENS,'4096');
    assert.equal(larger.env.DISABLE_AUTO_COMPACT,undefined);
    assert.equal(larger.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,undefined);
    assert.deepEqual(larger.args,original.args);
    assert.notEqual(larger.signature,original.signature);
  }
});

for(const mode of ['gui','persistent','terminal']) for(const previousSessionID of [null,'test-session']) {
  test(`${mode}: ${previousSessionID?'resume':'fresh'} keeps permissions/identity and local defaults`,()=>{
    const result=build({mode,previousSessionID});
    assert.equal(result.env.ANTHROPIC_BASE_URL,profile.endpoint);
    assert.equal(result.env.CLAUDE_CODE_USE_GATEWAY,'1');
    assert.equal(result.env.CLAUDE_CODE_MAX_CONTEXT_TOKENS,'32768');
    assert.equal(result.env.CLAUDE_CODE_MAX_OUTPUT_TOKENS,'4096');
    assert.equal(result.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,undefined);
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
