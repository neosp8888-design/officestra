// 앱이 소유한 PTY의 실행 명세, 직원 잠금, CLI 턴 기록을 관리한다.

import { randomUUID, randomBytes, timingSafeEqual } from "node:crypto";
import { createRequire } from "node:module";
import {
  closeSync,
  existsSync,
  openSync,
  readSync,
  readdirSync,
  statSync,
  watch,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  AgentBusyError,
  AgentDrainingError,
  CharacterNotFoundError,
  executionEnvironment,
  locateExecutable,
} from "./agent-runtime.mjs";
import {
  nestedFields,
  parseAntigravityStepMetadata,
  protobufFields,
  utf8Field,
} from "./antigravity-local-state.mjs";
import {
  CodexTerminalTurnGate,
  codexRolloutUserPrompt,
  findCodexRolloutPath,
} from "./codex-rollout-turns.mjs";
import {
  generatedImageRoot,
  listGeneratedImages,
} from "./local-artifacts.mjs";
import {
  consumeStructuredTurnResult,
  discardStructuredTurnResult,
  prepareStructuredTurnResult,
  readStructuredTurnResult,
  identityPromptWithStructuredResult,
} from "./structured-turn-result.mjs";
import { antigravityStepPayloadReasoning } from "./antigravity-reasoning.mjs";
import { TerminalActivityCollector, readClaudeTerminalActivities } from "./terminal-turn-activities.mjs";

const require = createRequire(import.meta.url);
const SESSION_ID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
const terminalHookPath = join(dirname(fileURLToPath(import.meta.url)),
  "officestra-terminal-hook");

function quotedConfig(value) {
  return JSON.stringify(String(value ?? ""));
}

function nonnegativeCost(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : null;
}

// 상태줄 명령은 Claude Code가 세션 누적 비용(cost.total_cost_usd)을 JSON으로
// 넘겨주는 유일한 대화형 경로다. 훅과 같은 스크립트가 받아 백엔드로 보내고
// 화면에는 아무것도 출력하지 않는다.
function terminalHookSettings(character) {
  const hook = { hooks: [{ type: "command", command: "officestra-terminal-hook" }] };
  return JSON.stringify({
    fastMode: character.fastMode === true,
    hooks: {
      UserPromptSubmit: [hook],
      Stop: [hook],
    },
    statusLine: { type: "command", command: "officestra-terminal-hook" },
  });
}

function antigravityPermissionArguments(permission) {
  switch (permission) {
    case "plan":
      return ["--mode", "plan"];
    case "accept-edits":
      return [
        "--mode", "accept-edits", "--sandbox",
        "--dangerously-skip-permissions",
      ];
    case "dangerously-skip-permissions":
      return ["--mode", "accept-edits", "--dangerously-skip-permissions"];
    default:
      throw new Error(`지원하지 않는 Antigravity 권한입니다: ${permission}`);
  }
}

export function terminalArguments({
  character,
  previousSessionID = null,
  workdir,
  hookPath = terminalHookPath,
  nodePath = process.execPath,
}) {
  switch (character.backend) {
    case "claude": { // print 모드 플래그 없이 원래 대화형 CLI를 실행한다.
      const args = [
        "--effort", character.effort,
        "--permission-mode", character.permission,
        "--settings", terminalHookSettings(character),
      ];
      if (character.model) args.push("--model", character.model);
      args.push("--append-system-prompt", identityPromptWithStructuredResult(character.identityPrompt, character.id));
      if (previousSessionID) args.push("--resume", previousSessionID);
      return args;
    }
    case "codex": {
      const args = previousSessionID ? ["resume", previousSessionID] : [];
      if (character.model) args.push("-c", `model=${quotedConfig(character.model)}`);
      args.push(
        "-c", `model_reasoning_effort=${quotedConfig(character.effort)}`,
        "-c", "features.fast_mode=true",
        "-c", `service_tier=${quotedConfig(character.fastMode ? "fast" : "default")}`,
        "-c", 'model_reasoning_summary="detailed"',
        "-c", "show_raw_agent_reasoning=true",
        "-c", `developer_instructions=${quotedConfig(identityPromptWithStructuredResult(character.identityPrompt, character.id))}`,
        "-s", character.permission,
        "-c", `notify=${JSON.stringify([nodePath, hookPath])}`,
      );
      return args;
    }
    case "antigravity": {
      const args = [
        "--model", character.model,
        "--effort", character.effort,
      ];
      if (previousSessionID) args.push("--conversation", previousSessionID);
      if (workdir) args.push("--add-dir", workdir);
      args.push(...antigravityPermissionArguments(character.permission));
      return args;
    }
    default:
      throw new Error(`지원하지 않는 직원 백엔드입니다: ${character.backend}`);
  }
}

export function terminalEnvironment(character, {
  baseEnvironment = process.env,
  workdir,
  characterID,
  port = 4317,
} = {}) {
  const environment = {
    ...executionEnvironment(character, baseEnvironment, { workdir }),
    TERM: "xterm-256color",
    COLORTERM: "truecolor",
    LANG: baseEnvironment.LANG || "en_US.UTF-8",
    OFFICESTRA_TERMINAL_EVENTS_URL:
      `http://127.0.0.1:${port}/api/terminal-sessions/` +
      `${encodeURIComponent(characterID)}/events`,
    OFFICESTRA_TERMINAL_CHARACTER_ID: characterID,
  };
  environment.PATH = [
    dirname(terminalHookPath),
    dirname(process.execPath),
    ...String(environment.PATH ?? "").split(delimiter),
  ].filter(Boolean).filter((value, index, values) =>
    values.indexOf(value) === index
  ).join(delimiter);
  return Object.fromEntries(
    Object.entries(environment)
      .filter(([, value]) => value !== undefined && value !== null)
      .map(([key, value]) => [key, String(value)]),
  );
}

export function parseAntigravityTerminalStep(row) {
  try {
    if (Number(row?.status) !== 3) return null;
    const fields = protobufFields(row.step_payload);
    const stepType = Number(row.step_type);
    if (stepType === 14) {
      const text = utf8Field(nestedFields(fields, 19), 2);
      return text ? { kind: "user", text } : null;
    }
    if (stepType === 15) {
      const text = utf8Field(nestedFields(fields, 20), 1);
      const reasoning = antigravityStepPayloadReasoning(row.step_payload);
      const activities = reasoning ? [{
        kind: "thinking", text: reasoning, eventKey: `step:${row.idx}:thinking`,
      }] : [];
      return text
        ? {
          kind: "assistant",
          text,
          metadata: parseAntigravityStepMetadata(row.metadata),
          activities,
        }
        : { kind: "activity", activities, metadata: parseAntigravityStepMetadata(row.metadata) };
    }
    if (stepType === 132) {
      return { kind: "activity", activities: [{
        kind: "tool", text: "도구 결과 수신", eventKey: `step:${row.idx}:tool`,
      }] };
    }
    return null;
  } catch {
    return null;
  }
}

function dateFromEpoch(value, fallback = new Date()) {
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds > 0
    ? new Date(seconds * 1_000)
    : fallback;
}

function resetTerminalArtifacts(state) {
  state.activities = new TerminalActivityCollector(state.workdir);
  state.initialGeneratedImages = new Set(listGeneratedImages(
    state.externalSessionID,
    generatedImageRoot(state.backend),
  ));
  state.structuredResultPath = prepareStructuredTurnResult({
    workdir: state.workdir,
    characterID: state.characterID,
  });
}

function lastClaudeAssistantMessage(path) {
  if (!path || !existsSync(path)) return "";
  let descriptor;
  try {
    const size = statSync(path).size;
    const length = Math.min(size, 4 * 1024 * 1024);
    const buffer = Buffer.alloc(length);
    descriptor = openSync(path, "r");
    const count = readSync(descriptor, buffer, 0, length, size - length);
    const lines = buffer.subarray(0, count).toString("utf8").split("\n");
    if (size > length) lines.shift();
    for (let index = lines.length - 1; index >= 0; index -= 1) {
      let value;
      try {
        value = JSON.parse(lines[index]);
      } catch {
        continue;
      }
      if (value?.type !== "assistant") continue;
      const content = value?.message?.content ?? value?.content;
      const text = (Array.isArray(content) ? content : [])
        .filter((item) => item?.type === "text")
        .map((item) => String(item.text ?? "").trim())
        .filter(Boolean)
        .join("\n");
      if (text) return text;
    }
  } catch {
    return "";
  } finally {
    if (descriptor !== undefined) closeSync(descriptor);
  }
  return "";
}

class AntigravityTerminalWatcher {
  constructor({
    state,
    runtime,
    root,
    pollIntervalMs = 1_000,
    debounceMs = 500,
  }) {
    this.state = state;
    this.runtime = runtime;
    this.root = root;
    this.conversationsRoot = join(root, "conversations");
    this.summaryPath = join(root, "conversation_summaries.db");
    this.path = state.externalSessionID
      ? join(this.conversationsRoot, `${state.externalSessionID}.db`)
      : null;
    this.lastIndex = this.path ? this.maximumIndex(this.path) : -1;
    this.pendingIndexes = new Set();
    // 응답 뒤 background task 알림으로 같은 질문의 후속 응답이 올 수 있다.
    // running 잠금과 별개로 마지막 사용자 경계를 다음 질문까지 보존한다.
    this.currentTurn = null;
    this.baseline = new Map(this.databaseFiles().map((entry) => [entry.path, entry.mtimeMs]));
    this.watchers = [];
    this.timer = null;
    this.pollTimer = null;
    this.pollIntervalMs = pollIntervalMs;
    this.debounceMs = debounceMs;
    this.sweepPromise = Promise.resolve();
  }

  databaseFiles() {
    try {
      return readdirSync(this.conversationsRoot)
        .filter((name) => name.endsWith(".db"))
        .map((name) => {
          const path = join(this.conversationsRoot, name);
          return { path, mtimeMs: statSync(path).mtimeMs };
        });
    } catch {
      return [];
    }
  }

  maximumIndex(path) {
    let database;
    try {
      const { DatabaseSync } = require("node:sqlite");
      database = new DatabaseSync(path, { readOnly: true });
      return Number(database.prepare("SELECT COALESCE(max(idx), -1) AS idx FROM steps").get()?.idx ?? -1);
    } catch {
      return -1;
    } finally {
      try { database?.close(); } catch {}
    }
  }

  start() {
    const schedule = () => this.schedule();
    try { this.watchers.push(watch(this.conversationsRoot, schedule)); } catch {}
    try { this.watchers.push(watch(this.summaryPath, schedule)); } catch {}
    // SQLite가 WAL에만 기록하면 macOS의 fs.watch가 마지막 변경을 놓칠 수
    // 있다. 주기 확인은 debounce를 거치지 않아 지속적인 WAL 알림에도
    // 굶지 않고 반드시 실행된다.
    this.pollTimer = setInterval(
      () => this.enqueueSweep(),
      this.pollIntervalMs,
    );
    this.pollTimer.unref?.();
    this.enqueueSweep();
  }

  schedule() {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.enqueueSweep();
    }, this.debounceMs);
  }

  enqueueSweep() {
    this.sweepPromise = this.sweepPromise
      .then(() => this.sweep())
      .catch((error) => console.warn(
        "Antigravity 터미널 기록 해독 실패:",
        error instanceof Error ? error.message : String(error),
      ));
    return this.sweepPromise;
  }

  async adoptConversationIfNeeded() {
    if (this.path) return true;
    const candidates = this.databaseFiles()
      .filter((entry) =>
        !this.baseline.has(entry.path) ||
        entry.mtimeMs > Math.max(this.baseline.get(entry.path) ?? 0, this.state.startedAt)
      )
      .sort((left, right) => right.mtimeMs - left.mtimeMs);
    const candidate = candidates[0];
    if (!candidate) return false;
    const id = basename(candidate.path, ".db");
    if (!SESSION_ID.test(id)) return false;
    await this.runtime.bindTerminalExternalSession({
      characterID: this.state.characterID,
      sessionID: this.state.sessionID,
      externalSessionID: id,
    });
    this.state.externalSessionID = id;
    this.path = candidate.path;
    this.lastIndex = -1;
    return true;
  }

  rows() {
    let database;
    try {
      const { DatabaseSync } = require("node:sqlite");
      database = new DatabaseSync(this.path, { readOnly: true });
      const rows = database.prepare(
        `
          SELECT idx, step_type, status, metadata, step_payload
          FROM steps
          WHERE idx > ?
          ORDER BY idx
        `,
      ).all(this.lastIndex);
      const pending = [...this.pendingIndexes];
      if (pending.length > 0) {
        const placeholders = pending.map(() => "?").join(", ");
        rows.push(...database.prepare(
          `
            SELECT idx, step_type, status, metadata, step_payload
            FROM steps
            WHERE idx IN (${placeholders})
          `,
        ).all(...pending));
      }
      return [...new Map(rows.map((row) => [Number(row.idx), row])).values()]
        .sort((left, right) => Number(left.idx) - Number(right.idx));
    } finally {
      try { database?.close(); } catch {}
    }
  }

  async sweep() {
    if (this.state.closed || !await this.adoptConversationIfNeeded()) return;
    for (const row of this.rows()) {
      const index = Number(row.idx);
      this.lastIndex = Math.max(this.lastIndex, index);
      if (this.currentTurn && index < this.currentTurn.userIndex) {
        this.pendingIndexes.delete(index);
        continue;
      }
      // Antigravity는 한 idx의 행을 먼저 미완료 상태로 INSERT한 뒤 UPDATE한다.
      // 커서만 넘기면 완료된 같은 행을 다시 볼 수 없으므로 따로 재조회한다.
      if (Number(row.status) !== 3) {
        // 실패/중단된 도구 행은 완료로 UPDATE되지 않는다.
        if ([6, 7].includes(Number(row.status))) this.pendingIndexes.delete(index);
        else this.pendingIndexes.add(index);
        // 미완료 사용자 행 뒤 응답을 이전 질문에 붙이지 않는다.
        if (Number(row.step_type) === 14) break;
        continue;
      }
      const event = parseAntigravityTerminalStep(row);
      if (!event) {
        this.pendingIndexes.delete(index);
        continue;
      }
      if (event.kind === "user") {
        this.pendingIndexes.add(index);
        await this.publishCurrentTurn();
        if (this.state.runningTurnID) {
          await this.runtime.interruptTerminalTurn(this.state.characterID, this.state.runningTurnID);
          this.state.runningTurnID = null;
        }
        if (this.currentTurn) resetTerminalArtifacts(this.state);
        const turn = await (this.state.beginTurn ?? this.runtime.beginTerminalTurn.bind(this.runtime))({
          characterID: this.state.characterID,
          sessionID: this.state.sessionID,
          prompt: event.text,
          execution: this.state.character,
        });
        this.state.runningTurnID = turn.turnID;
        this.currentTurn = {
          turnID: turn.turnID, userIndex: index, responseIndex: -1,
          response: null, usageByIndex: new Map(), published: false, dirty: false,
        };
        this.pendingIndexes.delete(index);
        continue;
      }
      if (!this.currentTurn) {
        // 모드 진입 전의 고아 단계는 미래의 새 질문에 이월하지 않는다.
        this.pendingIndexes.delete(index);
        continue;
      }
      for (const activity of event.activities ?? []) this.state.activities.add(activity);
      if (event.metadata?.usage) {
        this.currentTurn.usageByIndex.set(index, event.metadata.usage);
      }
      this.currentTurn.dirty = true;
      if (event.kind === "assistant") {
        this.state.activities.add({
          kind: "message", text: event.text, eventKey: `step:${index}:message`,
        });
        if (index > this.currentTurn.responseIndex) {
          this.currentTurn.responseIndex = index;
          this.currentTurn.response = event;
        }
      }
      this.pendingIndexes.delete(index);
    }
    await this.publishCurrentTurn();
  }

  async publishCurrentTurn() {
    const turn = this.currentTurn;
    if (!turn?.dirty || !turn.response) return;
    const totals = {};
    for (const usage of turn.usageByIndex.values()) {
      for (const key of ["inputTokens", "outputTokens", "cachedInputTokens", "reasoningOutputTokens"]) {
        totals[key] = (totals[key] ?? 0) + (usage[key] ?? 0);
      }
    }
    // 같은 질문의 전체 스냅샷으로 덮어쓴다. 재시도/후속 응답에 토큰을
    // 더하기만 하거나 마지막 응답 한 단계의 사용량만 남기지 않는다.
    await this.runtime.completeTerminalTurn({
      characterID: this.state.characterID,
      turnID: turn.turnID,
      response: turn.response.text,
      endedAt: turn.response.metadata?.at ? new Date(turn.response.metadata.at) : new Date(),
      usage: turn.usageByIndex.size ? {
        ...totals, cacheWriteInputTokens: null, cacheWrite5mInputTokens: null, cacheWrite1hInputTokens: null,
      } : null,
      initialGeneratedImages: this.state.initialGeneratedImages,
      structured: readStructuredTurnResult(this.state.structuredResultPath),
      activities: this.state.activities.finish(turn.response.text),
      refreshCompleted: turn.published,
    });
    turn.published = true;
    turn.dirty = false;
    this.state.runningTurnID = null;
  }

  forgetCurrentTurn() {
    this.currentTurn = null;
    this.pendingIndexes.clear();
  }

  async stop({ finalSweep = true } = {}) {
    clearTimeout(this.timer);
    clearInterval(this.pollTimer);
    this.pollTimer = null;
    for (const watcher of this.watchers) watcher.close();
    this.watchers = [];
    if (finalSweep) {
      await this.sweepPromise;
      await this.sweep();
    }
  }
}

// Codex 터미널은 notify가 완료 때만 와서 running 구간이 화면에 잡히지 않는다.
// GUI가 하듯 rollout을 파일 끝부터 증분으로 읽어, task_started 뒤 사용자
// 메시지가 붙는 순간 질문과 함께 running 턴을 먼저 만든다. 완료와 답변
// 추출은 검증된 notify 경로가 그대로 맡는다. 터미널이 열려 있는 동안 그
// 직원의 GUI 턴은 막히므로, 이때 붙는 턴은 모두 터미널 턴이다.
class CodexTerminalWatcher {
  constructor({ state, runtime, broadcast, sessionsRoot, intervalMs = 400 }) {
    this.state = state;
    this.runtime = runtime;
    this.broadcast = broadcast;
    this.sessionsRoot = sessionsRoot;
    this.intervalMs = intervalMs;
    this.path = null;
    this.offset = 0;
    this.remainder = "";
    this.pendingStarts = new Map();
    this.seen = new Set();
    this.timer = null;
    this.sweepPromise = Promise.resolve();
  }

  start() {
    this.timer = setInterval(() => this.schedule(), this.intervalMs);
    this.timer.unref?.();
  }

  schedule() {
    this.sweepPromise = this.sweepPromise
      .then(() => this.sweep())
      .catch((error) => console.warn(
        "Codex 터미널 기록을 읽지 못했습니다.",
        error instanceof Error ? error.message : String(error),
      ));
    return this.sweepPromise;
  }

  // notify 경로가 기록한 턴은 늦게 읽혀도 다시 만들지 않는다.
  markCompleted(turnID) {
    const id = String(turnID ?? "").trim();
    if (!id) return;
    this.pendingStarts.delete(id);
    this.seen.add(id);
  }

  // 세션이 묶이는 시점의 파일 끝에서 시작해 지난 기록을 되풀이하지 않는다.
  async adoptRolloutIfNeeded() {
    if (this.path) return true;
    if (!this.state.externalSessionID) return false;
    const path = await findCodexRolloutPath(this.state.externalSessionID, {
      sessionsRoot: this.sessionsRoot,
    });
    if (!path) return false;
    this.path = path;
    this.offset = statSync(path).size;
    this.remainder = "";
    return true;
  }

  readNewLines() {
    const size = statSync(this.path).size;
    if (size < this.offset) {
      this.offset = 0;
      this.remainder = "";
    }
    if (size === this.offset) return [];
    const length = size - this.offset;
    const buffer = Buffer.alloc(length);
    let descriptor;
    try {
      descriptor = openSync(this.path, "r");
      readSync(descriptor, buffer, 0, length, this.offset);
    } finally {
      if (descriptor !== undefined) closeSync(descriptor);
    }
    this.offset = size;
    const lines = (this.remainder + buffer.toString("utf8")).split("\n");
    this.remainder = lines.pop() ?? "";
    return lines;
  }

  async sweep() {
    if (this.state.closed || !await this.adoptRolloutIfNeeded()) return;
    for (const line of this.readNewLines()) {
      if (!line) continue;
      let record;
      try {
        record = JSON.parse(line);
      } catch {
        continue;
      }
      const payload = record?.payload ?? {};
      if (record?.type === "event_msg" && payload.type === "task_started") {
        const turnID = String(payload.turn_id ?? "").trim();
        if (turnID && !this.seen.has(turnID)) {
          this.pendingStarts.set(turnID, payload.started_at);
        }
        continue;
      }
      if (record?.type === "event_msg" && payload.type === "task_complete") {
        // 질문을 보기 전에 끝난 턴은 notify 경로가 처음부터 기록한다.
        this.pendingStarts.delete(String(payload.turn_id ?? "").trim());
        continue;
      }
      if (record?.type === "event_msg" && payload.type === "turn_aborted") {
        const abortedTurnID = String(payload.turn_id ?? "").trim();
        this.pendingStarts.delete(abortedTurnID);
        this.seen.add(abortedTurnID);
        if (
          abortedTurnID &&
          this.state.codexTurnID === abortedTurnID &&
          this.state.runningTurnID
        ) {
          const runningTurnID = this.state.runningTurnID;
          await this.runtime.interruptTerminalTurn(
            this.state.characterID,
            runningTurnID,
          );
          this.state.runningTurnID = null;
          this.state.codexTurnID = null;
          resetTerminalArtifacts(this.state);
          this.broadcast({
            type: "terminal.changed",
            characterId: this.state.characterID,
          });
        }
        continue;
      }
      const prompt = codexRolloutUserPrompt(record);
      if (!prompt || !this.pendingStarts.has(prompt.turnID)) continue;
      const startedAt = this.pendingStarts.get(prompt.turnID);
      this.pendingStarts.delete(prompt.turnID);
      this.seen.add(prompt.turnID);
      if (this.state.runningTurnID) continue;
      // 한 턴을 못 만들어도 같은 묶음의 뒷줄은 계속 읽는다. 이미 읽은
      // 바이트는 다시 오지 않는다.
      try {
        const turn = await (this.state.beginTurn ?? this.runtime.beginTerminalTurn.bind(this.runtime))({
          characterID: this.state.characterID,
          sessionID: this.state.sessionID,
          prompt: prompt.text,
          startedAt: dateFromEpoch(startedAt),
          execution: this.state.character,
        });
        this.state.runningTurnID = turn.turnID;
        this.state.codexTurnID = prompt.turnID;
        this.broadcast({
          type: "terminal.changed",
          characterId: this.state.characterID,
        });
      } catch (error) {
        console.warn(
          `Codex 터미널 running 턴을 만들지 못했습니다(${this.state.characterID}):`,
          error instanceof Error ? error.message : String(error),
        );
      }
    }
  }

  // 완료는 notify가 맡으므로 마지막 sweep은 하지 않는다. 닫히는 중에 턴을
  // 새로 만들었다가 곧바로 중단하는 낭비를 피한다.
  async stop() {
    clearInterval(this.timer);
    this.timer = null;
    await this.sweepPromise;
  }
}

export class TerminalSessionManager {
  constructor({
    runtime,
    broadcast,
    port = 4317,
    antigravityRoot = join(homedir(), ".gemini", "antigravity-cli"),
    codexSessionsRoot = join(homedir(), ".codex", "sessions"),
    baseEnvironment = process.env,
    antigravityPollIntervalMs = 1_000,
    antigravityDebounceMs = 500,
    dispatchTimeoutMs = 20_000,
  }) {
    this.runtime = runtime;
    this.broadcast = broadcast;
    this.port = port;
    this.antigravityRoot = antigravityRoot;
    this.codexSessionsRoot = codexSessionsRoot;
    this.baseEnvironment = baseEnvironment;
    this.antigravityPollIntervalMs = antigravityPollIntervalMs;
    this.antigravityDebounceMs = antigravityDebounceMs;
    this.sessions = new Map();
    this.opening = new Set();
    this.dispatchTimeoutMs = dispatchTimeoutMs;
  }

  // Not a queue: a busy session rejects immediately. A successful PTY write
  // is not acceptance; only the existing CLI hook/watcher can confirm a turn.
  async dispatch({ characterID, prompt, conversationID, attachmentPaths = [], senderCharacterID = null }) {
    const state = this.sessions.get(String(characterID));
    if (this.runtime.draining) throw new AgentDrainingError('백엔드가 재시작 준비 중입니다.');
    if (!state || state.closed || state.closing || this.opening.has(String(characterID))) throw new AgentBusyError('터미널이 아직 준비되지 않았습니다.');
    if (state.runningTurnID || state.dispatch || this.runtime.running?.has(characterID) ||
        this.runtime.compactingCharacters?.has(characterID) || this.runtime.preparingCharacters?.has(characterID)) {
      throw new AgentBusyError('이 직원의 현재 업무가 끝난 뒤 새 업무를 시작하세요.');
    }
    if (conversationID && conversationID !== state.conversationID) throw new AgentBusyError('터미널의 현재 대화와 일치하지 않습니다.');
    if (!Array.isArray(attachmentPaths) || attachmentPaths.some(p => typeof p !== 'string' || !p.startsWith('/') || /[\r\n]/.test(p))) throw new Error('Invalid terminal attachments');
    const text = [String(prompt ?? '').trim(), ...attachmentPaths].filter(Boolean).join('\n');
    if (!text || Buffer.byteLength(text) > 256_000 || /[\x00-\x08\x0b-\x1f\x7f]/.test(text)) throw new Error('Invalid terminal prompt');
    const id = randomUUID();
    let resolve, reject;
    const result = new Promise((yes, no) => { resolve = yes; reject = no; });
    result.catch(() => {}); // The timeout may fire while the DB preflight awaits.
    // Install the lock before the first await, including the DB busy check.
    const pending = { id, text, senderCharacterID, wire: `[OFFICESTRA_REQUEST:${id}]\n${text}`, claimed: false, resolve, reject, expiresAt: Date.now() + this.dispatchTimeoutMs };
    state.dispatch = pending;
    pending.timer = setTimeout(() => {
      if (state.dispatch !== pending) return;
      pending.expired = true;
      // Once claimed the app might already have written to the PTY. Preserve
      // the lock until a matching hook, explicit interrupt, or session close.
      if (!pending.claimed) state.dispatch = null;
      reject(new AgentBusyError('터미널의 접수를 확인하지 못했습니다. 자동 재전송하지 않았습니다. 터미널을 확인하세요.'));
    }, this.dispatchTimeoutMs);
    void (async () => { try {
      const busy = await this.runtime.pool.query("SELECT t.id FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id WHERE s.character_id=$1 AND t.status IN ('pending','running') LIMIT 1", [characterID]);
      if (busy.rows.length || state.closed || state.closing || state.runningTurnID || pending.expired || state.dispatch !== pending) throw new AgentBusyError('터미널의 현재 업무가 끝난 뒤 시작하세요.');
      this.broadcast({ type: 'terminal.dispatch', characterId: characterID, dispatchId: id, terminalSessionId: state.terminalSessionID });
    } catch (error) {
      clearTimeout(pending.timer);
      if (state.dispatch === pending) state.dispatch = null;
      reject(error);
    } })();
    return await result;
  }

  dispatchControl(characterID, body) {
    if (!body || typeof body !== 'object' || Array.isArray(body)) throw new AgentBusyError('잘못된 터미널 전달 확인입니다.');
    const state = this.sessions.get(String(characterID));
    const token = Buffer.from(String(body?.ownerToken ?? ''));
    const expected = Buffer.from(state?.ownerToken ?? '');
    if (!state || state.closed || state.closing || body.terminalSessionId !== state.terminalSessionID ||
        !token.length || token.length !== expected.length || !timingSafeEqual(token, expected)) throw new AgentBusyError('터미널 소유 세션이 일치하지 않습니다.');
    const pending = state.dispatch;
    if (!pending || pending.id !== body.dispatchId) throw new AgentBusyError('만료되었거나 이미 처리한 터미널 요청입니다.');
    if (body.action === 'claim') {
      if (pending.expired || pending.claimed || state.runningTurnID || this.runtime.compactingCharacters?.has(characterID)) throw new AgentBusyError('터미널이 사용 중입니다.');
      pending.claimed = true;
      return { prompt: pending.wire, expiresAt: pending.expiresAt };
    }
    if (body.action === 'reject' && pending.claimed) {
      // Only the owning app can certify that it did not write to the PTY.
      // Accept this negative acknowledgement even after the HTTP deadline.
      clearTimeout(pending.timer);
      state.dispatch = null;
      pending.reject(new AgentBusyError('터미널 입력 중이거나 입력창이 준비되지 않았습니다.'));
      return { accepted: false };
    }
    throw new AgentBusyError('잘못된 터미널 전달 확인입니다.');
  }

  async beginDispatchedTurn(state, options) {
    const pending = state.dispatch;
    const matches = pending?.claimed && String(options.prompt ?? '').trim() === pending.wire;
    const turn = await this.runtime.beginTerminalTurn({ ...options, prompt: matches ? pending.text : options.prompt, senderCharacterID: matches ? pending.senderCharacterID : null });
    state.runningTurnID = turn.turnID;
    if (matches) {
      clearTimeout(pending.timer);
      state.dispatch = null;
      pending.resolve({ turnId: turn.turnID, conversationId: state.conversationID, status: 'running' });
    }
    return turn;
  }

  cancelDispatch(state) {
    const pending = state.dispatch;
    if (!pending) return;
    clearTimeout(pending.timer);
    state.dispatch = null;
    pending.reject(new AgentBusyError('터미널 전달이 취소되었습니다.'));
  }

  get size() { return this.sessions.size + this.opening.size; }
  has(characterID) {
    const id = String(characterID ?? "");
    return this.sessions.has(id) || this.opening.has(id);
  }

  list() {
    return [...this.sessions.values()].map((state) => ({
      terminalSessionId: state.terminalSessionID,
      characterId: state.characterID,
      backend: state.backend,
      externalSessionId: state.externalSessionID,
      conversationId: state.conversationID,
      runningTurnId: state.runningTurnID,
      startedAt: new Date(state.startedAt).toISOString(),
    }));
  }

  async open(characterID) {
    const id = String(characterID ?? "").trim();
    if (!id) throw new CharacterNotFoundError("캐릭터를 찾을 수 없습니다.");
    if (this.has(id)) throw new AgentBusyError("터미널 모드에서 사용 중입니다.");
    this.opening.add(id);
    let state = null;
    try {
      const launch = await this.runtime.prepareTerminalLaunch(id);
      const character = launch.character;
      const terminalSessionID = randomUUID();
      state = {
        terminalSessionID,
        ownerToken: randomBytes(32).toString('hex'),
        characterID: id,
        character,
        backend: character.backend,
        sessionID: launch.sessionID,
        conversationID: launch.conversationID,
        externalSessionID: launch.externalSessionID,
        workdir: launch.workdir,
        startedAt: Date.now(),
        runningTurnID: null,
        lastCompletedTurnID: null,
        // Claude 상태줄이 알려준 세션 누적 비용과 이미 턴에 반영한 몫이다.
        claudeCost: { reported: null, settled: 0 },
        initialGeneratedImages: new Set(listGeneratedImages(
          launch.externalSessionID,
          generatedImageRoot(character.backend),
        )),
        closed: false,
        watcher: null,
        codexTurnID: null,
        activities: new TerminalActivityCollector(launch.workdir),
        codexGate: character.backend === "codex"
          ? new CodexTerminalTurnGate({
            cwd: launch.workdir,
            externalSessionID: launch.externalSessionID,
            sessionsRoot: this.codexSessionsRoot,
          })
          : null,
        structuredResultPath: prepareStructuredTurnResult({
          workdir: launch.workdir,
          characterID: id,
        }),
      };
      this.sessions.set(id, state);
      state.beginTurn = options => this.beginDispatchedTurn(state, options);
      if (character.backend === "antigravity") {
        state.watcher = new AntigravityTerminalWatcher({
          state,
          runtime: this.runtime,
          root: this.antigravityRoot,
          pollIntervalMs: this.antigravityPollIntervalMs,
          debounceMs: this.antigravityDebounceMs,
        });
        state.watcher.start();
      } else if (character.backend === "codex") {
        state.watcher = new CodexTerminalWatcher({
          state,
          runtime: this.runtime,
          broadcast: (event) => this.broadcast(event),
          sessionsRoot: this.codexSessionsRoot,
        });
        state.watcher.start();
      }
      const env = terminalEnvironment(character, {
        baseEnvironment: this.baseEnvironment,
        workdir: launch.workdir,
        characterID: id,
        port: this.port,
      });
      const spec = {
        terminalSessionId: terminalSessionID,
        ownerToken: state.ownerToken,
        executable: locateExecutable(character),
        args: terminalArguments({
          character,
          previousSessionID: launch.externalSessionID,
          workdir: launch.workdir,
        }),
        cwd: launch.workdir,
        env,
        externalSessionId: launch.externalSessionID,
        conversationId: launch.conversationID,
        backend: character.backend,
      };
      if (character.localProfile) {
        if (!this.runtime.localProviders) throw new Error('Local provider service unavailable');
        const baseEnvironment = env;
        const local = await this.runtime.localProviders.launch({character,mode:'terminal',previousSessionID:launch.externalSessionID,workdir:launch.workdir,baseEnvironment});
        state.localRelease = local.release;
        spec.executable = local.executable;
        spec.args = local.args;
        spec.env = local.env;
      }
      this.broadcast({ type: "terminal.changed", characterId: id });
      return spec;
    } catch (error) {
      if (state) {
        state.closed = true;
        await state.localRelease?.();
        try { await state.watcher?.stop({ finalSweep: false }); } catch {}
        discardStructuredTurnResult(state.structuredResultPath);
      }
      this.sessions.delete(id);
      if (error instanceof AgentDrainingError) throw error;
      throw error;
    } finally {
      this.opening.delete(id);
    }
  }

  async bindIfNeeded(state, externalSessionID) {
    const id = String(externalSessionID ?? "").trim();
    if (!id || state.externalSessionID === id) return;
    await this.runtime.bindTerminalExternalSession({
      characterID: state.characterID,
      sessionID: state.sessionID,
      externalSessionID: id,
    });
    state.externalSessionID = id;
  }

  resetTurnArtifacts(state) {
    resetTerminalArtifacts(state);
  }

  async handleEvent(characterID, body) {
    const state = this.sessions.get(String(characterID ?? ""));
    if (!state || state.closed) {
      throw new Error("열린 터미널 세션을 찾을 수 없습니다.");
    }
    const source = String(body?.source ?? "");
    if (source === "claude" && state.backend === "claude") {
      return await this.handleClaudeEvent(state, body.payload ?? {});
    }
    if (source === "codex" && state.backend === "codex") {
      return await this.handleCodexEvent(state, body.payload ?? {});
    }
    throw new Error("터미널 CLI와 이벤트 종류가 일치하지 않습니다.");
  }

  async handleClaudeEvent(state, payload) {
    // 상태줄 payload에는 hook_event_name이 없고 cost 객체가 있다.
    const event = String(payload.hook_event_name ?? "") ||
      (payload.cost && typeof payload.cost === "object" ? "StatusLine" : "");
    await this.bindIfNeeded(state, payload.session_id);
    if (event === "StatusLine") {
      return await this.recordClaudeSessionCost(state, payload.cost);
    }
    if (event === "UserPromptSubmit") {
      // Esc로 중단된 턴은 Stop 훅이 오지 않아 running으로 남는다. 새 질문이
      // 제출됐다는 것은 이전 턴이 끝났다는 뜻이므로, 남은 턴을 중단 처리하고
      // 세션을 풀어 이 질문부터 다시 기록한다.
      if (state.runningTurnID) {
        await this.runtime.interruptTerminalTurn(
          state.characterID,
          state.runningTurnID,
        );
        state.runningTurnID = null;
        this.resetTurnArtifacts(state);
      }
      const path = payload.transcript_path;
      let offset = 0;
      try { if (path) offset = statSync(path).size; } catch {}
      state.claudeTranscript = { path, offset };
      const turn = await this.beginDispatchedTurn(state, {
        characterID: state.characterID,
        sessionID: state.sessionID,
        prompt: payload.prompt,
        execution: state.character,
      });
      state.runningTurnID = turn.turnID;
      this.broadcast({ type: "terminal.changed", characterId: state.characterID });
      return { accepted: true, turnId: turn.turnID };
    }
    if (event !== "Stop" || !state.runningTurnID) {
      return { accepted: false, reason: "ignored-hook" };
    }
    const turnID = state.runningTurnID;
    const response = String(payload.last_assistant_message ?? "").trim() ||
      lastClaudeAssistantMessage(payload.transcript_path);
    // 시작 훅이 지목한 같은 원본만 읽는다. 경로가 다르면 과거 세션을 섞지 않는다.
    const activities = state.claudeTranscript?.path &&
        state.claudeTranscript.path === payload.transcript_path
      ? await readClaudeTerminalActivities(payload.transcript_path, {
        offset: state.claudeTranscript.offset,
        sessionID: state.externalSessionID,
        workdir: state.workdir,
        finalResponse: response,
      }) : [];
    const structured = consumeStructuredTurnResult(
      state.structuredResultPath,
    );
    state.structuredResultPath = null;
    // 완료 저장이 실패해도 턴을 running으로 두면 안 된다. 세션이 그 턴에
    // 묶여 다음 질문까지 전부 turn-running으로 거절된다.
    try {
      await this.runtime.completeTerminalTurn({
        characterID: state.characterID,
        turnID,
        response,
        structured,
        initialGeneratedImages: state.initialGeneratedImages,
        reportedCostUsd: this.settleClaudeTurnCost(state),
        activities,
      });
      state.runningTurnID = null;
      state.lastCompletedTurnID = turnID;
      this.resetTurnArtifacts(state);
    } catch (error) {
      await this.runtime.interruptTerminalTurn(state.characterID, turnID);
      state.runningTurnID = null;
      throw error;
    } finally {
      this.broadcast({
        type: "terminal.changed",
        characterId: state.characterID,
      });
    }
    return { accepted: true, turnId: turnID };
  }

  // Claude Code 상태줄은 cost.total_cost_usd로 세션 누적 비용을 알려준다.
  // 값이 줄어들면 CLI 프로세스가 새로 시작한 것(resume 포함)이라 기준을
  // 0으로 되돌린다. 실측상 턴 마지막 갱신은 Stop 훅보다 늦게 오므로, 진행
  // 중인 턴이 없으면 방금 끝난 턴에 차액을 더한다.
  async recordClaudeSessionCost(state, cost) {
    const total = nonnegativeCost(cost?.total_cost_usd);
    if (total === null) return { accepted: false, reason: "no-cost" };
    const tracker = state.claudeCost;
    if (total < tracker.settled) tracker.settled = 0;
    tracker.reported = total;
    if (state.runningTurnID) {
      return { accepted: true, turnId: state.runningTurnID };
    }
    const delta = total - tracker.settled;
    tracker.settled = total;
    if (delta <= 0 || !state.lastCompletedTurnID) {
      return { accepted: true, turnId: null };
    }
    await this.runtime.addTerminalTurnReportedCost({
      characterID: state.characterID,
      turnID: state.lastCompletedTurnID,
      costUsd: delta,
    });
    return { accepted: true, turnId: state.lastCompletedTurnID };
  }

  // 지금까지 보고된 누적 비용 중 아직 어느 턴에도 반영하지 않은 몫이다.
  // 상태줄이 한 번도 오지 않았으면 null을 돌려 비용을 꾸며내지 않는다.
  settleClaudeTurnCost(state) {
    const tracker = state.claudeCost;
    if (tracker.reported === null) return null;
    const delta = Math.max(tracker.reported - tracker.settled, 0);
    tracker.settled = tracker.reported;
    return delta;
  }

  async handleCodexEvent(state, payload) {
    const result = await state.codexGate.accept(payload);
    if (!result.accepted) {
      console.info(
        `Codex 터미널 notify 폐기(${state.characterID}): ${result.reason}`,
      );
      return result;
    }
    if (result.boundExternalSessionID) {
      await this.bindIfNeeded(state, result.boundExternalSessionID);
    }
    const turn = result.turn;
    state.watcher?.markCompleted(turn.turnID);
    // 워처가 이미 이 턴을 running으로 만들었으면 그 턴을 완료한다. 다른 턴이
    // running으로 남아 있으면(그 notify가 버려진 경우) 세션이 묶이지 않게
    // 먼저 중단하고 이 notify 기준으로 처음부터 기록한다.
    let turnID;
    if (state.runningTurnID && state.codexTurnID === turn.turnID) {
      turnID = state.runningTurnID;
    } else {
      if (state.runningTurnID) {
        await this.runtime.interruptTerminalTurn(
          state.characterID,
          state.runningTurnID,
        );
        state.runningTurnID = null;
      }
      const started = await this.beginDispatchedTurn(state, {
        characterID: state.characterID,
        sessionID: state.sessionID,
        prompt: turn.prompt,
        startedAt: dateFromEpoch(turn.startedAt),
        execution: state.character,
      });
      turnID = started.turnID;
      state.runningTurnID = turnID;
    }
    state.codexTurnID = null;
    try {
      const structured = consumeStructuredTurnResult(
        state.structuredResultPath,
      );
      state.structuredResultPath = null;
      await this.runtime.completeTerminalTurn({
        characterID: state.characterID,
        turnID,
        response: turn.response,
        activities: turn.activities,
        structured,
        endedAt: dateFromEpoch(turn.completedAt),
        initialGeneratedImages: state.initialGeneratedImages,
      });
      state.runningTurnID = null;
      this.resetTurnArtifacts(state);
    } catch (error) {
      await this.runtime.interruptTerminalTurn(state.characterID, turnID);
      state.runningTurnID = null;
      throw error;
    } finally {
      this.broadcast({ type: "terminal.changed", characterId: state.characterID });
    }
    return { accepted: true, turnId: turnID };
  }

  // 앱이 터미널에 Esc를 보낸 뒤 호출한다. Claude는 중단 때 Stop 훅이 오지
  // 않으므로 여기서 턴을 중단 처리해야 화면과 예약이 풀린다. Codex는 뒤따르는
  // turn_aborted가 runningTurnID를 못 찾아 그대로 지나간다.
  async interrupt(characterID) {
    const id = String(characterID ?? "");
    const state = this.sessions.get(id);
    if (!state || state.closed) {
      throw new Error("열린 터미널 세션을 찾을 수 없습니다.");
    }
    this.cancelDispatch(state);
    const turnID = state.runningTurnID;
    if (!turnID) return { interrupted: false, turnId: null };
    await this.runtime.interruptTerminalTurn(id, turnID);
    state.runningTurnID = null;
    state.codexTurnID = null;
    if (state.backend === "antigravity") state.watcher?.forgetCurrentTurn();
    this.resetTurnArtifacts(state);
    this.broadcast({ type: "terminal.changed", characterId: id });
    return { interrupted: true, turnId: turnID };
  }

  async close(characterID) {
    const id = String(characterID ?? "");
    const state = this.sessions.get(id);
    if (!state) return false;
    state.closing = true;
    this.cancelDispatch(state);
    if (state.watcher) await state.watcher.stop({ finalSweep: true });
    state.closed = true;
    await state.localRelease?.();
    discardStructuredTurnResult(state.structuredResultPath);
    if (state.runningTurnID) {
      await this.runtime.interruptTerminalTurn(id, state.runningTurnID);
      state.runningTurnID = null;
    }
    this.sessions.delete(id);
    this.broadcast({ type: "terminal.changed", characterId: id });
    return true;
  }

  async shutdown() {
    for (const id of [...this.sessions.keys()]) await this.close(id);
  }
}
