import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { readClaudeCostState } from '../src/claude-cost-state.mjs';

test('resume reads latest matching cost-state across chunk boundaries and malformed trailing lines', t => {
  const dir=mkdtempSync(join(tmpdir(),'claude-cost-'));
  t.after(()=>rmSync(dir,{recursive:true,force:true}));
  const path=join(dir,'session.jsonl');
  const state=(cost,id='session')=>({type:'cost-state',sessionId:id,totalCostUSD:cost,modelUsage:{opus:{costUSD:cost,outputTokens:100}}});
  writeFileSync(path,[state(10),{type:'user',text:'한글'.repeat(40000)},state(39.985636),
    {type:'assistant',text:'긴 내용'.repeat(40000)},state(99,'other'),state(null)].map(x=>JSON.stringify(x)).join('\n')+'\n{"unfinished":');
  assert.deepEqual(readClaudeCostState(path,'session'),{totalCostUsd:39.985636,modelUsage:{opus:{costUSD:39.985636,outputTokens:100}}});
  assert.equal(readClaudeCostState(path,'missing'),null);
  assert.equal(readClaudeCostState(join(dir,'missing'),'session'),null);
  writeFileSync(path,JSON.stringify(state(0)));
  assert.equal(readClaudeCostState(path,'session').totalCostUsd,0);
});
