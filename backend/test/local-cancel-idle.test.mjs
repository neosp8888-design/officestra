import test from 'node:test';
import assert from 'node:assert/strict';
import {waitForLlamaIdle,llamaStartupArtifactScript,LLAMA_MODEL_ID} from '../src/local-llama-host.mjs';
import {MEROMERO_MODEL_ID} from '../src/local-model-capabilities.mjs';

test('ordinary startup checks model metadata without a full-file hash pass',()=>{
  for(const model of [LLAMA_MODEL_ID,MEROMERO_MODEL_ID]){
    const script=llamaStartupArtifactScript({model});
    assert.match(script,/Get-Item/);assert.match(script,/\.Length/);
    assert.doesNotMatch(script,/Get-FileHash|SHA256/);
  }
});

test('idle confirmation requires two consecutive samples and rejects inaccessible or active slots',async()=>{
  let checks=0;const states=[true,false,true,false,false];
  await waitForLlamaIdle('http://127.0.0.1:1',{pollMs:1,fetchImpl:async()=>({ok:true,json:async()=>[{is_processing:states[checks++]}]})});
  assert.equal(checks,5);
  await assert.rejects(waitForLlamaIdle('http://127.0.0.1:1',{timeoutMs:10,pollMs:1,fetchImpl:async()=>({ok:true,json:async()=>[{is_processing:true}]})}),/did not stop/);
  for(const slots of [[],{},[{is_processing:'false'}],[{is_processing:false},{is_processing:false}]]){
    await assert.rejects(waitForLlamaIdle('http://127.0.0.1:1',{fetchImpl:async()=>({ok:true,json:async()=>slots})}),/invalid/);
  }
  await assert.rejects(waitForLlamaIdle('http://127.0.0.1:1',{fetchImpl:async()=>({ok:false})}),/unavailable/);
  await assert.rejects(waitForLlamaIdle('http://127.0.0.1:1',{alive:()=>false}),/disconnected/);
});
