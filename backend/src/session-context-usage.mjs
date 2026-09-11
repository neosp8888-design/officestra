// 이 파일은 CLI 세션 기록을 읽어 대화 시점의 컨텍스트 사용량과 한도를 계산한다.

import {
  closeSync,
  existsSync,
  openSync,
  readdirSync,
  readSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import {
  antigravityContextUsage,
} from "./antigravity-local-state.mjs";

const CLAUDE_CONTEXT_WINDOWS = [
  ["claude-fable-5", 1_000_000],
  ["fable", 1_000_000],
  ["claude-mythos-5", 1_000_000],
  ["claude-opus-5", 1_000_000],
  ["claude-opus-4-8", 1_000_000],
  ["claude-opus-4-7", 1_000_000],
  ["claude-opus-4-6", 1_000_000],
  ["claude-sonnet-5", 1_000_000],
  ["claude-sonnet-4-6", 1_000_000],
  ["claude-haiku-4-5", 200_000],
];

const MAX_READ_BYTES = 64 * 1024 * 1024;
const MISSING_PATH_RETRY_MS = 30_000;

const transcriptReaders = new Map();
const transcriptPaths = new Map();

export function claudeContextWindow(model) {
  const id = String(model ?? "").trim().toLowerCase();
  if (!id) {
    return null;
  }
  // Claude Code 선택값의 `[1m]` 접미사는 모델과 무관하게 1M 컨텍스트 창이다.
  if (id.endsWith("[1m]")) {
    return 1_000_000;
  }
  for (const [prefix, window] of CLAUDE_CONTEXT_WINDOWS) {
    if (id.startsWith(prefix)) {
      return window;
    }
  }
  return null;
}

export function sessionContextUsage({
  backend,
  sessionID,
  model,
  at,
  claudeRoot = join(homedir(), ".claude", "projects"),
  codexRoot = join(homedir(), ".codex", "sessions"),
  antigravityRoot = join(
    homedir(),
    ".gemini",
    "antigravity-cli",
    "conversations",
  ),
  antigravityUsageReader = antigravityContextUsage,
  maxReadBytes = MAX_READ_BYTES,
  contextWindowOverride = null,
}) {
  const kind = String(backend ?? "").trim();
  if (!["claude", "codex", "antigravity"].includes(kind)) {
    return null;
  }
  const id = String(sessionID ?? "").trim();
  if (!id) {
    return null;
  }
  const boundary = new Date(at ?? Date.now()).getTime();
  if (!Number.isFinite(boundary)) {
    return null;
  }

  if (kind === "antigravity") {
    return antigravityUsageReader({
      sessionID: id,
      at: boundary,
      root: antigravityRoot,
    });
  }

  const path = kind === "claude"
    ? claudeTranscriptPath(id, claudeRoot)
    : codexRolloutPath(id, codexRoot);
  if (!path) {
    return null;
  }

  const entry = latestEntry(
    transcriptEntries(kind, path, maxReadBytes),
    boundary,
  );
  if (!entry) {
    return null;
  }
  const limitTokens = contextWindowOverride ?? entry.limitTokens ?? claudeContextWindow(model);
  if (!limitTokens || limitTokens <= 0) {
    return null;
  }
  return {
    usedTokens: Math.min(entry.usedTokens, limitTokens),
    limitTokens,
  };
}

function latestEntry(entries, boundary) {
  let match = null;
  for (const entry of entries) {
    if (entry.at > boundary) {
      break;
    }
    match = entry;
  }
  return match;
}

function transcriptEntries(kind, path, maxReadBytes) {
  let reader = transcriptReaders.get(path);
  let size;
  try {
    size = statSync(path).size;
  } catch {
    return reader?.entries ?? [];
  }

  if (!reader || size < reader.offset) {
    reader = { offset: 0, remainder: "", entries: [] };
    transcriptReaders.set(path, reader);
  }
  let length = size - reader.offset;
  if (length <= 0) {
    return reader.entries;
  }
  let discardsLeadingFragment = false;
  if (length > maxReadBytes) {
    const offset = Math.max(0, size - maxReadBytes);
    reader = { offset, remainder: "", entries: [] };
    transcriptReaders.set(path, reader);
    length = size - offset;
    discardsLeadingFragment = offset > 0;
  }

  let descriptor;
  try {
    descriptor = openSync(path, "r");
    const buffer = Buffer.alloc(length);
    const read = readSync(descriptor, buffer, 0, length, reader.offset);
    reader.offset += read;
    const lines = (
      reader.remainder + buffer.subarray(0, read).toString("utf8")
    ).split("\n");
    reader.remainder = lines.pop() ?? "";
    if (discardsLeadingFragment) {
      lines.shift();
    }
    for (const line of lines) {
      const entry = kind === "claude"
        ? claudeContextEntry(line)
        : codexContextEntry(line);
      if (entry) {
        reader.entries.push(entry);
      }
    }
  } catch {
    return reader.entries;
  } finally {
    if (descriptor !== undefined) {
      closeSync(descriptor);
    }
  }
  return reader.entries;
}

export function claudeContextEntry(line) {
  if (
    !line.includes("\"usage\"") &&
    !line.includes("compact_boundary")
  ) {
    return null;
  }
  let record;
  try {
    record = JSON.parse(line);
  } catch {
    return null;
  }
  if (
    record?.type === "system" &&
    record.subtype === "compact_boundary"
  ) {
    const metadata = record.compact_metadata ?? record.compactMetadata ?? {};
    const usedTokens = tokenCount(
      metadata.post_tokens ?? metadata.postTokens,
    );
    const at = Date.parse(record.timestamp);
    if (usedTokens <= 0 || !Number.isFinite(at)) {
      return null;
    }
    return { at, usedTokens, limitTokens: null };
  }
  if (record?.type !== "assistant" || record.isSidechain === true) {
    return null;
  }
  const usage = record.message?.usage;
  if (!usage) {
    return null;
  }
  const usedTokens = tokenCount(usage.input_tokens) +
    tokenCount(usage.cache_read_input_tokens) +
    tokenCount(usage.cache_creation_input_tokens);
  const at = Date.parse(record.timestamp);
  if (usedTokens <= 0 || !Number.isFinite(at)) {
    return null;
  }
  return { at, usedTokens, limitTokens: null };
}

export function codexContextEntry(line) {
  if (!line.includes("\"token_count\"")) {
    return null;
  }
  let record;
  try {
    record = JSON.parse(line);
  } catch {
    return null;
  }
  if (record?.payload?.type !== "token_count") {
    return null;
  }
  const info = record.payload.info;
  const usedTokens = tokenCount(info?.last_token_usage?.input_tokens);
  const limitTokens = tokenCount(info?.model_context_window);
  const at = Date.parse(record.timestamp);
  if (usedTokens <= 0 || !Number.isFinite(at)) {
    return null;
  }
  return {
    at,
    usedTokens,
    limitTokens: limitTokens > 0 ? limitTokens : null,
  };
}

function tokenCount(value) {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : 0;
}

function claudeTranscriptPath(sessionID, root) {
  if (!existsSync(root)) {
    return null;
  }
  let projects;
  try {
    projects = readdirSync(root, { withFileTypes: true });
  } catch {
    return null;
  }
  const candidates = [];
  for (const project of projects) {
    if (!project.isDirectory()) {
      continue;
    }
    const path = join(root, project.name, `${sessionID}.jsonl`);
    try {
      const metadata = statSync(path);
      if (metadata.isFile()) {
        candidates.push({
          path,
          size: metadata.size,
          modifiedAt: metadata.mtimeMs,
        });
      }
    } catch {
      // 동시에 정리된 과거 worktree 기록은 건너뛴다.
    }
  }
  candidates.sort((left, right) =>
    right.modifiedAt - left.modifiedAt ||
    right.size - left.size ||
    left.path.localeCompare(right.path)
  );
  return candidates.at(0)?.path ?? null;
}

function codexRolloutPath(sessionID, root) {
  return cachedPath(`codex:${sessionID}:${root}`, () => {
    if (!existsSync(root)) {
      return null;
    }
    let entries;
    try {
      entries = readdirSync(root, { recursive: true });
    } catch {
      return null;
    }
    const relative = entries.find((entry) => {
      const value = String(entry);
      return value.endsWith(".jsonl") && value.includes(sessionID);
    });
    return relative ? join(root, String(relative)) : null;
  });
}

function cachedPath(key, resolve) {
  const cached = transcriptPaths.get(key);
  if (cached?.path && existsSync(cached.path)) {
    return cached.path;
  }
  if (cached && !cached.path && Date.now() < cached.retryAt) {
    return null;
  }
  const path = resolve();
  transcriptPaths.set(key, {
    path,
    retryAt: Date.now() + MISSING_PATH_RETRY_MS,
  });
  return path;
}
