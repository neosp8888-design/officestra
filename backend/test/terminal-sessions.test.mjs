import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { appendFile, mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { submitStructuredResponseSource, STRUCTURED_RESULT_GUIDANCE } from "../src/structured-turn-result.mjs";

import {
  AgentBusyError,
  AgentRuntime,
} from "../src/agent-runtime.mjs";
import {
  TerminalSessionManager,
  parseAntigravityTerminalStep,
  terminalArguments,
} from "../src/terminal-sessions.mjs";

const baseCharacter = {
  id: "boss",
  model: "model-a",
  effort: "high",
  fastMode: false,
  permission: "danger-full-access",
  identityPrompt: "업무 지침",
  config: {},
};

test("세 CLI의 대화형 인자는 resume 유무와 권한을 보존한다", () => {
  for (const permission of ["read-only", "workspace-write", "danger-full-access"]) {
    const character = { ...baseCharacter, backend: "codex", permission };
    const fresh = terminalArguments({ character, workdir: "/tmp/office", hookPath: "/tmp/hook", nodePath: "/usr/bin/node" });
    const resumed = terminalArguments({ character, previousSessionID: "session", workdir: "/tmp/office", hookPath: "/tmp/hook", nodePath: "/usr/bin/node" });
    assert.equal(fresh.includes("resume"), false);
    assert.deepEqual(resumed.slice(0, 2), ["resume", "session"]);
    assert.equal(fresh[fresh.indexOf("-s") + 1], permission);
    assert.ok(fresh.some((value) => value.includes('notify=["/usr/bin/node","/tmp/hook"]')));
  }

  for (const permission of ["plan", "accept-edits", "dangerously-skip-permissions"]) {
    const character = { ...baseCharacter, backend: "antigravity", permission };
    const fresh = terminalArguments({ character, workdir: "/tmp/office" });
    const resumed = terminalArguments({ character, previousSessionID: "session", workdir: "/tmp/office" });
    assert.equal(fresh.includes("--conversation"), false);
    assert.equal(resumed[resumed.indexOf("--conversation") + 1], "session");
    assert.equal(fresh[fresh.indexOf("--add-dir") + 1], "/tmp/office");
  }

  for (const permission of ["plan", "acceptEdits", "bypassPermissions"]) {
    const character = { ...baseCharacter, backend: "claude", permission };
    const fresh = terminalArguments({ character, workdir: "/tmp/office" });
    const resumed = terminalArguments({ character, previousSessionID: "session", workdir: "/tmp/office" });
    assert.equal(fresh.includes("--resume"), false);
    assert.equal(resumed[resumed.indexOf("--resume") + 1], "session");
    assert.equal(fresh[fresh.indexOf("--permission-mode") + 1], permission);
    const settings = JSON.parse(fresh[fresh.indexOf("--settings") + 1]);
    assert.ok(settings.hooks.UserPromptSubmit);
    assert.ok(settings.hooks.Stop);
    assert.ok(fresh[fresh.indexOf("--append-system-prompt") + 1].includes(STRUCTURED_RESULT_GUIDANCE));
  }
});

test("터미널은 동적으로 발견된 모델 이름과 추론 레벨을 그대로 전달한다", () => {
  const codex = terminalArguments({
    character: {
      ...baseCharacter,
      backend: "codex",
      model: "gpt-future-mini",
      effort: "medium",
    },
    workdir: "/tmp/office",
  });
  assert.equal(codex.includes('model="gpt-future-mini"'), true);
  assert.equal(codex.includes('model_reasoning_effort="medium"'), true);

  const antigravity = terminalArguments({
    character: {
      ...baseCharacter,
      backend: "antigravity",
      model: "gemini-future-pro",
      effort: "low",
      permission: "accept-edits",
    },
    workdir: "/tmp/office",
  });
  assert.equal(
    antigravity[antigravity.indexOf("--model") + 1],
    "gemini-future-pro",
  );
  assert.equal(antigravity[antigravity.indexOf("--effort") + 1], "low");
});

test("열린 터미널은 두 번째 프로세스를 막고 진행 중 API 업무는 409다", async () => {
  const events = [];
  const runtime = fakeRuntime();
  const manager = new TerminalSessionManager({
    runtime,
    broadcast: (event) => events.push(event),
    baseEnvironment: { PATH: "/usr/bin", LANG: "ko_KR.UTF-8" },
  });
  runtime.registry = manager;
  await manager.open("boss");
  await assert.rejects(() => manager.open("boss"), AgentBusyError);

  const agentRuntime = new AgentRuntime({
    pool: {},
    withTransaction: async () => {},
    workdir: "/tmp/office",
    broadcast() {},
  });
  agentRuntime.setTerminalSessionRegistry(manager);
  manager.sessions.get('boss').runningTurnID = 'busy-turn';
  await assert.rejects(
    () => agentRuntime.start({ characterID: "boss", prompt: "GUI 업무" }),
    (error) => error instanceof AgentBusyError,
  );
  assert.equal(manager.list()[0].characterId, "boss");
  assert.equal(events.at(-1).type, "terminal.changed");
  await manager.close("boss");
});

test("Claude UserPromptSubmit과 Stop 훅이 같은 터미널 턴을 생성·완료한다", async () => {
  const runtime = fakeRuntime();
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  await manager.open("boss");
  const started = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "터미널 질문",
    },
  });
  assert.equal(started.accepted, true);
  const completed = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "Stop",
      session_id: "claude-session",
      last_assistant_message: "터미널 답변",
    },
  });
  assert.equal(completed.turnId, started.turnId);
  assert.deepEqual(runtime.begun.map((turn) => turn.prompt), ["터미널 질문"]);
  assert.deepEqual(runtime.completed.map((turn) => turn.response), ["터미널 답변"]);
  assert.deepEqual(runtime.bound.map((entry) => entry.externalSessionID), ["claude-session"]);
  await manager.close("boss");
});

test("Claude 시작 이후 활동과 명시적으로 제출한 근거가 같은 완료 턴으로 전달된다", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-terminal-history-"));
  const path = join(root, "session.jsonl");
  await writeFile(path, JSON.stringify({ type: "assistant", sessionId: "claude-session", message: { content: [{ type: "text", text: "이전 답변" }] } }) + "\n");
  const runtime = fakeRuntime({ workdir: root });
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  try {
    const spec = await manager.open("boss");
    assert.equal(spec.env.OFFICESTRA_RESULT_PATH, manager.sessions.get("boss").structuredResultPath);
    await manager.handleEvent("boss", { source: "claude", payload: { hook_event_name: "UserPromptSubmit", session_id: "claude-session", prompt: "질문", transcript_path: path } });
    await appendFile(path, JSON.stringify({ type: "assistant", sessionId: "claude-session", message: { id: "one", content: [{ type: "text", text: "확인 중" }, { type: "thinking", thinking: "공개 요약" }] } }) + "\n");
    submitStructuredResponseSource(spec.env.OFFICESTRA_RESULT_PATH, { kind: "file", title: "근거", locator: "source.mjs" });
    await manager.handleEvent("boss", { source: "claude", payload: { hook_event_name: "Stop", session_id: "claude-session", last_assistant_message: "완료", transcript_path: path } });
    assert.deepEqual(runtime.completed[0].activities.map((a) => a.kind).sort(), ["message", "thinking"]);
    assert.equal(runtime.completed[0].structured.sources[0].locator, "source.mjs");
    assert.equal(runtime.completed[0].response, "완료");
    await manager.handleEvent("boss", { source: "claude", payload: { hook_event_name: "UserPromptSubmit", session_id: "claude-session", prompt: "다음 질문", transcript_path: path } });
    await manager.handleEvent("boss", { source: "claude", payload: { hook_event_name: "Stop", session_id: "claude-session", last_assistant_message: "다음 답변", transcript_path: path } });
    assert.deepEqual(runtime.completed[1].activities, []);
    assert.deepEqual(runtime.completed[1].structured.sources, []);
  } finally { await manager.close("boss"); }
});

// Claude Code 상태줄은 세션 누적 비용만 알려주고, 턴 마지막 갱신은 실측상
// Stop 훅보다 늦게 온다. 턴 완료 시점의 차액을 저장하고 늦은 갱신은 직전
// 턴에 더하며, 누적값이 줄면 새 프로세스로 보고 기준을 되돌린다.
test("Claude 상태줄 누적 비용을 턴별 차액으로 나눠 저장하고 늦은 갱신은 직전 턴에 더한다", async () => {
  const runtime = fakeRuntime();
  const added = [];
  runtime.addTerminalTurnReportedCost = async (entry) => {
    added.push(entry);
    return true;
  };
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  await manager.open("boss");
  const status = (total) => manager.handleEvent("boss", {
    source: "claude",
    payload: {
      session_id: "claude-session",
      cost: { total_cost_usd: total, total_duration_ms: 10 },
    },
  });
  const prompt = (text) => manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: text,
    },
  });
  const stop = () => manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "Stop",
      session_id: "claude-session",
      last_assistant_message: "답변",
    },
  });

  await status(0);
  await prompt("첫 질문");
  await status(0.1);
  await stop();
  assert.equal(runtime.completed[0].reportedCostUsd, 0.1);
  // 턴이 끝난 뒤 도착한 마지막 갱신은 직전 턴에 차액만 더한다.
  await status(0.25);
  await status(0.25);
  assert.deepEqual(added, [{
    characterID: "boss",
    turnID: "turn-1",
    costUsd: 0.15,
  }]);
  // 두 번째 턴 중 누적값이 줄면 CLI가 새로 시작한 것이므로 0부터 센다.
  await prompt("둘째 질문");
  await status(0.05);
  await stop();
  assert.equal(runtime.completed[1].reportedCostUsd, 0.05);
  assert.equal(added.length, 1);
  await manager.close("boss");
});

test("Claude 상태줄이 한 번도 오지 않으면 터미널 턴 비용을 비워 둔다", async () => {
  const runtime = fakeRuntime();
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  await manager.open("boss");
  await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "질문",
    },
  });
  await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "Stop",
      session_id: "claude-session",
      last_assistant_message: "답변",
    },
  });
  assert.equal(runtime.completed[0].reportedCostUsd, null);
  const settings = JSON.parse(
    terminalArguments({
      character: { ...baseCharacter, backend: "claude" },
      workdir: "/tmp/office",
    }).find((value) => value.startsWith("{")),
  );
  assert.deepEqual(settings.statusLine, {
    type: "command",
    command: "officestra-terminal-hook",
  });
  await manager.close("boss");
});

// 완료 저장이 실패했을 때 턴을 running으로 남기면 세션이 그 턴에 묶여
// 이후 질문이 전부 turn-running으로 거절된다.
test("Claude Stop 훅의 완료 저장이 실패하면 턴을 중단 처리하고 세션을 푼다", async () => {
  const runtime = fakeRuntime();
  const interrupted = [];
  runtime.completeTerminalTurn = async () => {
    throw new Error("터미널 최종 메시지가 없습니다.");
  };
  runtime.interruptTerminalTurn = async (characterID, turnID) => {
    interrupted.push({ characterID, turnID });
    return true;
  };
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  await manager.open("boss");
  const started = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "첫 질문",
    },
  });

  await assert.rejects(() =>
    manager.handleEvent("boss", {
      source: "claude",
      payload: {
        hook_event_name: "Stop",
        session_id: "claude-session",
        last_assistant_message: "답변",
      },
    })
  );

  assert.deepEqual(interrupted, [{
    characterID: "boss",
    turnID: started.turnId,
  }]);
  // 다음 질문이 turn-running으로 막히지 않아야 한다.
  const next = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "다음 질문",
    },
  });
  assert.equal(next.accepted, true);
  assert.notEqual(next.turnId, started.turnId);
  await manager.close("boss");
});

// 앱의 멈춤 버튼은 터미널에 Esc를 보낸 뒤 이 경로를 부른다. Claude는 중단 때
// Stop 훅이 오지 않으므로 여기서 턴을 풀어야 화면과 예약이 이어진다.
test("터미널 중단 요청은 진행 중인 턴을 중단 처리하고 세션을 푼다", async () => {
  const events = [];
  const runtime = fakeRuntime();
  const manager = new TerminalSessionManager({
    runtime,
    broadcast: (event) => events.push(event),
  });
  await manager.open("boss");
  await assert.rejects(() => manager.interrupt("left-man"));
  assert.deepEqual(await manager.interrupt("boss"), {
    interrupted: false,
    turnId: null,
  });

  const started = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "긴 작업",
    },
  });
  assert.equal(manager.list()[0].runningTurnId, started.turnId);

  assert.deepEqual(await manager.interrupt("boss"), {
    interrupted: true,
    turnId: started.turnId,
  });
  assert.deepEqual(runtime.interrupted, [{
    characterID: "boss",
    turnID: started.turnId,
  }]);
  assert.equal(manager.list()[0].runningTurnId, null);
  assert.equal(events.at(-1).type, "terminal.changed");

  // 중단 뒤 도착하는 Stop 훅은 풀린 세션을 다시 잠그지 않는다.
  const late = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "Stop",
      session_id: "claude-session",
      last_assistant_message: "늦은 답변",
    },
  });
  assert.equal(late.accepted, false);
  assert.equal(runtime.completed.length, 0);

  const next = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "다음 질문",
    },
  });
  assert.equal(next.accepted, true);
  assert.notEqual(next.turnId, started.turnId);
  await manager.close("boss");
});

// 터미널에서 직접 Esc를 눌러도 Stop 훅은 오지 않는다. 그대로 두면 다음 질문이
// 전부 turn-running으로 버려져 기록이 끊긴다.
test("Stop 훅 없이 새 질문이 오면 남아 있던 Claude 턴을 중단 처리한다", async () => {
  const runtime = fakeRuntime();
  const manager = new TerminalSessionManager({ runtime, broadcast() {} });
  await manager.open("boss");
  const first = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "첫 질문",
    },
  });
  const second = await manager.handleEvent("boss", {
    source: "claude",
    payload: {
      hook_event_name: "UserPromptSubmit",
      session_id: "claude-session",
      prompt: "Esc 뒤 두 번째 질문",
    },
  });
  assert.equal(second.accepted, true);
  assert.notEqual(second.turnId, first.turnId);
  assert.deepEqual(runtime.interrupted, [{
    characterID: "boss",
    turnID: first.turnId,
  }]);
  assert.equal(manager.list()[0].runningTurnId, second.turnId);
  assert.deepEqual(runtime.begun.map((turn) => turn.prompt), [
    "첫 질문",
    "Esc 뒤 두 번째 질문",
  ]);
  await manager.close("boss");
});

test("Antigravity step 14/15 payload를 사용자·최종 응답으로 해독한다", () => {
  const user = parseAntigravityTerminalStep({
    step_type: 14,
    status: 3,
    step_payload: message(numberField(1, 14), bytesField(19, message(bytesField(2, Buffer.from("질문"))))),
  });
  const assistant = parseAntigravityTerminalStep({
    step_type: 15,
    status: 3,
    step_payload: message(numberField(1, 15), bytesField(20, message(bytesField(1, Buffer.from("답변"))))),
  });
  assert.deepEqual(user, { kind: "user", text: "질문" });
  assert.equal(assistant.kind, "assistant");
  assert.equal(assistant.text, "답변");
});

test("Antigravity 파일 알림을 놓쳐도 주기 확인으로 턴을 완료한다", async () => {
  const root = await mkdtemp(join(tmpdir(), "officestra-antigravity-terminal-"));
  const conversations = join(root, "conversations");
  const externalSessionID = "01a0c0de-3333-7000-8000-000000000003";
  const databasePath = join(conversations, `${externalSessionID}.db`);
  await mkdir(conversations, { recursive: true });
  const database = new DatabaseSync(databasePath);
  database.exec(`
    CREATE TABLE steps (
      idx INTEGER PRIMARY KEY,
      step_type INTEGER NOT NULL,
      status INTEGER NOT NULL,
      metadata BLOB,
      step_payload BLOB NOT NULL
    )
  `);
  const runtime = fakeRuntime({
    backend: "antigravity",
    externalSessionID,
    permission: "dangerously-skip-permissions",
  });
  const manager = new TerminalSessionManager({
    runtime,
    broadcast() {},
    antigravityRoot: root,
    antigravityPollIntervalMs: 10,
    antigravityDebounceMs: 50,
  });
  let notificationStorm = null;

  try {
    await manager.open("boss");
    const state = manager.sessions.get("boss");
    // 최초 sweep을 끝낸 뒤 파일 감시를 끊어 fs.watch 알림 누락을 재현한다.
    await delay(20);
    for (const watcher of state.watcher.watchers) watcher.close();
    state.watcher.watchers = [];
    // 실제 WAL/SHM처럼 debounce보다 잦은 알림이 와도 주기 확인은 굶으면
    // 안 된다. 기존 구현은 이 알림이 poll 타이머까지 계속 미뤘다.
    notificationStorm = setInterval(() => state.watcher.schedule(), 2);

    database.prepare(
      "INSERT INTO steps(idx, step_type, status, metadata, step_payload) VALUES (?, ?, ?, ?, ?)",
    ).run(
      1,
      14,
      3,
      null,
      message(
        numberField(1, 14),
        bytesField(19, message(bytesField(2, Buffer.from("질문")))),
      ),
    );
    await waitUntil(() => runtime.begun.length === 1);
    assert.equal(state.runningTurnID, "turn-1");
    submitStructuredResponseSource(state.structuredResultPath, { kind: "file", title: "근거", locator: "agy.mjs" });
    database.prepare("INSERT INTO steps(idx, step_type, status, metadata, step_payload) VALUES (?, ?, ?, ?, ?)")
      .run(2, 15, 3, null, message(numberField(1, 15), bytesField(20, message(bytesField(3, Buffer.from("공개 추론 요약"))))));
    database.prepare("INSERT INTO steps(idx, step_type, status, metadata, step_payload) VALUES (?, ?, ?, ?, ?)")
      .run(3, 132, 3, null, message(numberField(1, 132)));

    const assistantPayload = message(
      numberField(1, 15),
      bytesField(20, message(bytesField(1, Buffer.from("답변")))),
    );
    database.prepare(
      "INSERT INTO steps(idx, step_type, status, metadata, step_payload) VALUES (?, ?, ?, ?, ?)",
    ).run(
      4,
      15,
      1,
      null,
      assistantPayload,
    );
    // 미완료 행을 한 번 읽어 커서가 지나간 뒤 같은 idx가 완료되는 실제
    // Antigravity 기록 순서를 재현한다.
    await waitUntil(() => state.watcher.lastIndex === 4);
    assert.equal(runtime.completed.length, 0);
    database.prepare(
      "UPDATE steps SET status = 3, step_payload = ? WHERE idx = 4",
    ).run(assistantPayload);
    await waitUntil(() => runtime.completed.length === 1);
    assert.equal(runtime.completed[0].response, "답변");
    assert.deepEqual(runtime.completed[0].activities.map((a) => a.kind), ["thinking", "tool"]);
    assert.equal(runtime.completed[0].structured.sources[0].locator, "agy.mjs");
    // 다음 사용자 경계까지 같은 턴의 활동/근거를 보존한다.
    assert.equal(state.activities.finish("답변").length, 2);
    assert.equal(state.runningTurnID, null);
  } finally {
    clearInterval(notificationStorm);
    database.close();
    await manager.close("boss");
  }
});

test("Antigravity 비동기 작업 뒤 후속 응답은 같은 턴을 갱신하고 다음 질문에 섞이지 않는다", async (t) => {
  const h = await antigravityWatcherHarness(t);
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/antigravity-background-replies.json", import.meta.url)));
  for (const snapshot of fixture.snapshots) {
    for (const row of snapshot) h.insert(row);
    await h.watcher.sweep();
  }
  assert.equal(h.runtime.begun.length, 2);
  assert.deepEqual(h.runtime.completed.map((value) => value.turnID), ["turn-1", "turn-2", "turn-2", "turn-2", "turn-2"]);
  assert.deepEqual(h.runtime.completed.map((value) => value.refreshCompleted), [false, false, true, true, true]);
  const final = h.runtime.completed.at(-1);
  assert.equal(final.response, fixture.snapshots.at(-1)[1].text);
  assert.deepEqual(final.activities.filter((value) => value.kind === "message").map((value) => value.text), [
    fixture.snapshots[1][2].text, fixture.snapshots[2][2].text, fixture.snapshots[3][2].text,
  ]);
  assert.equal(h.watcher.pendingIndexes.size, 0);
  await h.watcher.sweep();
  assert.equal(h.runtime.completed.length, 5, "변경 없는 재조회는 중복 저장하지 않는다");
  h.insert({ idx: 4344, kind: "user", text: "새 질문" });
  h.insert({ idx: 4345, kind: "assistant", text: "새 답변" });
  await h.watcher.sweep();
  assert.equal(h.runtime.completed.at(-1).turnID, "turn-3");
  assert.deepEqual(h.runtime.completed.at(-1).activities, []);
});

test("Antigravity 늦게 완료된 이전 단계와 모드 진입 전 고아 응답을 새 질문에 붙이지 않는다", async (t) => {
  const h = await antigravityWatcherHarness(t);
  h.insert({ idx: 1, kind: "assistant", text: "모드 진입 전 응답" });
  h.insert({ idx: 2, kind: "user", text: "응답 전 중단할 질문" });
  h.insert({ idx: 3, kind: "assistant", text: "늦은 이전 답변", status: 1 });
  await h.watcher.sweep();
  h.insert({ idx: 4, kind: "user", text: "지금 질문" });
  h.insert({ idx: 5, kind: "assistant", text: "지금 답변" });
  await h.watcher.sweep();
  h.database.prepare("UPDATE steps SET status = 3 WHERE idx = 3").run();
  await h.watcher.sweep();
  assert.deepEqual(h.runtime.interrupted.map((value) => value.turnID), ["turn-1"]);
  assert.deepEqual(h.runtime.completed.map((value) => value.response), ["지금 답변"]);
  assert.deepEqual(h.runtime.completed[0].activities, []);
  assert.equal(h.watcher.pendingIndexes.size, 0);
});

test("Antigravity 후속 응답은 전체 단계 토큰·근거를 보존하며 재시도 때 중복 합산하지 않는다", async (t) => {
  const h = await antigravityWatcherHarness(t);
  const metadata = (input, output) => message(
    bytesField(12, Buffer.from("execution")),
    bytesField(1, message(numberField(1, 1_788_618_000))),
    bytesField(9, message(numberField(2, input), numberField(3, output))),
  );
  h.insert({ idx: 1, kind: "user", text: "질문" });
  await h.watcher.sweep();
  submitStructuredResponseSource(h.state.structuredResultPath, { kind: "file", title: "첫 근거", locator: "first.mjs" });
  h.insert({ idx: 2, kind: "assistant", text: "", metadata: metadata(100, 10) });
  h.insert({ idx: 3, kind: "assistant", text: "작업 중", metadata: metadata(50, 5) });
  await h.watcher.sweep();
  assert.equal(h.runtime.completed[0].usage.inputTokens, 150);
  submitStructuredResponseSource(h.state.structuredResultPath, { kind: "file", title: "후속 근거", locator: "last.mjs" });
  h.insert({ idx: 4, kind: "assistant", text: "최종 답변", metadata: metadata(25, 3) });
  const complete = h.runtime.completeTerminalTurn.bind(h.runtime);
  let fail = true;
  h.runtime.completeTerminalTurn = async (entry) => {
    if (fail) { fail = false; throw new Error("일시적 저장 실패"); }
    return complete(entry);
  };
  await assert.rejects(h.watcher.sweep(), /일시적 저장 실패/);
  await h.watcher.sweep();
  await h.watcher.sweep();
  assert.equal(h.runtime.completed.length, 2);
  assert.equal(h.runtime.completed[1].usage.inputTokens, 175);
  assert.equal(h.runtime.completed[1].usage.outputTokens, 18);
  assert.deepEqual(h.runtime.completed[1].structured.sources.map((value) => value.locator), ["first.mjs", "last.mjs"]);
  assert.equal(h.runtime.completed[1].response, "최종 답변");
});

test("Antigravity 명시적 중단 뒤 늦은 응답은 중단 턴을 되살리지 않는다", async (t) => {
  const h = await antigravityWatcherHarness(t);
  h.insert({ idx: 1, kind: "user", text: "중단할 질문" });
  await h.watcher.sweep();
  await h.manager.interrupt("boss");
  h.insert({ idx: 2, kind: "assistant", text: "중단 뒤 늦은 응답" });
  await h.watcher.sweep();
  assert.equal(h.runtime.completed.length, 0);
  h.insert({ idx: 3, kind: "user", text: "다음 질문" });
  h.insert({ idx: 4, kind: "assistant", text: "다음 답변" });
  await h.watcher.sweep();
  assert.deepEqual(h.runtime.completed.map((entry) => entry.response), ["다음 답변"]);
  assert.deepEqual(h.runtime.completed[0].activities, []);
});

async function antigravityWatcherHarness(t) {
  const root = await mkdtemp(join(tmpdir(), "officestra-antigravity-followups-"));
  const externalSessionID = "01a0c0de-3333-7000-8000-000000000003";
  await mkdir(join(root, "conversations"));
  const database = new DatabaseSync(join(root, "conversations", `${externalSessionID}.db`));
  database.exec("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, metadata BLOB, step_payload BLOB)");
  const runtime = fakeRuntime({ backend: "antigravity", externalSessionID, permission: "dangerously-skip-permissions" });
  const manager = new TerminalSessionManager({ runtime, broadcast() {}, antigravityRoot: root, antigravityPollIntervalMs: 60_000 });
  await manager.open("boss");
  const state = manager.sessions.get("boss");
  await state.watcher.sweepPromise;
  clearInterval(state.watcher.pollTimer);
  for (const watcher of state.watcher.watchers) watcher.close();
  state.watcher.watchers = [];
  t.after(async () => { await manager.close("boss"); database.close(); });
  return { runtime, manager, state, watcher: state.watcher, database, insert(row) {
    const type = { user: 14, assistant: 15, tool: 132, notification: 101 }[row.kind];
    const text = row.kind === "user"
      ? bytesField(19, message(bytesField(2, Buffer.from(row.text ?? ""))))
      : row.kind === "assistant" ? bytesField(20, message(bytesField(1, Buffer.from(row.text ?? "")))) : Buffer.alloc(0);
    database.prepare("INSERT INTO steps VALUES (?, ?, ?, ?, ?)").run(
      row.idx, type, row.status ?? 3, row.metadata ?? null, message(numberField(1, type), text),
    );
  } };
}

test("turn origin migration은 gui 기본값과 terminal 제약을 둔다", () => {
  const sql = readFileSync(new URL("../../database/migrations/034_turn_origin.sql", import.meta.url), "utf8");
  assert.match(sql, /origin text NOT NULL DEFAULT 'gui'/);
  assert.match(sql, /origin IN \('gui', 'terminal'\)/);
  assert.match(sql, /turns_origin_check/);
});

const codexThreadID = "01a0c0de-1111-7000-8000-000000000001";
const codexTurnID = "01a0c0de-2222-7000-8000-000000000002";

function rolloutLine(record) {
  return JSON.stringify(record) + "\n";
}

async function codexFixture() {
  const root = await mkdtemp(join(tmpdir(), "officestra-codex-terminal-"));
  const sessionsRoot = join(root, "sessions");
  const workdir = join(root, "workdir");
  const day = join(sessionsRoot, "2026", "09", "03");
  await mkdir(day, { recursive: true });
  await mkdir(workdir, { recursive: true });
  const rollout = join(
    day,
    `rollout-2026-09-03T00-00-00-${codexThreadID}.jsonl`,
  );
  // 열기 전에 이미 끝난 지난 턴. 워처가 되풀이하면 안 된다.
  await writeFile(rollout, [
    { type: "session_meta", payload: { id: codexThreadID, source: "cli", cwd: workdir } },
    { type: "event_msg", payload: { type: "task_started", turn_id: "old-turn", started_at: 1 } },
    { type: "event_msg", payload: { type: "item_completed", turn_id: "old-turn", item: { type: "UserMessage", content: [{ type: "text", text: "지난 질문" }] } } },
    { type: "event_msg", payload: { type: "task_complete", turn_id: "old-turn", last_agent_message: "지난 답변", started_at: 1, completed_at: 2 } },
  ].map(rolloutLine).join(""));
  return { sessionsRoot, workdir, rollout };
}

function codexNotify(workdir, message) {
  return {
    source: "codex",
    payload: {
      type: "agent-turn-complete",
      "thread-id": codexThreadID,
      "turn-id": codexTurnID,
      cwd: workdir,
      "last-assistant-message": message,
    },
  };
}

// codex notify는 완료 때만 오므로, rollout에서 시작을 읽어 running을 먼저
// 보여야 하단 바의 "일하는중" 표시가 켜진다.
test("Codex 터미널은 rollout의 시작과 질문을 보면 running 턴을 먼저 만들고 notify가 그 턴을 완료한다", async () => {
  const { sessionsRoot, workdir, rollout } = await codexFixture();
  const events = [];
  const runtime = fakeRuntime({
    backend: "codex",
    externalSessionID: codexThreadID,
    workdir,
    executablePath: "/usr/bin/true",
  });
  const manager = new TerminalSessionManager({
    runtime,
    broadcast: (event) => events.push(event),
    codexSessionsRoot: sessionsRoot,
  });
  await manager.open("boss");
  const state = manager.sessions.get("boss");
  // 열기 전 기록은 되풀이하지 않는다.
  await state.watcher.sweep();
  assert.equal(runtime.begun.length, 0);

  await appendFile(rollout, [
    { type: "event_msg", payload: { type: "task_started", turn_id: codexTurnID, started_at: 1_788_347_510 } },
    { type: "event_msg", payload: { type: "item_completed", turn_id: codexTurnID, item: { type: "UserMessage", content: [{ type: "text", text: "터미널 질문" }] } } },
  ].map(rolloutLine).join(""));
  events.length = 0;
  await state.watcher.sweep();
  assert.equal(runtime.begun.length, 1);
  assert.equal(runtime.begun[0].prompt, "터미널 질문");
  assert.equal(state.runningTurnID, "turn-1");
  assert.ok(events.some((event) => event.type === "terminal.changed"));
  // 같은 줄을 다시 읽어도 두 번 만들지 않는다.
  await state.watcher.sweep();
  assert.equal(runtime.begun.length, 1);

  await appendFile(rollout, [
    { type: "turn_context", payload: { turn_id: codexTurnID, cwd: workdir } },
    { type: "response_item", payload: { type: "message", role: "assistant", phase: "commentary", content: [{ type: "output_text", text: "작업 진행" }] } },
    { type: "response_item", payload: { type: "reasoning", summary: [{ type: "summary_text", text: "공개 요약" }] } },
    { type: "event_msg", payload: { type: "item_completed", turn_id: codexTurnID, item: { type: "AgentMessage", phase: "final_answer", content: [{ type: "Text", text: "터미널 답변" }] } } },
    { type: "event_msg", payload: { type: "task_complete", turn_id: codexTurnID, last_agent_message: "터미널 답변", started_at: 1_788_347_510, completed_at: 1_788_347_513 } },
  ].map(rolloutLine).join(""));
  submitStructuredResponseSource(state.structuredResultPath, { kind: "file", title: "근거", locator: "codex.mjs" });
  const completed = await manager.handleEvent(
    "boss",
    codexNotify(workdir, "터미널 답변"),
  );
  assert.equal(completed.accepted, true);
  assert.equal(completed.turnId, "turn-1");
  assert.equal(runtime.begun.length, 1);
  assert.deepEqual(
    runtime.completed.map((entry) => [entry.turnID, entry.response]),
    [["turn-1", "터미널 답변"]],
  );
  assert.equal(state.runningTurnID, null);
  assert.deepEqual(runtime.completed[0].activities.map((a) => a.kind), ["message", "thinking"]);
  assert.equal(runtime.completed[0].structured.sources[0].locator, "codex.mjs");
  assert.equal((await manager.handleEvent("boss", codexNotify(workdir, "터미널 답변"))).reason, "duplicate");
  assert.equal(runtime.completed.length, 1);
  await manager.close("boss");
});

test("Codex notify가 워처보다 먼저 오면 기존 경로로 기록하고 늦은 sweep이 같은 턴을 다시 만들지 않는다", async () => {
  const { sessionsRoot, workdir, rollout } = await codexFixture();
  const runtime = fakeRuntime({
    backend: "codex",
    externalSessionID: codexThreadID,
    workdir,
    executablePath: "/usr/bin/true",
  });
  const manager = new TerminalSessionManager({
    runtime,
    broadcast() {},
    codexSessionsRoot: sessionsRoot,
  });
  await manager.open("boss");
  const state = manager.sessions.get("boss");
  await state.watcher.sweep();

  await appendFile(rollout, [
    { type: "event_msg", payload: { type: "task_started", turn_id: codexTurnID, started_at: 1_788_347_510 } },
    { type: "turn_context", payload: { turn_id: codexTurnID, cwd: workdir } },
    { type: "event_msg", payload: { type: "item_completed", turn_id: codexTurnID, item: { type: "UserMessage", content: [{ type: "text", text: "빠른 질문" }] } } },
    { type: "event_msg", payload: { type: "task_complete", turn_id: codexTurnID, last_agent_message: "빠른 답변", started_at: 1_788_347_510, completed_at: 1_788_347_511 } },
  ].map(rolloutLine).join(""));
  const completed = await manager.handleEvent(
    "boss",
    codexNotify(workdir, "빠른 답변"),
  );
  assert.equal(completed.accepted, true);
  assert.equal(runtime.begun.length, 1);
  assert.equal(runtime.completed.length, 1);
  assert.equal(state.runningTurnID, null);

  await state.watcher.sweep();
  assert.equal(runtime.begun.length, 1);
  await manager.close("boss");
});

test("Codex turn_aborted를 보면 rollout 워처가 running 턴을 중단한다", async () => {
  const { sessionsRoot, workdir, rollout } = await codexFixture();
  const events = [];
  const runtime = fakeRuntime({
    backend: "codex",
    externalSessionID: codexThreadID,
    workdir,
    executablePath: "/usr/bin/true",
  });
  const manager = new TerminalSessionManager({
    runtime,
    broadcast: (event) => events.push(event),
    codexSessionsRoot: sessionsRoot,
  });
  await manager.open("boss");
  const state = manager.sessions.get("boss");
  await state.watcher.sweep();

  await appendFile(rollout, [
    { type: "event_msg", payload: { type: "task_started", turn_id: codexTurnID, started_at: 1_788_347_510 } },
    { type: "event_msg", payload: { type: "item_completed", turn_id: codexTurnID, item: { type: "UserMessage", content: [{ type: "text", text: "중단할 질문" }] } } },
  ].map(rolloutLine).join(""));
  await state.watcher.sweep();
  assert.equal(state.runningTurnID, "turn-1");

  events.length = 0;
  await appendFile(rollout, rolloutLine({
    type: "event_msg",
    payload: {
      type: "turn_aborted",
      turn_id: codexTurnID,
      started_at: 1_788_347_510,
      completed_at: 1_788_347_511,
    },
  }));
  await state.watcher.sweep();

  assert.deepEqual(runtime.interrupted, [{
    characterID: "boss",
    turnID: "turn-1",
  }]);
  assert.equal(state.runningTurnID, null);
  assert.equal(state.codexTurnID, null);
  assert.ok(events.some((event) => event.type === "terminal.changed"));
  await manager.close("boss");
});

function fakeRuntime({
  backend = "claude",
  externalSessionID = null,
  workdir = "/tmp/office",
  executablePath = null,
  permission = baseCharacter.permission,
} = {}) {
  return {
    pool: { query: async () => ({ rows: [] }) },
    begun: [],
    completed: [],
    interrupted: [],
    bound: [],
    registry: null,
    async prepareTerminalLaunch(characterID) {
      return {
        character: {
          ...baseCharacter,
          id: characterID,
          backend,
          permission,
          ...(executablePath ? { executablePath } : {}),
        },
        sessionID: "db-session",
        conversationID: "conversation",
        externalSessionID,
        workdir,
      };
    },
    async bindTerminalExternalSession(entry) { this.bound.push(entry); },
    async beginTerminalTurn(entry) {
      const turn = { ...entry, turnID: `turn-${this.begun.length + 1}` };
      this.begun.push(turn);
      return turn;
    },
    async completeTerminalTurn(entry) { this.completed.push(entry); },
    async interruptTerminalTurn(characterID, turnID) {
      this.interrupted.push({ characterID, turnID });
      return true;
    },
  };
}

async function waitUntil(predicate, timeoutMs = 1_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await delay(10);
  }
  assert.fail("조건이 제한 시간 안에 충족되지 않았습니다.");
}

async function dispatchFixture(t, timeout = 1_000) {
  const runtime = fakeRuntime();
  const events = [];
  const manager = new TerminalSessionManager({ runtime, broadcast: e => events.push(e), dispatchTimeoutMs: timeout });
  const spec = await manager.open('boss');
  t.after(() => manager.close('boss'));
  const state = manager.sessions.get('boss');
  const control = (action, extra = {}) => manager.dispatchControl('boss', {
    action, dispatchId: state.dispatch?.id, terminalSessionId: spec.terminalSessionId,
    ownerToken: spec.ownerToken, ...extra,
  });
  const submit = prompt => manager.handleEvent('boss', { source: 'claude', payload: { hook_event_name: 'UserPromptSubmit', prompt } });
  return { manager, runtime, state, spec, control, submit, events };
}

test('유휴 터미널 API는 같은 CLI 훅 접수 후에만 202 turnId를 반환한다', async t => {
  const f = await dispatchFixture(t);
  const runtime = new AgentRuntime({ pool: {}, withTransaction: async () => {}, workdir: '/tmp/office', broadcast() {} });
  runtime.setTerminalSessionRegistry(f.manager);
  const result = runtime.start({ characterID: 'boss', prompt: '답변해줘' });
  let settled = false; result.then(() => { settled = true; });
  await waitUntil(() => f.events.some(e => e.type === 'terminal.dispatch'));
  assert.equal(f.runtime.begun.length, 0);
  const { prompt } = f.control('claim');
  await delay(5); assert.equal(settled, false);
  const started = await f.submit(prompt);
  assert.deepEqual(await result, { turnId: started.turnId, conversationId: 'conversation', status: 'running' });
  assert.equal(f.runtime.begun[0].prompt, '답변해줘');
  assert.equal(f.manager.sessions.size, 1);
  await f.manager.handleEvent('boss', { source: 'claude', payload: { hook_event_name: 'Stop', last_assistant_message: '회신' } });
  assert.equal(f.runtime.completed[0].turnID, started.turnId);
});

test('터미널 dispatch 중복·DB running·압축은 즉시409이며 예약하지 않는다', async t => {
  const f = await dispatchFixture(t);
  f.runtime.pool.query = async () => ({ rows: [{ id: 'running' }] });
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: 'a' }), AgentBusyError);
  assert.equal(f.state.dispatch, null);
  f.runtime.pool.query = async () => ({ rows: [] });
  f.runtime.compactingCharacters = new Set(['boss']);
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: 'a' }), AgentBusyError);
  f.runtime.compactingCharacters.clear();
  const result = f.manager.dispatch({ characterID: 'boss', prompt: 'a' });
  const failure = assert.rejects(result, AgentBusyError);
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: 'b' }), AgentBusyError);
  await f.manager.close('boss'); await failure;
  assert.equal(f.runtime.begun.length, 0);
});

test('터미널 소유권·epoch·중복claim·사용자입력 혼입은 전달 확정하지 않는다', async t => {
  const f = await dispatchFixture(t);
  const result = f.manager.dispatch({ characterID: 'boss', prompt: 'API 요청' });
  const failure = assert.rejects(result, AgentBusyError);
  await waitUntil(() => f.events.some(e => e.type === 'terminal.dispatch'));
  assert.throws(() => f.control('claim', { ownerToken: 'wrong' }), AgentBusyError);
  assert.throws(() => f.control('claim', { terminalSessionId: 'old-session' }), AgentBusyError);
  const { prompt } = f.control('claim');
  assert.throws(() => f.control('claim'), AgentBusyError);
  await f.submit(prompt + ' altered');
  assert.ok(f.state.dispatch);
  assert.equal(f.runtime.begun[0].prompt, prompt + ' altered');
  await f.manager.close('boss'); await failure;
});

test('앱이 입력충돌을 거부하면 턴 생성 없이409로 종료한다', async t => {
  const f = await dispatchFixture(t);
  const result = f.manager.dispatch({ characterID: 'boss', prompt: 'a' });
  const failure = assert.rejects(result, AgentBusyError);
  await waitUntil(() => f.events.some(e => e.type === 'terminal.dispatch'));
  f.control('claim'); f.control('reject'); await failure;
  assert.equal(f.state.dispatch, null); assert.equal(f.runtime.begun.length, 0);
});

test('잘못된 control 본문은409이고 늦은 미전송 확인은 잠금을 해제한다', async t => {
  const f = await dispatchFixture(t, 35);
  for (const body of [null, undefined, [], 'claim']) {
    assert.throws(() => f.manager.dispatchControl('boss', body), AgentBusyError);
  }
  const result = f.manager.dispatch({ characterID: 'boss', prompt: '전송 안 됨' });
  const failure = assert.rejects(result, AgentBusyError);
  await waitUntil(() => f.events.some(e => e.type === 'terminal.dispatch'));
  f.control('claim');
  await failure;
  assert.ok(f.state.dispatch.expired);
  assert.throws(() => f.control('claim'), AgentBusyError);
  f.control('reject');
  assert.equal(f.state.dispatch, null);
  assert.equal(f.runtime.begun.length, 0);
});

test('미수신 timeout은 해제, claim 이후 불확실 timeout은 재전송 차단 후 늦은 훅 수용', async t => {
  const f = await dispatchFixture(t, 35);
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: '미수신' }), AgentBusyError);
  assert.equal(f.state.dispatch, null);
  const result = f.manager.dispatch({ characterID: 'boss', prompt: '늦은 접수' });
  const failure = assert.rejects(result, AgentBusyError);
  const { prompt } = f.control('claim');
  await failure;
  assert.ok(f.state.dispatch.expired);
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: '재시도' }), AgentBusyError);
  await f.submit(prompt);
  assert.equal(f.state.dispatch, null);
  assert.equal(f.runtime.begun[0].prompt, '늦은 접수');
});

test('종료·명시적중단은 불확실 전달을 정리하고 제어문자·다른대화는 거부한다', async t => {
  const f = await dispatchFixture(t);
  for (const prompt of ['', 'a\u001bb', 'a\rb', 'a\0b']) {
    await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt }));
  }
  await assert.rejects(f.manager.dispatch({ characterID: 'boss', prompt: 'x', conversationID: 'other' }), AgentBusyError);
  const result = f.manager.dispatch({ characterID: 'boss', prompt: 'x', attachmentPaths: ['/tmp/example.png'] });
  const failure = assert.rejects(result, AgentBusyError);
  const { prompt } = f.control('claim'); assert.ok(prompt.endsWith('/tmp/example.png'));
  await f.manager.interrupt('boss'); await failure;
  assert.equal(f.state.dispatch, null);
});

test('공통 턴 시작 어댑터는 세 backend의 실제 본문과 turnId를 보존한다', async t => {
  for (const backend of ['claude', 'codex', 'antigravity']) {
    const f = await dispatchFixture(t);
    const result = f.manager.dispatch({ characterID: 'boss', prompt: '같은 대화' });
    const { prompt } = f.control('claim');
    const turn = await f.state.beginTurn({ characterID: 'boss', sessionID: 'db-session', prompt, execution: { backend } });
    assert.equal((await result).turnId, turn.turnID);
    assert.equal(f.runtime.begun[0].execution.backend, backend);
    assert.equal(f.runtime.begun[0].prompt, '같은 대화');
  }
});

test('Antigravity SQLite 실제 감시 경로가 API 표식 턴을 연결하고 한 번만 완료한다', async t => {
  const h = await antigravityWatcherHarness(t);
  const result = h.manager.dispatch({ characterID: 'boss', prompt: '안티 회신' });
  const { prompt } = h.manager.dispatchControl('boss', { action: 'claim', dispatchId: h.state.dispatch.id, terminalSessionId: h.state.terminalSessionID, ownerToken: h.state.ownerToken });
  h.insert({ idx: 1, kind: 'user', text: prompt });
  await h.watcher.sweep();
  assert.equal((await result).turnId, h.state.runningTurnID);
  assert.equal(h.runtime.begun[0].prompt, '안티 회신');
  h.insert({ idx: 2, kind: 'assistant', text: '확인했습니다' });
  await h.watcher.sweep(); await h.watcher.sweep();
  assert.equal(h.runtime.begun.length, 1);
  assert.equal(h.runtime.completed.length, 1);
});

test('Codex rollout 감시와 notify는 API 요청을 같은 턴으로 접수·완료한다', async t => {
  const { sessionsRoot, workdir, rollout } = await codexFixture();
  const runtime = fakeRuntime({ backend: 'codex', externalSessionID: codexThreadID, workdir, executablePath: '/usr/bin/true' });
  const manager = new TerminalSessionManager({ runtime, broadcast() {}, codexSessionsRoot: sessionsRoot });
  const spec = await manager.open('boss'); t.after(() => manager.close('boss'));
  const state = manager.sessions.get('boss'); await state.watcher.sweep();
  const result = manager.dispatch({ characterID: 'boss', prompt: '코덱스 회신' });
  const { prompt } = manager.dispatchControl('boss', { action: 'claim', dispatchId: state.dispatch.id, terminalSessionId: spec.terminalSessionId, ownerToken: spec.ownerToken });
  await appendFile(rollout, [
    { type: 'turn_context', payload: { turn_id: codexTurnID, cwd: workdir } },
    { type: 'event_msg', payload: { type: 'task_started', turn_id: codexTurnID, started_at: 1_788_347_510 } },
    { type: 'event_msg', payload: { type: 'item_completed', turn_id: codexTurnID, item: { type: 'UserMessage', content: [{ type: 'text', text: prompt }] } } },
  ].map(rolloutLine).join(''));
  await state.watcher.sweep();
  assert.equal((await result).turnId, state.runningTurnID);
  assert.equal(runtime.begun[0].prompt, '코덱스 회신');
  await appendFile(rollout, rolloutLine({ type: 'event_msg', payload: { type: 'task_complete', turn_id: codexTurnID, last_agent_message: '회신 완료', started_at: 1_788_347_510, completed_at: 1_788_347_513 } }));
  await manager.handleEvent('boss', codexNotify(workdir, '회신 완료'));
  assert.equal(runtime.begun.length, 1); assert.equal(runtime.completed.length, 1);
});

function encodeVarint(input) {
  let value = BigInt(input);
  const bytes = [];
  do {
    let byte = Number(value & 0x7fn);
    value >>= 7n;
    if (value > 0n) byte |= 0x80;
    bytes.push(byte);
  } while (value > 0n);
  return Buffer.from(bytes);
}

function numberField(number, value) {
  return Buffer.concat([encodeVarint(number << 3), encodeVarint(value)]);
}

function bytesField(number, value) {
  const bytes = Buffer.from(value);
  return Buffer.concat([encodeVarint((number << 3) | 2), encodeVarint(bytes.length), bytes]);
}

function message(...fields) { return Buffer.concat(fields); }
