// 실제 모델 ID가 비어 있는 Claude 턴을 세션 기록에서 읽어 채운다.
// --apply 없이 실행하면 읽은 결과만 보여주고 DB에는 쓰지 않는다.

import { pool } from "./db.mjs";
import { findClaudeSessionPath } from "./agent-runtime.mjs";
import { claudeTranscriptTurn } from "./terminal-usage.mjs";

const MISSING_MODEL_TURNS = `
  SELECT
    turn.id,
    turn.model,
    turn.started_at AS "startedAt",
    turn.ended_at AS "endedAt",
    session.external_id AS "externalSessionID",
    character.name AS "characterName"
  FROM turns AS turn
  JOIN cli_sessions AS session ON session.id = turn.cli_session_id
  JOIN characters AS character ON character.id = session.character_id
  WHERE turn.backend = 'claude'
    AND turn.resolved_model IS NULL
  ORDER BY turn.started_at
`;

export async function turnResolvedModelBackfill({ apply = false } = {}) {
  const { rows } = await pool.query(MISSING_MODEL_TURNS);
  const entries = [];
  for (const row of rows) {
    entries.push({ turn: row, resolvedModel: await recordedModel(row) });
  }
  const applied = [];
  if (apply) {
    for (const { turn, resolvedModel } of entries) {
      if (!resolvedModel) continue;
      // 그사이 런타임이 먼저 채웠으면 덮어쓰지 않는다.
      const { rowCount } = await pool.query(
        "UPDATE turns SET resolved_model = $2 WHERE id = $1 AND resolved_model IS NULL",
        [turn.id, resolvedModel],
      );
      if (rowCount) applied.push(turn.id);
    }
  }
  return { entries, applied };
}

async function recordedModel(row) {
  if (!row.externalSessionID || !row.startedAt) {
    return null;
  }
  try {
    const { model } = await claudeTranscriptTurn(
      findClaudeSessionPath(row.externalSessionID),
      { startedAt: row.startedAt, endedAt: row.endedAt },
    );
    return model;
  } catch (error) {
    console.warn(
      `모델을 읽지 못했습니다(${row.id}).`,
      error instanceof Error ? error.message : String(error),
    );
    return null;
  }
}

async function main() {
  const apply = process.argv.includes("--apply");
  const { entries, applied } = await turnResolvedModelBackfill({ apply });
  const counts = new Map();
  for (const { turn, resolvedModel } of entries) {
    const key = `${turn.model ?? "-"} → ${resolvedModel ?? "기록 없음"}`;
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  for (const [key, count] of [...counts].sort()) {
    console.log(`${key} | ${count}건`);
  }
  const found = entries.filter((entry) => entry.resolvedModel).length;
  console.log(
    apply
      ? `저장 ${applied.length}건 / 찾음 ${found}건 / 대상 ${entries.length}건`
      : `계산만 함 — 찾음 ${found}건 / 대상 ${entries.length}건, 저장하려면 --apply`,
  );
  await pool.end();
}

if (import.meta.url === `file://${process.argv[1]}`) {
  await main();
}
