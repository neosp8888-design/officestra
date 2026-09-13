// Antigravity가 로컬 SQLite에 남긴 대화 메타데이터를 읽기 전용으로 해석한다.

import { createRequire } from "node:module";
import { existsSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const SESSION_ID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
const MAX_METADATA_BYTES = 64 * 1024;
const parsedStateCache = new Map();

export function antigravityConversationPath(
  sessionID,
  root = join(homedir(), ".gemini", "antigravity-cli", "conversations"),
) {
  const id = String(sessionID ?? "").trim();
  return SESSION_ID.test(id) ? join(root, `${id}.db`) : null;
}

export function antigravityContextUsage({
  sessionID,
  at = Date.now(),
  root,
}) {
  const boundary = new Date(at).getTime();
  if (!Number.isFinite(boundary)) return null;
  const path = antigravityConversationPath(sessionID, root);
  const state = readAntigravityState(path);
  if (!state) return null;
  let match = null;
  for (const entry of state.contextEntries) {
    if (entry.at > boundary) break;
    match = entry;
  }
  if (!match?.limitTokens || match.limitTokens <= 0) return null;
  return {
    usedTokens: Math.min(match.usedTokens, match.limitTokens),
    limitTokens: match.limitTokens,
  };
}

export function antigravitySessionUsage(sessionID, { root } = {}) {
  const path = antigravityConversationPath(sessionID, root);
  return readAntigravityState(path)?.usage ?? null;
}

// Each completed model step carries its own input/cache usage. Keep those
// boundaries for context-tier pricing instead of pricing the session delta.
export function antigravityTurnRequestUsages(sessionID, {
  root, afterIndex, startedAt, endedAt = new Date(),
} = {}) {
  const path = antigravityConversationPath(sessionID, root);
  if (!path || !existsSync(path)) return null;
  const from = new Date(startedAt ?? Number.NaN).getTime();
  const to = new Date(endedAt).getTime();
  if (!Number.isFinite(to)) return null;
  if (!Number.isInteger(afterIndex) && !Number.isFinite(from)) return null;
  let database;
  try {
    const { DatabaseSync } = require("node:sqlite");
    database = new DatabaseSync(path, { readOnly: true });
    const rows = database.prepare(`
      SELECT idx, metadata FROM steps
      WHERE step_type = 15 AND idx > ? AND metadata IS NOT NULL
        AND length(metadata) <= ? ORDER BY idx
    `).all(Number.isInteger(afterIndex) ? afterIndex : -1, MAX_METADATA_BYTES);
    const requests = [];
    for (const row of rows) {
      const parsed = parseAntigravityStepMetadata(row.metadata);
      if (!parsed) return null;
      if (!Number.isInteger(afterIndex) && parsed.at < from) continue;
      if (parsed.at > to) continue;
      requests.push({ usage: parsed.usage, pricedAt: new Date(parsed.at) });
    }
    return requests;
  } catch {
    return null;
  } finally {
    database?.close();
  }
}

function readAntigravityState(path) {
  if (!path || !existsSync(path)) return null;
  const signature = fileSignature(path);
  const cached = parsedStateCache.get(path);
  if (cached?.signature === signature) return cached.value;

  let database;
  try {
    const { DatabaseSync } = require("node:sqlite");
    database = new DatabaseSync(path, { readOnly: true });
    const generatorRows = database.prepare(
      `
        SELECT data
        FROM gen_metadata
        WHERE length(data) <= ?
        ORDER BY idx
      `,
    ).all(MAX_METADATA_BYTES);
    const stepRows = database.prepare(
      `
        SELECT metadata
        FROM steps
        WHERE step_type = 15
          AND metadata IS NOT NULL
          AND length(metadata) <= ?
        ORDER BY idx
      `,
    ).all(MAX_METADATA_BYTES);
    const value = parsedAntigravityState(generatorRows, stepRows);
    parsedStateCache.set(path, { signature, value });
    return value;
  } catch {
    return cached?.value ?? null;
  } finally {
    try {
      database?.close();
    } catch {
      // 대화가 동시에 갱신되는 순간의 close 오류는 다음 읽기에서 회복한다.
    }
  }
}

function parsedAntigravityState(generatorRows, stepRows) {
  const contexts = new Map();
  for (const row of generatorRows ?? []) {
    const parsed = parseAntigravityGeneratorMetadata(row.data);
    if (parsed) contexts.set(parsed.executionID, parsed);
  }

  const executions = new Map();
  const total = emptyUsage();
  let usageCount = 0;
  for (const row of stepRows ?? []) {
    const parsed = parseAntigravityStepMetadata(row.metadata);
    if (!parsed) continue;
    const execution = executions.get(parsed.executionID) ?? {
      at: parsed.at,
      usage: emptyUsage(),
    };
    execution.at = Math.max(execution.at, parsed.at);
    addUsage(execution.usage, parsed.usage);
    executions.set(parsed.executionID, execution);
    addUsage(total, parsed.usage);
    usageCount += 1;
  }

  const contextEntries = [];
  for (const [executionID, context] of contexts) {
    const execution = executions.get(executionID);
    if (!execution) continue;
    contextEntries.push({
      at: execution.at,
      usedTokens: context.usedTokens,
      limitTokens: context.limitTokens,
    });
  }
  contextEntries.sort((left, right) => left.at - right.at);
  return {
    contextEntries,
    usage: usageCount > 0 ? usagePayload(total) : null,
  };
}

export function parseAntigravityGeneratorMetadata(value) {
  try {
    const fields = protobufFields(value);
    const executionID = utf8Field(fields, 4);
    const model = nestedFields(fields, 1);
    const start = nestedFields(model, 9);
    const context = nestedFields(start, 10);
    const usedTokens = numberField(context, 1);
    const limitTokens = numberField(context, 4);
    if (!executionID || usedTokens <= 0 || limitTokens <= 0) return null;
    return { executionID, usedTokens, limitTokens };
  } catch {
    return null;
  }
}

export function parseAntigravityStepMetadata(value) {
  try {
    const fields = protobufFields(value);
    const executionID = utf8Field(fields, 12);
    const timestamp = nestedFields(fields, 1);
    const seconds = numberField(timestamp, 1);
    const nanos = numberField(timestamp, 2);
    const usage = nestedFields(fields, 9);
    if (!executionID || seconds <= 0 || usage.length === 0) return null;
    return {
      executionID,
      at: seconds * 1_000 + Math.trunc(nanos / 1_000_000),
      usage: {
        inputTokens: numberField(usage, 2),
        outputTokens: numberField(usage, 3),
        cachedInputTokens: numberField(usage, 5),
        reasoningOutputTokens: numberField(usage, 9),
      },
    };
  } catch {
    return null;
  }
}

function fileSignature(path) {
  return [path, `${path}-wal`].map((candidate) => {
    try {
      const metadata = statSync(candidate);
      return `${metadata.size}:${metadata.mtimeMs}`;
    } catch {
      return "missing";
    }
  }).join("|");
}

function emptyUsage() {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cachedInputTokens: 0,
    reasoningOutputTokens: 0,
  };
}

function addUsage(target, source) {
  for (const field of Object.keys(target)) {
    target[field] += source[field] ?? 0;
  }
}

function usagePayload(usage) {
  return {
    ...usage,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
}

export function nestedFields(fields, number) {
  const value = fields.find(
    (field) => field.number === number && Buffer.isBuffer(field.value),
  )?.value;
  return value ? protobufFields(value) : [];
}

export function utf8Field(fields, number) {
  const value = fields.find(
    (field) => field.number === number && Buffer.isBuffer(field.value),
  )?.value;
  return value?.toString("utf8").trim() ?? "";
}

export function numberField(fields, number) {
  return fields.find(
    (field) => field.number === number && typeof field.value === "number",
  )?.value ?? 0;
}

export function protobufFields(value) {
  const buffer = Buffer.isBuffer(value) ? value : Buffer.from(value ?? []);
  const fields = [];
  let offset = 0;
  while (offset < buffer.length) {
    const tag = readVarint(buffer, offset);
    offset = tag.offset;
    const number = Number(tag.value >> 3n);
    const wire = Number(tag.value & 7n);
    if (number <= 0) throw new Error("invalid protobuf field");
    if (wire === 0) {
      const decoded = readVarint(buffer, offset);
      offset = decoded.offset;
      fields.push({ number, value: Number(decoded.value) });
    } else if (wire === 1) {
      offset = checkedOffset(buffer, offset, 8);
    } else if (wire === 2) {
      const decoded = readVarint(buffer, offset);
      offset = decoded.offset;
      const length = Number(decoded.value);
      const end = checkedOffset(buffer, offset, length);
      fields.push({ number, value: buffer.subarray(offset, end) });
      offset = end;
    } else if (wire === 5) {
      offset = checkedOffset(buffer, offset, 4);
    } else {
      throw new Error("unsupported protobuf wire type");
    }
  }
  return fields;
}

function readVarint(buffer, start) {
  let value = 0n;
  let shift = 0n;
  let offset = start;
  while (offset < buffer.length && shift <= 70n) {
    const byte = buffer[offset];
    offset += 1;
    value |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) return { value, offset };
    shift += 7n;
  }
  throw new Error("invalid protobuf varint");
}

function checkedOffset(buffer, offset, length) {
  const end = offset + length;
  if (!Number.isSafeInteger(length) || length < 0 || end > buffer.length) {
    throw new Error("invalid protobuf length");
  }
  return end;
}
