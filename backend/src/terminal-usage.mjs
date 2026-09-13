// 터미널 턴의 사용량을 각 CLI가 디스크에 남긴 기록에서 턴 구간만큼 합산한다.

import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";

import {
  claudeMessageUsage,
  claudeSessionUsageKey,
  codexRequestUsage,
} from "./agent-event-parser.mjs";

function boundary(value) {
  const at = value instanceof Date ? value : new Date(value ?? Number.NaN);
  const time = at.getTime();
  return Number.isFinite(time) ? time : null;
}

export function addUsage(total, usage) {
  if (!usage) {
    return total;
  }
  const result = total ?? {
    inputTokens: null,
    outputTokens: null,
    cachedInputTokens: null,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: null,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
  for (
    const field of [
      "inputTokens",
      "outputTokens",
      "cachedInputTokens",
      "cacheWriteInputTokens",
      "cacheWrite5mInputTokens",
      "cacheWrite1hInputTokens",
      "reasoningOutputTokens",
    ]
  ) {
    const value = usage[field];
    if (value === null || value === undefined) continue;
    result[field] = (result[field] ?? 0) + value;
  }
  for (const field of ["serviceTier", "speed", "inferenceGeo"]) {
    result[field] ??= usage[field] ?? null;
  }
  return result;
}

// 턴 구간 안의 기록만 골라 사용량을 더한다. 기록마다 시각과 중복 열쇠를
// 뽑는 방법만 다르므로 그 부분만 호출자가 넘긴다.
async function sumTurnUsage(path, { startedAt, endedAt }, extract) {
  const from = boundary(startedAt);
  const to = boundary(endedAt) ?? Date.now();
  if (!path || from === null) {
    return null;
  }
  const seen = new Set();
  let total = null;
  let stream;
  try {
    stream = createReadStream(path, { encoding: "utf8" });
    const lines = createInterface({ input: stream, crlfDelay: Infinity });
    for await (const line of lines) {
      if (!line) continue;
      let object;
      try {
        object = JSON.parse(line);
      } catch {
        continue;
      }
      const at = boundary(object?.timestamp);
      if (at === null || at < from || at > to) continue;
      const entry = extract(object);
      if (!entry?.usage) continue;
      if (entry.key !== null && entry.key !== undefined) {
        if (seen.has(entry.key)) continue;
        seen.add(entry.key);
      }
      total = addUsage(total, entry.usage);
    }
  } catch {
    // 기록을 읽지 못하면 사용량 없이 턴을 마친다.
    return null;
  } finally {
    stream?.destroy();
  }
  return total;
}

export async function claudeTranscriptTurnUsage(path, window) {
  return await sumTurnUsage(path, window, (object) => {
    if (object?.type !== "assistant") return null;
    return {
      usage: claudeMessageUsage(object),
      key: claudeSessionUsageKey(object),
    };
  });
}

// Codex 턴의 종료 시각은 초 단위로 잘려 저장되므로, 마지막 token_count가
// 그 초의 소수점 뒤에 오면 시간 구간으로는 놓친다. 대신 rollout이 스스로
// 남긴 task_started~task_complete 구간을 턴 경계로 삼는다.
export async function codexRolloutTurnUsage(path, {
  startedAt, endedAt, scope = "task", startOffset = 0,
}) {
  const from = boundary(startedAt);
  if (!path || from === null) {
    return null;
  }
  const startedSecond = Math.floor(from / 1_000);
  const to = boundary(endedAt) ?? Date.now();
  let inside = false;
  let total = null;
  let context = {};
  const seen = new Set();
  const requestUsages = [];
  let stream;
  try {
    stream = createReadStream(path, { encoding: "utf8", start: startOffset });
    const lines = createInterface({ input: stream, crlfDelay: Infinity });
    for await (const line of lines) {
      if (!line) continue;
      let object;
      try {
        object = JSON.parse(line);
      } catch {
        continue;
      }
      const payload = object?.payload;
      if (object?.type === "turn_context") {
        context = payload ?? {};
        continue;
      }
      if (object?.type !== "event_msg" || !payload) continue;
      if (scope === "window") {
        const at = boundary(object.timestamp);
        if (at === null || at < from || at > to) continue;
        inside = true;
      }
      if (payload.type === "task_started") {
        if (scope === "task") inside = Number(payload.started_at) === startedSecond;
        continue;
      }
      if (!inside) continue;
      if (payload.type === "task_complete" && scope === "task") break;
      const usage = codexRequestUsage(object);
      if (!usage) continue;
      // Rate-limit updates may repeat the last request's usage unchanged.
      // Distinct requests with identical last usage still have different totals.
      const snapshot = payload.info?.total_token_usage;
      const key = JSON.stringify([context.turn_id, snapshot ?? object.timestamp]);
      if (seen.has(key)) continue;
      seen.add(key);
      total = addUsage(total, usage);
      const tier = usage.serviceTier ?? context.service_tier;
      requestUsages.push({
        usage,
        model: typeof context.model === "string" ? context.model : undefined,
        fastMode: tier === "fast" ? true : tier === "default" ? false : undefined,
        pricedAt: object.timestamp,
      });
    }
  } catch {
    // 기록을 읽지 못하면 사용량 없이 턴을 마친다.
    return null;
  } finally {
    stream?.destroy();
  }
  return total ? { ...total, requestUsages } : null;
}
