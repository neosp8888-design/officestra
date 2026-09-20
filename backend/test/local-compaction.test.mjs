import test from 'node:test';
import assert from 'node:assert/strict';
import {createLocalAgentLaunch} from '../src/local-agent-profile.mjs';
import {localCodexModelCatalog} from '../src/local-provider-service.mjs';
import {normalizeAutoCompactPercent} from '../src/agent-runtime.mjs';
import {localSelectionSettings} from '../src/local-profile-selection.mjs';

test('local 100 percent reaches Claude/Codex GUI and terminal while cloud normalization stays capped',()=>{
  assert.equal(normalizeAutoCompactPercent(100),95);
  assert.equal(normalizeAutoCompactPercent(100,100),100);
  for(const backend of ['claude','codex'])for(const mode of ['gui','terminal']){
    const profile={id:'full-context',providerKind:'local',backend,model:'test-model',endpoint:'http://127.0.0.1:40001',credentialEnv:'OFFICESTRA_LOCAL_TEST_TOKEN',credentialVersion:'v1',contextWindow:65536,maxOutputTokens:8192,usageProtocol:backend==='claude'?'anthropic-normalized-v1':'openai-responses-v1'};
    const character={id:'test',backend,model:profile.model,effort:'default',fastMode:false,permission:backend==='claude'?'plan':'read-only',autoCompactPercent:100};
    const options={profile,character,mode,executable:'/test/'+backend,workdir:'/tmp',baseEnvironment:{OFFICESTRA_LOCAL_TEST_TOKEN:'test'}};
    const launch=createLocalAgentLaunch(options);
    if(backend==='claude')assert.equal(launch.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,'100');
    else{
      assert.ok(launch.args.includes('model_auto_compact_token_limit=65536'));
      assert.equal(localCodexModelCatalog(profile).models[0].effective_context_window_percent,100);
    }
    assert.notEqual(launch.signature,createLocalAgentLaunch({...options,character:{...character,autoCompactPercent:95}}).signature);
    const fallback=createLocalAgentLaunch({...options,character:{...character,autoCompactPercent:undefined}});
    if(backend==='claude')assert.equal(fallback.env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE,'100');
    else assert.ok(fallback.args.includes('model_auto_compact_token_limit=65536'));
  }
});
test('selecting local defaults to 100 and returning to cloud restores its original threshold',()=>{
  const cloud={backend:'codex',model:'cloud',effort:'high',permission:'read-only',fastMode:false,autoCompactPercent:30,config:{}};
  const local=localSelectionSettings(cloud,{profile:{id:'local-test',backend:'codex',model:'local'}});
  assert.equal(local.autoCompactPercent,100);
  assert.equal(localSelectionSettings(local,null).autoCompactPercent,30);
});
