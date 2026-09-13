// Deliberately scoped to the one verified missing turn. Dry run unless --apply.
import assert from 'node:assert/strict';
import {pool} from '../../backend/src/db.mjs';
import {AgentRuntime,findClaudeSessionPath} from '../../backend/src/agent-runtime.mjs';
import {readClaudeTerminalTurn} from '../../backend/src/terminal-turn-activities.mjs';
const turnID='9a40a347-5adc-4344-90f4-289b5a04ecea';
const {turn}=await fetch('http://127.0.0.1:4317/api/live-feed/'+turnID).then(r=>r.json());
assert.equal(turn.characterId,'right-woman');assert.equal(turn.providerKind,'local');assert.equal(turn.origin,'terminal');assert.equal(turn.status,'completed');
const result=await readClaudeTerminalTurn(findClaudeSessionPath(turn.externalSessionId),{offset:null,sessionID:turn.externalSessionId,startedAt:turn.startedAt,endedAt:turn.endedAt,finalResponse:turn.response,workdir:turn.conversationWorkdir});
assert.ok(result.finalFound);assert.equal(result.usage.inputTokens,15731);assert.equal(result.usage.outputTokens,175);
assert.equal(result.activities.length,1);assert.equal(result.activities[0].kind,'thinking');
const client=await pool.connect();
try {
 await client.query(process.argv.includes('--apply')?'BEGIN':'BEGIN READ ONLY');
 const row=(await client.query('SELECT status,provider_kind,origin,provider_snapshot FROM turns WHERE id=$1'+(process.argv.includes('--apply')?' FOR UPDATE':''),[turnID])).rows[0];
 assert.equal(row.status,'completed');assert.equal(row.provider_kind,'local');assert.equal(row.origin,'terminal');
 const usage=(await client.query('SELECT * FROM usage_records WHERE turn_id=$1',[turnID])).rows;
 const activities=(await client.query('SELECT * FROM turn_activities WHERE turn_id=$1',[turnID])).rows;
 const emptyUsage=usage.length===0||(usage.length===1&&['input_tokens','output_tokens','cached_input_tokens','reasoning_output_tokens','cache_write_input_tokens','cache_write_5m_input_tokens','cache_write_1h_input_tokens'].every(k=>usage[0][k]===null)&&usage[0].cost_usd===null);
 if(!emptyUsage||activities.length) {
  assert.equal(usage.length,1);assert.equal(String(usage[0].input_tokens),'15731');assert.equal(String(usage[0].output_tokens),'175');
  assert.equal(activities.length,1);assert.equal(activities[0].event_key,result.activities[0].eventKey);
  console.log(JSON.stringify({turnID,alreadyRepaired:true}));
 } else if(process.argv.includes('--apply')) {
  await AgentRuntime.prototype.persistUsageRecord.call({},client,{turnID,usage:{...result.usage,reportedCostUsd:usage[0]?.reported_cost_audit?.reportedCostUsd??null},character:{localProfile:row.provider_snapshot}});
  for(const [index,a] of result.activities.entries())await client.query('INSERT INTO turn_activities(turn_id,seq,kind,text,event_key,status,occurred_at) VALUES($1,$2,$3,$4,$5,$6,$7)',[turnID,index+1,a.kind,a.text,a.eventKey,a.status,turn.endedAt]);
  await client.query('UPDATE turns SET updated_at=now() WHERE id=$1',[turnID]);
  console.log(JSON.stringify({turnID,repaired:true,input:15731,output:175,activities:1,cost:null}));
 }else console.log(JSON.stringify({turnID,dryRun:true,missingUsage:true,missingActivities:true,input:15731,output:175,activities:1}));
 await client.query(process.argv.includes('--apply')?'COMMIT':'ROLLBACK');
}catch(error){await client.query('ROLLBACK');throw error;}
finally{client.release();await pool.end();}
