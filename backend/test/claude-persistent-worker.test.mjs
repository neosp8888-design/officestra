import assert from "node:assert/strict";
import { EventEmitter, once } from "node:events";
import { PassThrough } from "node:stream";
import test from "node:test";

import {
  ClaudePersistentWorker,
  scopedClaudeCumulativeResult,
} from "../src/claude-persistent-worker.mjs";

class FakeChild extends EventEmitter {
  constructor(context) {
    super();
    this.stdin = new PassThrough();
    this.stdout = new PassThrough();
    this.stderr = new PassThrough();
    this.pid = undefined;
    this.exitCode = null;
    this.killed = false;
    // 실제 ChildProcess의 열린 pipe처럼 unref 타이머를 기다리는 동안에도
    // 테스트 이벤트 루프를 유지한다. 실패한 테스트에서도 반드시 정리한다.
    this.keepAlive = setInterval(() => {}, 1_000);
    context.after(() => this.kill("SIGTERM"));
  }

  kill(signal) {
    if (this.killed) return true;
    clearInterval(this.keepAlive);
    this.killed = true;
    this.exitCode = signal === "SIGKILL" ? 137 : 0;
    this.stdout.end();
    this.stderr.end();
    queueMicrotask(() => this.emit("close", this.exitCode, signal));
    return true;
  }
}

function emit(child, object) {
  child.stdout.write(`${JSON.stringify(object)}\n`);
}

async function submittedMessage(child, run) {
  const input = once(child.stdin, "data");
  const promise = run();
  const [chunk] = await input;
  return { message: JSON.parse(String(chunk)), promise };
}

test("같은 Claude 프로세스가 연속 turn을 받고 누적 사용량은 turn별 증분으로 전달한다", async (context) => {
  const child = new FakeChild(context);
  let spawnCount = 0;
  const received = [];
  const worker = new ClaudePersistentWorker({
    executable: "claude",
    argumentsList: ["-p"],
    cwd: "/repo",
    env: {},
    signature: "same",
    spawnProcess: () => {
      spawnCount += 1;
      return child;
    },
    suggestionGraceMs: 100,
  });

  const first = await submittedMessage(child, () => worker.runTurn({
    prompt: "첫 질문",
    onLine: async (line) => received.push(JSON.parse(line)),
  }));
  assert.equal(first.message.message.content, "첫 질문");
  emit(child, {
    type: "system",
    subtype: "init",
    session_id: "session-1",
  });
  emit(child, {
    type: "result",
    subtype: "success",
    is_error: false,
    result: "첫 답",
    total_cost_usd: 1.2,
    usage: { input_tokens: 10, output_tokens: 2 },
    modelUsage: {
      "claude-opus-5": {
        inputTokens: 10,
        outputTokens: 2,
        cacheReadInputTokens: 100,
        cacheCreationInputTokens: 900,
        costUSD: 1.2,
      },
    },
  });
  emit(child, {
    type: "prompt_suggestion",
    suggestion: "첫 추천",
    uuid: "suggestion-1",
  });
  await first.promise;

  const second = await submittedMessage(child, () => worker.runTurn({
    prompt: "둘째 질문",
    onLine: async (line) => received.push(JSON.parse(line)),
  }));
  assert.equal(second.message.message.content, "둘째 질문");
  emit(child, {
    type: "result",
    subtype: "success",
    is_error: false,
    result: "둘째 답",
    total_cost_usd: 1.5,
    usage: { input_tokens: 1, output_tokens: 3 },
    modelUsage: {
      "claude-opus-5": {
        inputTokens: 11,
        outputTokens: 5,
        cacheReadInputTokens: 1_050,
        cacheCreationInputTokens: 920,
        costUSD: 1.5,
      },
    },
  });
  emit(child, {
    type: "prompt_suggestion",
    suggestion: "둘째 추천",
    uuid: "suggestion-2",
  });
  await second.promise;

  assert.equal(spawnCount, 1);
  assert.equal(worker.sessionID, "session-1");
  assert.equal(worker.matches({ signature: "same", sessionID: "session-1" }), true);
  assert.equal(worker.matches({ signature: "same", sessionID: null }), false);
  assert.equal(worker.matches({ signature: "same", sessionID: "session-2" }), false);
  const results = received.filter((event) => event.type === "result");
  assert.equal(results[0].total_cost_usd, 1.2);
  assert.ok(Math.abs(results[1].total_cost_usd - 0.3) < 1e-9);
  assert.deepEqual(results[1].usage, { input_tokens: 1, output_tokens: 3 });
  assert.deepEqual({
    ...results[1].modelUsage["claude-opus-5"],
    costUSD: 0.3,
  }, {
    inputTokens: 1,
    outputTokens: 3,
    cacheReadInputTokens: 950,
    cacheCreationInputTokens: 20,
    costUSD: 0.3,
  });
  assert.ok(
    Math.abs(
      results[1].modelUsage["claude-opus-5"].costUSD - 0.3,
    ) < 1e-9,
  );

  worker.close();
  assert.equal(child.killed, true);
});

test("이전 turn에서 늦은 추천은 다음 turn에 붙이지 않는다", async (context) => {
  const child = new FakeChild(context);
  const worker = new ClaudePersistentWorker({
    executable: "claude",
    argumentsList: ["-p"],
    cwd: "/repo",
    env: {},
    signature: "same",
    spawnProcess: () => child,
    suggestionGraceMs: 5,
  });
  const firstEvents = [];
  const first = await submittedMessage(child, () => worker.runTurn({
    prompt: "첫 질문",
    onLine: async (line) => firstEvents.push(JSON.parse(line)),
  }));
  emit(child, {
    type: "result",
    subtype: "success",
    is_error: false,
    result: "첫 답",
    total_cost_usd: 0.1,
  });
  await first.promise;

  const secondEvents = [];
  const second = await submittedMessage(child, () => worker.runTurn({
    prompt: "둘째 질문",
    onLine: async (line) => secondEvents.push(JSON.parse(line)),
  }));
  emit(child, {
    type: "prompt_suggestion",
    suggestion: "늦은 첫 추천",
    uuid: "late-1",
  });
  emit(child, {
    type: "result",
    subtype: "success",
    is_error: false,
    result: "둘째 답",
    total_cost_usd: 0.2,
  });
  emit(child, {
    type: "prompt_suggestion",
    suggestion: "둘째 추천",
    uuid: "suggestion-2",
  });
  await second.promise;

  assert.equal(
    secondEvents.some((event) => event.suggestion === "늦은 첫 추천"),
    false,
  );
  assert.equal(
    secondEvents.some((event) => event.suggestion === "둘째 추천"),
    true,
  );
  worker.close();
});

test("누적 집계가 감소하면 새 query 기준으로 그대로 사용한다", () => {
  const previous = {
    totalCostUsd: 2,
    modelUsage: {
      model: { inputTokens: 200, costUSD: 2 },
    },
  };
  const scoped = scopedClaudeCumulativeResult({
    type: "result",
    total_cost_usd: 0.4,
    modelUsage: {
      model: { inputTokens: 40, costUSD: 0.4 },
    },
  }, previous);
  assert.equal(scoped.result.total_cost_usd, 0.4);
  assert.equal(scoped.result.modelUsage.model.inputTokens, 40);
  assert.equal(scoped.result.modelUsage.model.costUSD, 0.4);
});

test("Claude 수동 압축은 /compact와 완료 경계의 토큰을 사용한다", async (context) => {
  const child = new FakeChild(context);
  const worker = new ClaudePersistentWorker({
    executable: "claude",
    argumentsList: ["-p"],
    cwd: "/repo",
    env: {},
    signature: "compact",
    sessionID: "session-1",
    spawnProcess: () => child,
    suggestionGraceMs: 5,
  });

  const submitted = await submittedMessage(child, () => worker.compact());
  assert.match(
    submitted.message.message.content,
    /^\/compact OFFICESTRA 직원 이름과 직급 호칭을 쓰지 말고/,
  );
  assert.equal(
    /백부장|클대리/.test(submitted.message.message.content),
    false,
  );
  emit(child, {
    type: "system",
    subtype: "compact_boundary",
    compact_metadata: {
      pre_tokens: 901_000,
      post_tokens: 42_000,
    },
  });
  emit(child, {
    type: "result",
    subtype: "success",
    is_error: false,
    result: "Compacted conversation",
    total_cost_usd: 0.1,
  });
  emit(child, {
    type: "prompt_suggestion",
    suggestion: "계속할까요?",
  });

  assert.deepEqual(await submitted.promise, {
    preTokens: 901_000,
    postTokens: 42_000,
  });
  worker.close();
});

test("중단 요청은 interrupt 제어 요청을 먼저 보내고 비용이 담긴 result를 받은 뒤 종료한다", async (context) => {
  const child = new FakeChild(context);
  const received = [];
  const worker = new ClaudePersistentWorker({
    executable: "claude",
    argumentsList: ["-p"],
    cwd: "/repo",
    env: {},
    signature: "same",
    spawnProcess: () => child,
    suggestionGraceMs: 100,
  });

  const submitted = await submittedMessage(child, () => worker.runTurn({
    prompt: "긴 작업",
    onLine: async (line) => received.push(JSON.parse(line)),
  }));
  assert.equal(submitted.message.type, "user");

  const interrupt = await submittedMessage(child, () => worker.cancelCurrent());
  assert.equal(interrupt.message.type, "control_request");
  assert.equal(interrupt.message.request.subtype, "interrupt");
  assert.equal(child.killed, false);

  // 실제 Claude Code 2.1.261은 interrupt 뒤 이 형태의 result를 보낸다.
  emit(child, {
    type: "result",
    subtype: "error_during_execution",
    is_error: true,
    result: "",
    total_cost_usd: 0.002706,
    usage: { input_tokens: 0, output_tokens: 0 },
    modelUsage: {
      "claude-sonnet-5": {
        inputTokens: 1_218,
        outputTokens: 27,
        costUSD: 0.002706,
      },
    },
  });

  await submitted.promise;
  await interrupt.promise;
  assert.equal(child.killed, true);
  const result = received.find((event) => event.type === "result");
  assert.equal(result.total_cost_usd, 0.002706);
  assert.equal(result.modelUsage["claude-sonnet-5"].inputTokens, 1_218);
});

test("중단 result가 제한 시간 안에 오지 않으면 프로세스를 그대로 종료한다", async (context) => {
  const child = new FakeChild(context);
  const worker = new ClaudePersistentWorker({
    executable: "claude",
    argumentsList: ["-p"],
    cwd: "/repo",
    env: {},
    signature: "same",
    spawnProcess: () => child,
    suggestionGraceMs: 100,
    interruptResultTimeoutMs: 20,
  });

  const submitted = await submittedMessage(child, () => worker.runTurn({
    prompt: "긴 작업",
    onLine: async () => {},
  }));
  const interrupt = await submittedMessage(child, () => worker.cancelCurrent());
  assert.equal(interrupt.message.type, "control_request");

  await interrupt.promise;
  assert.equal(child.killed, true);
  await assert.rejects(submitted.promise, /중단했습니다/);
});
