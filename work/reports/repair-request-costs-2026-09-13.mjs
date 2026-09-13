// Explicit, reversible repair of five verified completed turns; dry-run by default.
import assert from "node:assert/strict";
import { pool } from "../../backend/src/db.mjs";
import { codexRolloutTurnUsage } from "../../backend/src/terminal-usage.mjs";
import { estimateTurnTokenCost } from "../../backend/src/token-cost-estimator.mjs";
import { findCodexRolloutPath } from "../../backend/src/codex-rollout-turns.mjs";

const expected = new Map([
  ["eb4962ec-3bb8-45b0-8c64-73ca3a9d9157", "7.17211400"],
  ["5b3699e8-d70e-4447-ba68-a3365bd1d088", "4.00945000"],
  ["0df53f10-145d-401f-a0a0-5080973d5dfa", "3.98956000"],
  ["b08a85c7-1269-4004-9167-456a1a66d533", "4.39966300"],
  ["ba00e328-8d43-4f74-ba4b-c9a7a85a2191", "7.11639400"],
]);
const apply = process.argv.includes("--apply");
const client = await pool.connect();
try {
  await client.query("BEGIN READ ONLY");
  const { rows } = await client.query(`
    SELECT t.id,t.backend,t.model,t.fast_mode,t.started_at,t.ended_at,s.external_id,
      u.input_tokens,u.output_tokens,u.cached_input_tokens,u.cache_write_input_tokens,u.cost_usd
    FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
      JOIN usage_records u ON u.turn_id=t.id
    WHERE t.id=ANY($1::uuid[]) AND t.status='completed'
    ORDER BY t.started_at
  `, [[...expected.keys()]]);
  await client.query("ROLLBACK");
  assert.equal(rows.length, expected.size);
  const results = [];
  for (const row of rows) {
    assert.equal(row.cost_usd, expected.get(row.id), "Unexpected current cost; stop before writes");
    assert.equal(row.backend, "codex");
    const recorded = await codexRolloutTurnUsage(await findCodexRolloutPath(row.external_id), {
      startedAt: row.started_at, endedAt: row.ended_at, scope: "window",
    });
    assert.ok(recorded?.requestUsages?.length);
    for (const [db, cli] of Object.entries({
      input_tokens:"inputTokens",output_tokens:"outputTokens",
      cached_input_tokens:"cachedInputTokens",cache_write_input_tokens:"cacheWriteInputTokens",
    })) assert.equal(BigInt(row[db] ?? 0), BigInt(recorded[cli] ?? 0), db);
    const cost = estimateTurnTokenCost({
      backend:row.backend,model:row.model,fastMode:row.fast_mode,
      pricedAt:row.started_at,usage:recorded,
    });
    assert.ok(Number.isFinite(cost) && cost >= 0 && cost < Number(row.cost_usd));
    results.push({id:row.id,previousCost:row.cost_usd,correctedCost:cost.toFixed(8),
      requests:recorded.requestUsages.length,
      maxRequestInput:Math.max(...recorded.requestUsages.map(r=>r.usage.inputTokens)),
      inputTokens:row.input_tokens,outputTokens:row.output_tokens,
      cachedInputTokens:row.cached_input_tokens,cacheWriteInputTokens:row.cache_write_input_tokens});
  }
  if (apply) {
    await client.query("BEGIN");
    for (const result of results) {
      const changed = await client.query(`
        UPDATE usage_records SET cost_usd=$1
        WHERE turn_id=$2 AND cost_usd=$3 AND input_tokens=$4 AND output_tokens=$5
          AND cached_input_tokens=$6 AND cache_write_input_tokens=$7
        RETURNING turn_id,cost_usd
      `, [result.correctedCost,result.id,result.previousCost,result.inputTokens,
        result.outputTokens,result.cachedInputTokens,result.cacheWriteInputTokens]);
      assert.equal(changed.rowCount,1,"Concurrent change; roll back entire repair");
    }
    await client.query("COMMIT");
  }
  console.log(JSON.stringify({applied:apply,results},null,2));
} finally {
  await client.query("ROLLBACK").catch(()=>{});
  client.release();
  await pool.end();
}

