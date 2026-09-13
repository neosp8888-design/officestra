// 사용량이 비어 있는 터미널 턴을 CLI 기록에서 다시 계산해 채운다.
// --apply 없이 실행하면 계산 결과만 보여주고 DB에는 쓰지 않는다.

import { pool, withTransaction } from "./db.mjs";
import { findClaudeSessionPath } from "./agent-runtime.mjs";
import { findCodexRolloutPath } from "./codex-rollout-turns.mjs";
import { estimateTurnTokenCost } from "./token-cost-estimator.mjs";
import {
  claudeTranscriptTurnUsage,
  codexRolloutTurnUsage,
} from "./terminal-usage.mjs";

const MISSING_USAGE_TURNS = `
  SELECT
    turn.id,
    turn.backend,
    turn.model,
    turn.fast_mode AS "fastMode",
    turn.started_at AS "startedAt",
    turn.ended_at AS "endedAt",
    session.external_id AS "externalSessionID",
    character.id AS "characterID",
    character.name AS "characterName"
  FROM turns AS turn
  JOIN cli_sessions AS session ON session.id = turn.cli_session_id
  JOIN characters AS character ON character.id = session.character_id
  LEFT JOIN usage_records AS usage ON usage.turn_id = turn.id
  WHERE turn.origin = 'terminal'
    AND turn.status = 'completed'
    AND usage.turn_id IS NULL
  ORDER BY turn.started_at
`;

export async function terminalTurnUsageBackfill({ apply = false } = {}) {
  const { rows } = await pool.query(MISSING_USAGE_TURNS);
  const entries = [];
  for (const row of rows) {
    const usage = await recordedUsage(row);
    entries.push({
      turn: row,
      usage,
      skipReason: skipReason(row, usage),
      costUsd: usage
        ? estimateTurnTokenCost({
          backend: row.backend,
          model: row.model,
          fastMode: row.fastMode,
          usage,
          pricedAt: row.startedAt,
        })
        : null,
    });
  }
  const applied = [];
  if (apply) {
    for (const entry of entries) {
      if (entry.skipReason) continue;
      await withTransaction(async (client) => {
        await insertUsageRecord(client, entry);
      });
      applied.push(entry.turn.id);
    }
  }
  return { entries, applied };
}

async function recordedUsage(row) {
  const window = { startedAt: row.startedAt, endedAt: row.endedAt };
  if (!row.externalSessionID || !row.startedAt) {
    return null;
  }
  try {
    if (row.backend === "codex") {
      return await codexRolloutTurnUsage(
        await findCodexRolloutPath(row.externalSessionID),
        window,
      );
    }
    if (row.backend === "claude") {
      return await claudeTranscriptTurnUsage(
        findClaudeSessionPath(row.externalSessionID),
        window,
      );
    }
  } catch (error) {
    console.warn(
      `사용량을 읽지 못했습니다(${row.id}).`,
      error instanceof Error ? error.message : String(error),
    );
  }
  return null;
}

// 억지로 채우지 않는다. 기록을 못 찾았거나 토큰이 하나도 없으면 건너뛴다.
function skipReason(row, usage) {
  if (row.backend === "antigravity") {
    return "Antigravity 터미널 턴은 CLI가 사용량을 직접 남긴다";
  }
  if (!usage) {
    return "CLI 기록에서 이 턴 구간의 사용량을 찾지 못했다";
  }
  const total = (usage.inputTokens ?? 0) + (usage.outputTokens ?? 0) +
    (usage.cachedInputTokens ?? 0) + (usage.cacheWriteInputTokens ?? 0);
  return total > 0 ? null : "구간에서 읽은 토큰이 모두 0이다";
}

// 이미 채워진 턴은 건드리지 않으므로 여러 번 실행해도 행이 늘지 않는다.
async function insertUsageRecord(client, { turn, usage, costUsd }) {
  await client.query(
    `
      INSERT INTO usage_records (
        turn_id,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        reasoning_output_tokens,
        cost_usd,
        cache_write_input_tokens,
        cache_write_5m_input_tokens,
        cache_write_1h_input_tokens
      )
      VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
      ON CONFLICT (turn_id) DO NOTHING
    `,
    [
      turn.id,
      usage.inputTokens,
      usage.outputTokens,
      usage.cachedInputTokens,
      usage.reasoningOutputTokens,
      costUsd,
      usage.cacheWriteInputTokens,
      usage.cacheWrite5mInputTokens,
      usage.cacheWrite1hInputTokens,
    ],
  );
}

function formatted(value) {
  return value === null || value === undefined
    ? "-"
    : Number(value).toLocaleString("en-US");
}

async function main() {
  const apply = process.argv.includes("--apply");
  const { entries, applied } = await terminalTurnUsageBackfill({ apply });
  for (const entry of entries) {
    const { turn, usage, costUsd, skipReason: reason } = entry;
    console.log(
      [
        turn.id.slice(0, 8),
        turn.characterName,
        turn.backend,
        turn.model,
        reason ? `제외 — ${reason}` : [
          `입력 ${formatted(usage.inputTokens)}`,
          `출력 ${formatted(usage.outputTokens)}`,
          `캐시읽기 ${formatted(usage.cachedInputTokens)}`,
          `캐시쓰기 ${formatted(usage.cacheWriteInputTokens)}`,
          `추론 ${formatted(usage.reasoningOutputTokens)}`,
          `$${(costUsd ?? 0).toFixed(4)}`,
        ].join(" · "),
      ].join(" | "),
    );
  }
  console.log(
    apply
      ? `저장 ${applied.length}건 / 대상 ${entries.length}건`
      : `계산만 함 — 대상 ${entries.length}건, 저장하려면 --apply`,
  );
  await pool.end();
}

if (import.meta.url === `file://${process.argv[1]}`) {
  await main();
}
