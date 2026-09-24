import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { readClaudeCostState } from './claude-cost-state.mjs';

const DEFAULT_SUGGESTION_GRACE_MS = 3_000;
// 중단 제어 요청을 보내면 Claude Code는 보통 수십 ms 안에 비용이 담긴
// result를 돌려준다. 이 시간 안에 오지 않으면 프로세스를 그대로 종료한다.
const DEFAULT_INTERRUPT_RESULT_TIMEOUT_MS = 3_000;
const FORCE_KILL_DELAY_MS = 5_000;
const COMPACT_PROMPT =
  "/compact OFFICESTRA 직원 이름과 직급 호칭을 쓰지 말고, " +
  "사용자는 '사용자'로 표현하며 요청·결정·진행 상태만 " +
  "내용 중심으로 요약해 주세요.";

export class ClaudePersistentWorker {
  constructor({
    executable,
    argumentsList,
    cwd,
    env,
    signature,
    sessionID = null,
    resumeTranscriptPath = null,
    onExit = () => {},
    spawnProcess = spawn,
    suggestionGraceMs = DEFAULT_SUGGESTION_GRACE_MS,
    interruptResultTimeoutMs = DEFAULT_INTERRUPT_RESULT_TIMEOUT_MS,
  }) {
    this.signature = signature;
    this.cwd = cwd;
    this.sessionID = sessionID;
    this.onExit = onExit;
    this.suggestionGraceMs = suggestionGraceMs;
    this.interruptResultTimeoutMs = interruptResultTimeoutMs;
    this.current = null;
    this.closed = false;
    this.exitNotified = false;
    this.terminalError = null;
    this.stderrTail = "";
    this.cumulativeUsage = readClaudeCostState(resumeTranscriptPath, sessionID);
    this.resumedWithoutCostBaseline = Boolean(sessionID && !this.cumulativeUsage);

    this.child = spawnProcess(executable, argumentsList, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
      detached: process.platform !== "win32",
    });
    this.child.stderr?.on("data", (chunk) => {
      this.stderrTail = `${this.stderrTail}${String(chunk ?? "")}`
        .slice(-8_192);
    });
    this.child.once("error", (error) => {
      this.fail(error);
    });
    this.child.once("close", (code, signal) => {
      if (this.closed) {
        this.notifyExit();
        return;
      }
      const diagnostic = this.stderrTail.trim();
      const suffix = diagnostic ? ` ${diagnostic}` : "";
      this.fail(
        new Error(
          signal
            ? `Claude Code 지속 세션이 ${signal} 신호로 종료됐습니다.${suffix}`
            : `Claude Code 지속 세션이 종료 코드 ${code ?? "unknown"}로 끝났습니다.${suffix}`,
        ),
      );
    });
    void this.readOutput();
  }

  matches({ signature, sessionID }) {
    if (this.closed || this.terminalError || this.signature !== signature) {
      return false;
    }
    const requestedSessionID = String(sessionID ?? "").trim() || null;
    const workerSessionID = String(this.sessionID ?? "").trim() || null;
    return workerSessionID === requestedSessionID;
  }

  runTurn({ prompt, onLine }) {
    if (this.closed || this.terminalError) {
      return Promise.reject(
        this.terminalError ?? new Error("Claude Code 지속 세션이 종료됐습니다."),
      );
    }
    if (this.current) {
      return Promise.reject(
        new Error("Claude Code 지속 세션이 이전 업무를 아직 처리 중입니다."),
      );
    }
    let current;
    const promise = new Promise((resolve, reject) => {
      current = {
        onLine,
        resolve,
        reject,
        resultSeen: false,
        suggestionTimer: null,
      };
    });
    // 중단 요청은 turn이 어떻게든 끝난 시점만 기다린다.
    current.settled = promise.then(() => {}, () => {});
    this.current = current;
    const message = {
      type: "user",
      message: {
        role: "user",
        content: String(prompt ?? ""),
      },
      parent_tool_use_id: null,
    };
    try {
      if (!this.child.stdin?.writable) {
        throw new Error("Claude Code 지속 세션의 입력 스트림이 닫혔습니다.");
      }
      this.child.stdin.write(`${JSON.stringify(message)}\n`, (error) => {
        if (error) {
          this.fail(error);
        }
      });
    } catch (error) {
      this.fail(error);
    }
    return promise;
  }

  async compact() {
    let boundary = null;
    let resultError = null;
    await this.runTurn({
      prompt: COMPACT_PROMPT,
      onLine: async (line) => {
        let event;
        try {
          event = JSON.parse(line);
        } catch {
          return;
        }
        if (event.type === "system" && event.subtype === "compact_boundary") {
          boundary = event;
        }
        if (
          event.type === "result" &&
          (event.is_error === true || event.subtype !== "success")
        ) {
          resultError = String(
            event.result ?? event.error ?? "Claude Code 컨텍스트 압축이 실패했습니다.",
          );
        }
      },
    });
    if (resultError) {
      throw new Error(resultError);
    }
    if (!boundary) {
      throw new Error("Claude Code가 압축 완료 경계를 보고하지 않았습니다.");
    }
    const metadata = boundary.compact_metadata ??
      boundary.compactMetadata ?? {};
    return {
      preTokens: finiteTokenCount(
        metadata.pre_tokens ?? metadata.preTokens,
      ),
      postTokens: finiteTokenCount(
        metadata.post_tokens ?? metadata.postTokens,
      ),
    };
  }

  // 프로세스를 바로 죽이면 result가 오지 않아 중단된 턴의 비용이 비어
  // 남는다. 먼저 interrupt 제어 요청을 보내 비용과 모델별 토큰이 담긴
  // result를 받은 뒤 종료한다. 제한 시간 안에 오지 않으면 그대로 끝낸다.
  async cancelCurrent() {
    const reason = new Error("사용자가 Claude Code 업무를 중단했습니다.");
    const current = this.current;
    if (current && !this.closed && this.requestInterrupt()) {
      let timer;
      await Promise.race([
        current.settled,
        new Promise((resolve) => {
          timer = setTimeout(resolve, this.interruptResultTimeoutMs);
          timer.unref?.();
        }),
      ]);
      clearTimeout(timer);
    }
    this.close(reason);
  }

  requestInterrupt() {
    if (!this.child.stdin?.writable) {
      return false;
    }
    const request = {
      type: "control_request",
      request_id: `interrupt-${Date.now()}`,
      request: { subtype: "interrupt" },
    };
    try {
      this.child.stdin.write(`${JSON.stringify(request)}\n`);
      return true;
    } catch {
      return false;
    }
  }

  close(reason = new Error("Claude Code 지속 세션을 종료했습니다.")) {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.rejectCurrent(reason);
    try {
      this.child.stdin?.end();
    } catch {
      // 이미 종료 중인 stdin은 무시한다.
    }
    terminateChild(this.child);
    this.notifyExit();
  }

  async readOutput() {
    const lines = createInterface({
      input: this.child.stdout,
      crlfDelay: Infinity,
    });
    try {
      for await (const line of lines) {
        await this.handleLine(line);
      }
    } catch (error) {
      this.fail(error);
    }
  }

  async handleLine(line) {
    let object;
    try {
      object = JSON.parse(line);
    } catch {
      return;
    }
    if (object.type === "system" && object.subtype === "init") {
      const sessionID = String(object.session_id ?? "").trim();
      if (sessionID) {
        this.sessionID = sessionID;
      }
    }
    if (object.type === "result") {
      object = this.turnScopedResult(object);
    }

    const current = this.current;
    if (!current) {
      return;
    }
    // 정상 suggestion은 해당 turn의 result 뒤에 온다. 이전 turn의 grace
    // window를 넘겨 늦게 온 suggestion이 다음 turn에 붙는 일은 막는다.
    if (object.type === "prompt_suggestion" && !current.resultSeen) {
      return;
    }
    try {
      await current.onLine(JSON.stringify(object));
    } catch (error) {
      this.fail(error);
      return;
    }

    if (object.type === "prompt_suggestion" && current.resultSeen) {
      this.finishCurrent(current);
      return;
    }
    if (object.type !== "result") {
      return;
    }
    current.resultSeen = true;
    if (object.is_error === true || object.subtype !== "success") {
      this.finishCurrent(current);
      return;
    }
    current.suggestionTimer = setTimeout(() => {
      this.finishCurrent(current);
    }, this.suggestionGraceMs);
    current.suggestionTimer.unref?.();
  }

  turnScopedResult(result) {
    const scoped = scopedClaudeCumulativeResult(
      result,
      this.cumulativeUsage,
    );
    this.cumulativeUsage = scoped.cumulative;
    if (this.resumedWithoutCostBaseline) {
      this.resumedWithoutCostBaseline = false;
      // The first resumed total may include old turns. Keep this turn's usage
      // but leave cost unknown if its persisted baseline cannot be recovered.
      const safe = { ...scoped.result, total_cost_usd: null };
      delete safe.modelUsage;
      delete safe.model_usage;
      return safe;
    }
    return scoped.result;
  }

  finishCurrent(expected) {
    if (this.current !== expected) {
      return;
    }
    if (expected.suggestionTimer) {
      clearTimeout(expected.suggestionTimer);
    }
    this.current = null;
    expected.resolve();
  }

  rejectCurrent(error) {
    const current = this.current;
    if (!current) {
      return;
    }
    if (current.suggestionTimer) {
      clearTimeout(current.suggestionTimer);
    }
    this.current = null;
    current.reject(error);
  }

  fail(error) {
    if (this.terminalError) {
      return;
    }
    this.terminalError = error instanceof Error
      ? error
      : new Error(String(error));
    this.closed = true;
    this.rejectCurrent(this.terminalError);
    terminateChild(this.child);
    this.notifyExit();
  }

  notifyExit() {
    if (this.exitNotified) {
      return;
    }
    this.exitNotified = true;
    this.onExit(this);
  }
}

function finiteTokenCount(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? Math.round(value)
    : null;
}

export function scopedClaudeCumulativeResult(result, previous = null) {
  const modelUsageKey = result?.modelUsage &&
      typeof result.modelUsage === "object"
    ? "modelUsage"
    : result?.model_usage && typeof result.model_usage === "object"
      ? "model_usage"
      : null;
  const cumulative = {
    totalCostUsd: finiteNumber(result?.total_cost_usd),
    modelUsage: modelUsageKey
      ? cloneModelUsage(result[modelUsageKey])
      : {},
  };
  if (!previous) {
    return { result, cumulative };
  }

  const reset = cumulative.totalCostUsd !== null &&
    previous.totalCostUsd !== null &&
    cumulative.totalCostUsd < previous.totalCostUsd;
  const scoped = { ...result };
  if (!reset && cumulative.totalCostUsd !== null) {
    scoped.total_cost_usd = cumulativeDelta(
      cumulative.totalCostUsd,
      previous.totalCostUsd,
    );
  }
  if (modelUsageKey) {
    scoped[modelUsageKey] = {};
    for (const [model, usage] of Object.entries(result[modelUsageKey])) {
      const prior = reset ? null : previous.modelUsage?.[model];
      scoped[modelUsageKey][model] = scopedModelUsage(usage, prior);
    }
  }
  return { result: scoped, cumulative };
}

function cloneModelUsage(value) {
  return Object.fromEntries(
    Object.entries(value ?? {}).map(([model, usage]) => [
      model,
      usage && typeof usage === "object" ? { ...usage } : usage,
    ]),
  );
}

function scopedModelUsage(usage, previous) {
  if (!usage || typeof usage !== "object" || Array.isArray(usage)) {
    return usage;
  }
  const result = { ...usage };
  const cumulativeFields = [
    "inputTokens",
    "input_tokens",
    "outputTokens",
    "output_tokens",
    "cacheReadInputTokens",
    "cache_read_input_tokens",
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "costUSD",
    "cost_usd",
  ];
  for (const field of cumulativeFields) {
    const value = finiteNumber(usage[field]);
    if (value === null) {
      continue;
    }
    result[field] = cumulativeDelta(value, finiteNumber(previous?.[field]));
  }
  return result;
}

function cumulativeDelta(value, previous) {
  if (previous === null || value < previous) {
    return value;
  }
  return value - previous;
}

function finiteNumber(value) {
  if (value === null || value === undefined || value === '') return null;
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : null;
}

function terminateChild(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  try {
    if (process.platform === "win32" || !Number.isInteger(child.pid)) {
      child.kill("SIGTERM");
    } else {
      process.kill(-child.pid, "SIGTERM");
    }
  } catch {
    try {
      child.kill("SIGTERM");
    } catch {
      return;
    }
  }
  const timer = setTimeout(() => {
    if (child.exitCode !== null) {
      return;
    }
    try {
      if (process.platform === "win32" || !Number.isInteger(child.pid)) {
        child.kill("SIGKILL");
      } else {
        process.kill(-child.pid, "SIGKILL");
      }
    } catch {
      try {
        child.kill("SIGKILL");
      } catch {
        // 이미 끝난 프로세스는 무시한다.
      }
    }
  }, FORCE_KILL_DELAY_MS);
  timer.unref?.();
}
