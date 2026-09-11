// 이 파일은 파일 첨부 인수와 실행 중단 상태 저장을 검증한다.

import assert from "node:assert/strict";
import {
  appendFileSync,
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { Readable } from "node:stream";
import test from "node:test";

import {
  AgentBusyError,
  AgentDrainingError,
  AgentJobNotFoundError,
  AgentRuntime,
  accumulatedUsage,
  buildArguments,
  claudePersistentArguments,
  claudePersistentWorkerSignature,
  claudeSessionPath,
  claudeSessionResumable,
  codexUsageDelta,
  configuredExecutableForCharacter,
  executionEnvironment,
  findClaudeSessionPath,
  latestClaudeUsageFromSession,
  latestCodexUsageFromRollout,
  normalizeAutoCompactPercent,
  persistTurnWikiProposals,
  prepareClaudeSessionResume,
  recoverInterruptedUsage,
  promptWithAttachments,
  stageAttachments,
} from "../src/agent-runtime.mjs";
import { decodeAgentResponse } from "../src/agent-event-parser.mjs";
import {
  STRUCTURED_RESULT_ENV,
  identityPromptWithStructuredResult,
  structuredResultToolDirectory,
  structuredTurnResultPath,
} from "../src/structured-turn-result.mjs";


const codexCharacter = {
  backend: "codex",
  model: "gpt-5.6-sol",
  effort: "high",
  fastMode: true,
  permission: "workspace-write",
  name: "코과장",
  seat: "우측 아래",
  identityPrompt: "업무를 정확히 처리한다.",
};

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

test("자동 압축 기준은 20~95% 범위와 90% 기본값을 사용한다", () => {
  assert.equal(normalizeAutoCompactPercent(undefined), 90);
  assert.equal(normalizeAutoCompactPercent(42), 42);
  assert.equal(normalizeAutoCompactPercent(12), 20);
  assert.equal(normalizeAutoCompactPercent(93.6), 94);
  assert.equal(normalizeAutoCompactPercent(100), 95);
});

test("Claude 턴 종료 점유가 직원 기준에 도달하면 같은 세션을 자동 압축한다", async () => {
  const calls = [];
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    contextUsageReader: () => ({
      usedTokens: 900_000,
      limitTokens: 1_000_000,
    }),
  });
  runtime.compactContext = async (characterID, options) => {
    calls.push({ characterID, options });
    return { ok: true };
  };

  await runtime.maybeAutoCompactAfterTurn({
    externalSessionID: "session-1",
    character: {
      id: "left-man",
      name: "클대리",
      backend: "claude",
      model: "claude-sonnet-5",
      autoCompactPercent: 90,
    },
  });

  assert.deepEqual(calls, [{
    characterID: "left-man",
    options: {
      automatic: true,
      expectedSessionID: "session-1",
    },
  }]);
});

test("Claude 턴 종료 점유가 직원 기준 미만이면 자동 압축하지 않는다", async () => {
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    contextUsageReader: () => ({
      usedTokens: 899_999,
      limitTokens: 1_000_000,
    }),
  });
  let called = false;
  runtime.compactContext = async () => {
    called = true;
  };

  await runtime.maybeAutoCompactAfterTurn({
    externalSessionID: "session-1",
    character: {
      id: "left-man",
      name: "클대리",
      backend: "claude",
      model: "claude-sonnet-5",
      autoCompactPercent: 90,
    },
  });

  assert.equal(called, false);
});

test("Codex 턴 종료는 CLI 기본 자동 압축에 맡긴다", async () => {
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    contextUsageReader: () => ({
      usedTokens: 999_999,
      limitTokens: 1_000_000,
    }),
  });
  let called = false;
  runtime.compactContext = async () => {
    called = true;
  };

  await runtime.maybeAutoCompactAfterTurn({
    externalSessionID: "session-1",
    character: {
      id: "boss",
      name: "백부장",
      backend: "codex",
      model: "gpt-5.6-sol",
      autoCompactPercent: 50,
    },
  });

  assert.equal(called, false);
});

test("Codex 수동 압축은 활성 세션을 app-server compactor에 전달한다", async () => {
  const calls = [];
  const usage = [
    { usedTokens: 230_000, limitTokens: 258_400 },
    { usedTokens: 31_000, limitTokens: 258_400 },
  ];
  const runtime = new AgentRuntime({
    pool: {
      query: async () => ({
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "백부장",
          seat: "상단",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "ultra",
          fastMode: true,
          autoCompactPercent: 90,
          permission: "danger-full-access",
          identityPrompt: "업무 지침",
          config: {},
          externalSessionID: "thread-1",
          conversationWorkdir: "/tmp",
          sessionRepositoryRoot: "/tmp",
          resumeExecutionWorkdir: "/tmp",
        }],
      }),
    },
    withTransaction: async (operation) => await operation({}),
    workdir: "/tmp",
    repositoryRoot: "/tmp",
    broadcast: (event) => calls.push({ event }),
    contextUsageReader: () => usage.shift(),
    codexCompactor: async (options) => {
      calls.push({ compact: options });
      return { turnID: "turn-compact" };
    },
  });

  const result = await runtime.compactContext("boss");

  assert.deepEqual(calls[0].event, {
    type: "context.compaction.started",
    characterId: "boss",
    automatic: false,
  });
  assert.equal(calls[1].compact.threadID, "thread-1");
  assert.equal(calls[1].compact.cwd, "/tmp");
  assert.equal(Object.hasOwn(calls[1].compact, "contextWindow"), false);
  assert.equal(
    Object.hasOwn(calls[1].compact, "autoCompactTokenLimit"),
    false,
  );
  assert.deepEqual(result, {
    ok: true,
    automatic: false,
    backend: "codex",
    sessionId: "thread-1",
    preTokens: 230_000,
    postTokens: 31_000,
    limitTokens: 258_400,
  });
  assert.deepEqual(calls[2].event, {
    type: "context.compacted",
    characterId: "boss",
    automatic: false,
    preTokens: 230_000,
    postTokens: 31_000,
    limitTokens: 258_400,
  });
  assert.deepEqual(runtime.compactingCharacterIDs(), []);
  assert.equal(runtime.compactingCharacters.size, 0);
});

test("압축 실패도 시작과 종료 이벤트를 보내고 점유를 해제한다", async () => {
  const events = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async () => ({
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "백부장",
          seat: "상단",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "max",
          fastMode: false,
          autoCompactPercent: 90,
          permission: "danger-full-access",
          identityPrompt: "업무 지침",
          config: {},
          externalSessionID: "thread-1",
          conversationWorkdir: "/tmp",
          sessionRepositoryRoot: "/tmp",
          resumeExecutionWorkdir: "/tmp",
        }],
      }),
    },
    withTransaction: async (operation) => await operation({}),
    workdir: "/tmp",
    repositoryRoot: "/tmp",
    broadcast: (event) => events.push(event),
    contextUsageReader: () => ({
      usedTokens: 230_000,
      limitTokens: 258_400,
    }),
    codexCompactor: async () => {
      throw new Error("압축기 실패");
    },
  });

  await assert.rejects(
    runtime.compactContext("boss", { automatic: true }),
    /압축기 실패/,
  );

  assert.deepEqual(events, [
    {
      type: "context.compaction.started",
      characterId: "boss",
      automatic: true,
    },
    {
      type: "context.compaction.failed",
      characterId: "boss",
      automatic: true,
      errorMessage: "압축기 실패",
    },
  ]);
  assert.deepEqual(runtime.compactingCharacterIDs(), []);
});

function wikiProposalTestClient(queries) {
  const projectID = "11111111-1111-4111-8111-111111111111";
  const sourceID = "22222222-2222-4222-8222-222222222222";
  return {
    sourceID,
    client: {
      query: async (text, values = []) => {
        queries.push({ text, values });
        if (/SELECT id FROM projects WHERE repository_root/.test(text)) {
          return { rowCount: 1, rows: [{ id: projectID }] };
        }
        if (/FROM work_records AS record\s+WHERE record.id = ANY/.test(text)) {
          return {
            rowCount: 1,
            rows: [{
              id: sourceID,
              project_id: projectID,
              record_type: "result",
              searchable: true,
            }],
          };
        }
        if (/record.metadata->>'pageKey' = \$2/.test(text)) {
          return { rowCount: 0, rows: [] };
        }
        if (/INSERT INTO wiki_proposals/.test(text)) {
          return {
            rowCount: 1,
            rows: [{
              id: "33333333-3333-4333-8333-333333333333",
              project_id: projectID,
              page_key: values[1],
              target_record_id: null,
              base_version: values[3],
              state: values[4],
              approval_grade: values[5],
              proposal_kind: values[6],
              ordinal: values[7],
              draft_title: values[8],
              draft_body: values[9],
              source_work_record_ids: JSON.parse(values[10]),
              author_character_id: values[11],
              author_turn_id: values[12],
            }],
          };
        }
        return { rowCount: 0, rows: [] };
      },
    },
  };
}

test("완료 응답의 user 위키 수정안은 같은 트랜잭션의 승인 대기로 저장한다", async () => {
  const queries = [];
  const { client, sourceID } = wikiProposalTestClient(queries);
  const warning = await persistTurnWikiProposals(client, {
    repositoryRoot: "/repo",
    workRecordID: sourceID,
    turnID: "44444444-4444-4444-8444-444444444444",
    characterID: "boss",
    proposals: [{
      pageKey: "durable-preferences",
      kind: "constraint",
      title: "지속 선호",
      body: "자동 과거 대화 주입을 사용하지 않는다.",
      approvalTier: "user",
    }],
  });

  assert.equal(warning, null);
  const inserted = queries.find(({ text }) =>
    /INSERT INTO wiki_proposals/.test(text)
  );
  assert.equal(inserted.values[4], "pending_user");
  assert.equal(inserted.values[5], "user");
  assert.equal(inserted.values[6], "constraint");
  assert.deepEqual(JSON.parse(inserted.values[10]), [sourceID]);
  assert.equal(
    queries.filter(({ text }) => /SAVEPOINT wiki_proposal_0/.test(text)).length,
    2,
  );
});

test("질문 응답은 위키 수정안을 저장하지 않고 파서 경고만 보존한다", async () => {
  const queries = [];
  const warning = await persistTurnWikiProposals({
    query: async (...values) => queries.push(values),
  }, {
    repositoryRoot: "/repo",
    workRecordID: "22222222-2222-4222-8222-222222222222",
    turnID: "44444444-4444-4444-8444-444444444444",
    characterID: "boss",
    proposals: [{
      pageKey: "ignored-question",
      kind: "decision",
      title: "저장 금지",
      body: "질문 응답",
      approvalTier: "user",
    }],
    parserWarning: "위키 수정안 형식 경고",
    needsInput: true,
  });

  assert.equal(warning, "위키 수정안 형식 경고");
  assert.deepEqual(queries, []);
});

test("잘못된 수정안 하나는 savepoint로 격리하고 다음 수정안을 저장한다", async () => {
  const queries = [];
  const { client, sourceID } = wikiProposalTestClient(queries);
  const warning = await persistTurnWikiProposals(client, {
    repositoryRoot: "/repo",
    workRecordID: sourceID,
    turnID: "44444444-4444-4444-8444-444444444444",
    characterID: "boss",
    proposals: [{
      pageKey: "invalid-proposal",
      kind: "decision",
      title: "",
      body: "본문",
      approvalTier: "user",
    }, {
      pageKey: "valid-proposal",
      kind: "decision",
      title: "정상 제안",
      body: "본문",
      approvalTier: "user",
    }],
  });

  assert.match(warning, /1번을 저장하지 못했습니다/);
  assert.equal(
    queries.filter(({ text }) => /INSERT INTO wiki_proposals/.test(text)).length,
    1,
  );
  assert.equal(
    queries.some(({ text }) => /ROLLBACK TO SAVEPOINT wiki_proposal_0/.test(text)),
    true,
  );
});

test("drain은 준비 중인 업무를 계수하고 이후 일반 업무를 원자적으로 막는다", async () => {
  let releasePreparation;
  const preparationGate = new Promise((resolve) => {
    releasePreparation = resolve;
  });
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 0, rows: [] }) },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });
  runtime.startAccepted = async () => {
    await preparationGate;
    return { turnId: "turn-1", status: "running" };
  };

  const accepted = runtime.start({
    characterID: "boss",
    prompt: "이미 접수된 업무",
  });
  assert.deepEqual(runtime.maintenanceStatus(), {
    acceptingJobs: true,
    draining: false,
    activeTurnCount: 1,
    idle: false,
  });

  assert.deepEqual(runtime.beginDrain(), {
    acceptingJobs: false,
    draining: true,
    activeTurnCount: 1,
    idle: false,
  });
  await assert.rejects(
    runtime.start({ characterID: "right-man", prompt: "새 업무" }),
    (error) =>
      error instanceof AgentDrainingError &&
      /새 업무를 받지 않습니다/.test(error.message),
  );

  releasePreparation();
  await accepted;
  assert.deepEqual(runtime.maintenanceStatus(), {
    acceptingJobs: false,
    draining: true,
    activeTurnCount: 0,
    idle: true,
  });
  assert.deepEqual(runtime.cancelDrain(), {
    acceptingJobs: true,
    draining: false,
    activeTurnCount: 0,
    idle: true,
  });
});

test("drain은 이미 시작된 승인 후처리를 기다리고 새 수동 검토를 막는다", async () => {
  let releaseApproval;
  const approvalGate = new Promise((resolve) => {
    releaseApproval = resolve;
  });
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 0, rows: [] }) },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });
  runtime.approveWorkspaceAccepted = async () => {
    await approvalGate;
    return { workspace: { status: "merged" } };
  };

  const approval = runtime.approveWorkspace("turn-1", "review-tree");
  assert.equal(runtime.maintenanceStatus().activeTurnCount, 1);
  runtime.beginDrain();
  await assert.rejects(
    runtime.approveWorkspace("turn-2", "review-tree"),
    AgentDrainingError,
  );
  await assert.rejects(
    runtime.rejectWorkspace("turn-2"),
    AgentDrainingError,
  );
  assert.equal(runtime.maintenanceStatus().idle, false);

  releaseApproval();
  await approval;
  assert.equal(runtime.maintenanceStatus().idle, true);
});

test("서로 다른 직원 업무는 같은 공유 폴더에서 동시에 실행된다", async () => {
  const gates = {
    boss: deferred(),
    "right-man": deferred(),
  };
  const events = [];
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 1, rows: [] }) },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });
  runtime.prepareTurn = async ({
    characterID,
    prompt,
    conversationID,
    isolateGitWorkdir,
  }) => {
    assert.equal(isolateGitWorkdir, false);
    return {
      turnID: `turn-${characterID}`,
      sessionID: `session-${characterID}`,
      conversationID,
      externalSessionID: null,
      character: { id: characterID, backend: "codex" },
      prompt,
      workspace: null,
    };
  };
  runtime.beginPreparedTurn = async (turnID) => {
    events.push(`begin:${turnID}`);
  };
  runtime.execute = async (state) => {
    assert.equal(state.workdir, "/repo");
    assert.equal(state.workspace, null);
    events.push(`execute:${state.character.id}`);
    await gates[state.character.id].promise;
  };

  const first = await runtime.start({
    characterID: "boss",
    prompt: "첫 업무",
  });
  const second = await runtime.start({
    characterID: "right-man",
    prompt: "둘째 업무",
  });

  assert.equal(first.status, "running");
  assert.equal(second.status, "running");
  assert.deepEqual(events, [
    "begin:turn-boss",
    "execute:boss",
    "begin:turn-right-man",
    "execute:right-man",
  ]);
  assert.equal(runtime.running.size, 2);
  assert.equal(runtime.maintenanceStatus().activeTurnCount, 2);

  gates.boss.resolve();
  gates["right-man"].resolve();
});

test("후처리 계수는 작업 실패 뒤에도 반드시 해제된다", async () => {
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 0, rows: [] }) },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });

  await assert.rejects(
    runtime.withPostProcessing("failing-post-process", async () => {
      assert.equal(runtime.maintenanceStatus().activeTurnCount, 1);
      assert.equal(runtime.maintenanceStatus().idle, false);
      throw new Error("후처리 실패");
    }),
    /후처리 실패/,
  );

  assert.equal(runtime.maintenanceStatus().activeTurnCount, 0);
  assert.equal(runtime.maintenanceStatus().idle, true);
});

test("설정 실행 파일은 공급자 이름과 실행 가능한 일반 파일 조건을 모두 만족해야 한다", () => {
  const directory = mkdtempSync(join(tmpdir(), "officestra-cli-path-"));
  const codex = join(directory, "codex");
  const claude = join(directory, "claude");
  const agy = join(directory, "agy");
  const nonExecutableDirectory = join(directory, "non-executable");
  const nonExecutableCodex = join(nonExecutableDirectory, "codex");
  const directoryNamedCodex = join(directory, "directory", "codex");
  writeFileSync(codex, "");
  writeFileSync(claude, "");
  writeFileSync(agy, "");
  chmodSync(codex, 0o755);
  chmodSync(claude, 0o755);
  chmodSync(agy, 0o755);
  mkdirSync(nonExecutableDirectory, { recursive: true });
  writeFileSync(nonExecutableCodex, "");
  chmodSync(nonExecutableCodex, 0o644);
  mkdirSync(directoryNamedCodex, { recursive: true });
  try {
    assert.equal(
      configuredExecutableForCharacter({
        backend: "codex",
        config: { executablePath: codex },
      }),
      codex,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "codex",
        config: { executablePath: claude },
      }),
      null,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "antigravity",
        config: { executablePath: agy },
      }),
      agy,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "antigravity",
        config: { executablePath: codex },
      }),
      null,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "claude",
        config: { executablePath: join(directory, "missing-claude") },
      }),
      null,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "codex",
        config: { executablePath: nonExecutableCodex },
      }),
      null,
    );
    assert.equal(
      configuredExecutableForCharacter({
        backend: "codex",
        config: { executablePath: directoryNamedCodex },
      }),
      null,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

function makeCodexActivityState() {
  return {
    turnID: "turn-1",
    workdir: "/tmp",
    character: { id: "right-man", backend: "codex" },
    externalSessionID: "session-1",
    sequence: 0,
    lastActivity: null,
    activityRecords: new Map(),
    activityWritePromise: null,
    hasSeenInitialCodexReasoning: false,
    pendingInitialCodexReasoning: null,
    pendingAgentMessage: null,
    visibleAgentMessages: [],
    streamMessageID: null,
    responseText: "",
    partialText: "",
    lastPartialPersistedAt: 0,
    warning: null,
    failure: null,
  };
}

test("Codex에는 PNG와 JPEG만 이미지 인수로 전달한다", () => {
  const argumentsList = buildArguments({
    character: codexCharacter,
    prompt: "파일을 확인해줘.",
    previousSessionID: "session-1",
    attachments: [
      {
        path: "/tmp/photo.png",
        isCodexImage: true,
      },
      {
        path: "/tmp/document.pdf",
        isCodexImage: false,
      },
      {
        path: "/tmp/photo.jpeg",
        isCodexImage: true,
      },
    ],
  });

  assert.deepEqual(
    argumentsList.filter((value) => value.startsWith("/tmp/")),
    ["/tmp/photo.png", "/tmp/photo.jpeg"],
  );
  assert.equal(argumentsList.at(-1), "파일을 확인해줘.");
  assert.equal(argumentsList.includes("show_raw_agent_reasoning=true"), true);
  assert.equal(
    argumentsList.includes('model_reasoning_summary="detailed"'),
    true,
  );
  assert.equal(argumentsList.includes("features.fast_mode=true"), true);
  assert.equal(argumentsList.includes('service_tier="fast"'), true);
});

test("Codex는 Fast 비활성화도 신규 실행과 재개에 명시한다", () => {
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = buildArguments({
      character: { ...codexCharacter, fastMode: false },
      prompt: "상태를 확인해줘.",
      previousSessionID,
    });

    assert.equal(argumentsList.includes("features.fast_mode=true"), true);
    assert.equal(argumentsList.includes('service_tier="default"'), true);
  }
});

test("Codex는 CLI 기본 컨텍스트와 자동 압축 설정을 사용한다", () => {
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = buildArguments({
      character: codexCharacter,
      prompt: "긴 컨텍스트로 계속해줘.",
      previousSessionID,
    });

    assert.equal(
      argumentsList.some((value) =>
        value.startsWith("model_context_window=")
      ),
      false,
    );
    assert.equal(
      argumentsList.some((value) =>
        value.startsWith("model_auto_compact_token_limit=")
      ),
      false,
    );
  }
});

test("Codex 재개는 바뀐 모델·추론·Fast·권한을 같은 세션에 전달한다", () => {
  const argumentsList = buildArguments({
    character: {
      ...codexCharacter,
      model: "gpt-5.6-terra",
      effort: "xhigh",
      fastMode: false,
      permission: "danger-full-access",
    },
    prompt: "같은 세션에서 계속해줘.",
    previousSessionID: "session-1",
  });

  assert.deepEqual(argumentsList.slice(0, 4), [
    "exec",
    "resume",
    "session-1",
    "--json",
  ]);
  assert.equal(argumentsList.includes('model="gpt-5.6-terra"'), true);
  assert.equal(
    argumentsList.includes('model_reasoning_effort="xhigh"'),
    true,
  );
  assert.equal(argumentsList.includes('service_tier="default"'), true);
  assert.equal(
    argumentsList.includes('sandbox_mode="danger-full-access"'),
    true,
  );
});

test("GUI 실행은 동적으로 발견된 Codex 모델과 추론 레벨을 그대로 전달한다", () => {
  const argumentsList = buildArguments({
    character: {
      ...codexCharacter,
      model: "gpt-future-mini",
      effort: "medium",
      fastMode: false,
    },
    prompt: "새 모델을 사용해줘.",
    previousSessionID: "session-1",
  });

  assert.equal(argumentsList.includes('model="gpt-future-mini"'), true);
  assert.equal(
    argumentsList.includes('model_reasoning_effort="medium"'),
    true,
  );
});

test("Codex 신규와 재개는 DB 업무 지침과 짧은 결과 통로 안내만 전달한다", () => {
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = buildArguments({
      character: {
        ...codexCharacter,
        identityPrompt: "업데이트된 역할 지침을 따른다.",
      },
      prompt: "계속해줘.",
      previousSessionID,
    });
    const instructions = argumentsList.find((value) =>
      value.startsWith("developer_instructions=")
    );

    assert.equal(
      instructions,
      `developer_instructions=${JSON.stringify(
        identityPromptWithStructuredResult(
          "업데이트된 역할 지침을 따른다.",
        ),
      )}`,
    );
    assert.equal(argumentsList.at(-1), "계속해줘.");
  }
});

test("Claude는 Fast 설정을 매 실행마다 settings JSON으로 전달한다", () => {
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = buildArguments({
      character: {
        backend: "claude",
        model: "claude-opus-5",
        effort: "high",
        fastMode: true,
        permission: "auto",
        name: "클대리",
        seat: "좌측 아래",
        identityPrompt: "업무를 정확히 처리한다.",
      },
      prompt: "상태를 확인해줘.",
      previousSessionID,
    });
    const settingsIndex = argumentsList.indexOf("--settings");

    assert.notEqual(settingsIndex, -1);
    assert.deepEqual(
      JSON.parse(argumentsList[settingsIndex + 1]),
      { fastMode: true },
    );
  }
});

test("Claude 신규와 재개도 DB 업무 지침과 짧은 결과 통로 안내만 전달한다", () => {
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = buildArguments({
      character: {
        backend: "claude",
        model: "claude-opus-5",
        effort: "high",
        fastMode: true,
        permission: "bypassPermissions",
        name: "클대리",
        seat: "좌측 아래",
        identityPrompt: "업데이트된 역할 지침을 따른다.",
      },
      prompt: "계속해줘.",
      previousSessionID,
    });
    const identityIndex = argumentsList.indexOf("--append-system-prompt");
    const permissionIndex = argumentsList.indexOf("--permission-mode");

    assert.notEqual(identityIndex, -1);
    assert.equal(
      argumentsList[identityIndex + 1],
      identityPromptWithStructuredResult(
        "업데이트된 역할 지침을 따른다.",
      ),
    );
    assert.equal(argumentsList[permissionIndex + 1], "bypassPermissions");
    assert.equal(
      argumentsList.includes("--resume"),
      previousSessionID !== null,
    );
    assert.equal(
      argumentsList.includes("--exclude-dynamic-system-prompt-sections"),
      false,
    );
    const suggestionsIndex = argumentsList.indexOf("--prompt-suggestions");
    assert.notEqual(suggestionsIndex, -1);
    assert.equal(argumentsList[suggestionsIndex + 1], "true");
  }
});

test("Antigravity 신규와 재개는 모델·추론·대화 ID와 업무 폴더를 전달한다", () => {
  for (const previousSessionID of [null, "conversation-1"]) {
    const argumentsList = buildArguments({
      character: {
        backend: "antigravity",
        model: "gemini-3.7-flash",
        effort: "high",
        fastMode: false,
        permission: "accept-edits",
        identityPrompt: "업무 지시를 정확히 이해한다.",
      },
      prompt: "상태를 확인해줘.",
      previousSessionID,
      workdir: "/repo/project",
    });

    assert.equal(argumentsList[0], "-p");
    assert.match(argumentsList[1], /업무 지시를 정확히 이해한다/);
    assert.match(argumentsList[1], /상태를 확인해줘/);
    assert.match(argumentsList[1], /\/repo\/project/);
    assert.equal(
      argumentsList[argumentsList.indexOf("--add-dir") + 1],
      "/repo/project",
    );
    assert.equal(
      argumentsList[argumentsList.indexOf("--model") + 1],
      "gemini-3.7-flash",
    );
    assert.equal(
      argumentsList[argumentsList.indexOf("--effort") + 1],
      "high",
    );
    assert.equal(
      argumentsList[argumentsList.indexOf("--print-timeout") + 1],
      "24h",
    );
    assert.equal(argumentsList.includes("--sandbox"), true);
    assert.equal(
      argumentsList.includes("--dangerously-skip-permissions"),
      true,
    );
    assert.equal(
      argumentsList.includes("--conversation"),
      previousSessionID !== null,
    );
    assert.equal(argumentsList.includes("service_tier=\"fast\""), false);
    assert.equal(
      argumentsList.some((value) =>
        String(value).startsWith("model_context_window=")
      ),
      false,
    );
  }
});

test("Antigravity 공통 권한 3단계를 CLI 안전 모드로 변환한다", () => {
  const argumentsFor = (permission) => buildArguments({
    character: {
      backend: "antigravity",
      model: "gemini-3.1-pro",
      effort: "high",
      fastMode: false,
      permission,
      identityPrompt: "정확히 처리한다.",
    },
    prompt: "확인해줘.",
    previousSessionID: null,
    workdir: "/repo",
  });

  const readOnly = argumentsFor("plan");
  assert.deepEqual(
    readOnly.slice(readOnly.indexOf("--mode")),
    ["--mode", "plan"],
  );
  const workspaceWrite = argumentsFor("accept-edits");
  assert.equal(workspaceWrite.includes("--sandbox"), true);
  const fullAccess = argumentsFor("dangerously-skip-permissions");
  assert.equal(fullAccess.includes("--sandbox"), false);
  assert.equal(fullAccess.includes("--dangerously-skip-permissions"), true);
});

test("Antigravity 단계별 사용량은 결과 누적값과 중복하지 않고 합산한다", () => {
  const first = {
    inputTokens: 100,
    outputTokens: 10,
    cachedInputTokens: 80,
    reasoningOutputTokens: 3,
  };
  const second = {
    inputTokens: 40,
    outputTokens: 5,
    cachedInputTokens: 20,
    reasoningOutputTokens: 2,
  };

  assert.deepEqual(accumulatedUsage(first, second), {
    inputTokens: 140,
    outputTokens: 15,
    cachedInputTokens: 100,
    reasoningOutputTokens: 5,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reportedCostUsd: null,
    reportedSonnet5CostUsd: null,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
  });
});

test("Claude 지속 세션은 prompt를 인수가 아닌 stream-json stdin으로 받는다", () => {
  const character = {
    backend: "claude",
    model: "claude-opus-5",
    effort: "xhigh",
    fastMode: false,
    permission: "bypassPermissions",
    identityPrompt: "업데이트된 역할 지침을 따른다.",
  };
  for (const previousSessionID of [null, "session-1"]) {
    const argumentsList = claudePersistentArguments(
      character,
      previousSessionID,
    );
    assert.equal(argumentsList[0], "-p");
    assert.equal(argumentsList.includes("--input-format"), true);
    assert.equal(
      argumentsList[argumentsList.indexOf("--input-format") + 1],
      "stream-json",
    );
    assert.equal(argumentsList.includes("--include-partial-messages"), true);
    assert.equal(argumentsList.includes("--prompt-suggestions"), true);
    assert.equal(
      argumentsList[argumentsList.indexOf("--append-system-prompt") + 1],
      identityPromptWithStructuredResult(character.identityPrompt),
    );
    assert.equal(
      argumentsList.includes("--resume"),
      previousSessionID !== null,
    );
    assert.equal(
      argumentsList.includes("--exclude-dynamic-system-prompt-sections"),
      false,
    );
    assert.equal(argumentsList.includes("테스트 prompt"), false);
  }
});

test("Claude 지속 세션 서명은 캐시를 바꾸는 설정과 작업 공간을 구분한다", () => {
  const character = {
    backend: "claude",
    model: "claude-opus-5",
    effort: "high",
    fastMode: false,
    permission: "auto",
    identityPrompt: "업무 지침",
  };
  const original = claudePersistentWorkerSignature({
    character,
    executable: "/bin/claude",
    workdir: "/repo/a",
  });
  assert.equal(
    original,
    claudePersistentWorkerSignature({
      character: { ...character },
      executable: "/bin/claude",
      workdir: "/repo/a",
    }),
  );
  assert.notEqual(
    original,
    claudePersistentWorkerSignature({
      character: { ...character, identityPrompt: "바뀐 지침" },
      executable: "/bin/claude",
      workdir: "/repo/a",
    }),
  );
  assert.notEqual(
    original,
    claudePersistentWorkerSignature({
      character,
      executable: "/bin/claude",
      workdir: "/repo/b",
    }),
  );
});

test("AgentRuntime은 같은 Claude 설정·세션·작업 공간에서 worker 하나를 재사용한다", () => {
  const created = [];
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
    claudeWorkerFactory: (options) => {
      const worker = {
        child: {},
        signature: options.signature,
        sessionID: options.sessionID,
        closed: false,
        matches({ signature, sessionID }) {
          return !this.closed &&
            this.signature === signature &&
            (this.sessionID ?? null) === (sessionID ?? null);
        },
        close() {
          this.closed = true;
          options.onExit(this);
        },
      };
      created.push(worker);
      return worker;
    },
  });
  const state = {
    character: {
      id: "left-woman",
      backend: "claude",
      model: "claude-opus-5",
      effort: "high",
      fastMode: false,
      permission: "auto",
      identityPrompt: "업무 지침",
    },
    workdir: "/repo/worktree",
    externalSessionID: "session-1",
  };

  const first = runtime.acquireClaudeWorker(state, "/bin/claude");
  const second = runtime.acquireClaudeWorker(state, "/bin/claude");
  assert.equal(first, second);
  assert.equal(created.length, 1);

  const replacement = runtime.acquireClaudeWorker({
    ...state,
    character: { ...state.character, effort: "xhigh" },
  }, "/bin/claude");
  assert.notEqual(replacement, first);
  assert.equal(first.closed, true);
  assert.equal(created.length, 2);

  runtime.shutdown();
  assert.equal(replacement.closed, true);
  assert.equal(runtime.claudeWorkers.size, 0);
});

test("연속 Claude 업무는 같은 worker stdin 턴으로 실행하고 각각 완료한다", async () => {
  const prompts = [];
  const completed = [];
  let spawnCount = 0;
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
    claudeWorkerFactory: (options) => {
      spawnCount += 1;
      return {
        child: {},
        signature: options.signature,
        sessionID: options.sessionID,
        matches({ signature, sessionID }) {
          return this.signature === signature &&
            (this.sessionID ?? null) === (sessionID ?? null);
        },
        async runTurn({ prompt, onLine }) {
          prompts.push(prompt);
          await onLine(JSON.stringify({
            type: "result",
            subtype: "success",
            is_error: false,
            result: `응답: ${prompt}`,
          }));
        },
        close() {},
      };
    },
  });
  runtime.persistResponseDraft = async () => {};
  runtime.complete = async (state, decoded) => {
    completed.push({ turnID: state.turnID, text: decoded.text });
  };
  const character = {
    id: "left-woman",
    backend: "claude",
    model: "claude-opus-5",
    effort: "high",
    fastMode: false,
    permission: "auto",
    identityPrompt: "업무 지침",
  };
  const state = (turnID, prompt) => ({
    turnID,
    character,
    workdir: "/repo/worktree",
    prompt,
    executionPrompt: prompt,
    externalSessionID: null,
    responseText: "",
    partialText: "",
    visibleAgentMessages: [],
    lastPartialPersistedAt: 0,
    cancelRequested: false,
    failure: null,
  });

  await runtime.executeClaude(state("turn-1", "첫 질문"));
  await runtime.executeClaude(state("turn-2", "둘째 질문"));

  assert.equal(spawnCount, 1);
  assert.deepEqual(prompts, ["첫 질문", "둘째 질문"]);
  assert.deepEqual(completed, [
    { turnID: "turn-1", text: "응답: 첫 질문" },
    { turnID: "turn-2", text: "응답: 둘째 질문" },
  ]);
});

test("Claude 실행은 직원별 자동 압축 기준과 명시적 업데이트를 사용한다", () => {
  const baseEnvironment = {
    PATH: "/tmp/bin",
    CLAUDE_CODE_AUTO_COMPACT_WINDOW: "250000",
    CLAUDE_AUTOCOMPACT_PCT_OVERRIDE: "65",
  };
  const claudeEnvironment = executionEnvironment(
    { backend: "claude", autoCompactPercent: 93 },
    baseEnvironment,
  );

  assert.notEqual(claudeEnvironment, baseEnvironment);
  assert.equal(claudeEnvironment.PATH, "/tmp/bin");
  assert.equal(
    Object.hasOwn(
      claudeEnvironment,
      "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    ),
    false,
  );
  assert.equal(claudeEnvironment.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE, "93");
  assert.equal(claudeEnvironment.DISABLE_AUTOUPDATER, "1");

  const codexEnvironment = { PATH: "/tmp/bin" };
  assert.equal(
    executionEnvironment({ backend: "codex" }, codexEnvironment),
    codexEnvironment,
  );

  const antigravityEnvironment = executionEnvironment(
    { backend: "antigravity" },
    { PATH: "/tmp/bin" },
  );
  assert.match(
    antigravityEnvironment.PLAYWRIGHT_DRIVER_PATH,
    /playwright-driver-1\.57\.0$/,
  );
  assert.ok(antigravityEnvironment.PLAYWRIGHT_NODEJS_PATH);

  for (const backend of ["codex", "claude", "antigravity"]) {
    const character = { id: `${backend}-worker`, backend };
    const environment = executionEnvironment(
      character,
      { PATH: "/tmp/bin" },
      { workdir: "/repo/project" },
    );
    assert.equal(
      environment[STRUCTURED_RESULT_ENV],
      structuredTurnResultPath({
        workdir: "/repo/project",
        characterID: character.id,
      }),
    );
    assert.equal(
      environment.PATH.split(":")[0],
      structuredResultToolDirectory,
    );
    assert.match(environment.PATH, /\/tmp\/bin/);
  }
});

function startedRuntimeState({
  prompt,
  attachmentPaths = [],
  searchableDocuments = [],
} = {}) {
  const capture = { state: null, queries: [] };
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        capture.queries.push({ text, values });
        // 과거 기록이 검색되는 상황을 만든다. 자동 주입이 남아 있으면
        // 이 문서들이 executionPrompt로 흘러든다.
        return { rowCount: searchableDocuments.length, rows: searchableDocuments };
      },
    },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });
  runtime.prepareTurn = async ({ prompt: preparedPrompt, conversationID }) => ({
    turnID: "turn-1",
    conversationID,
    character: { id: "left-woman", backend: "claude", model: "claude-opus-5" },
    prompt: preparedPrompt,
    externalSessionID: null,
    workspaceID: null,
    isolateGitWorkdir: false,
  });
  runtime.ensureWorkspace = async () => null;
  runtime.beginPreparedTurn = async () => {};
  runtime.execute = async (state) => {
    capture.state = state;
  };
  return { runtime, capture, prompt, attachmentPaths };
}

test("일반 업무 시작은 과거 작업 기록을 자동으로 주입하지 않는다", async () => {
  const searchableDocuments = [{
    ragDocumentId: "rag-1",
    workRecordId: "record-1",
    title: "악성 채굴 분석",
    excerpt: "SSH root 침해로 XMRig가 설치됐다.",
    score: 0.9,
  }];
  const { runtime, capture } = startedRuntimeState({ searchableDocuments });

  await runtime.start({
    characterID: "left-woman",
    prompt: "테스트",
    conversationID: "conversation-1",
  });

  assert.ok(capture.state, "업무가 실행 상태까지 진행되어야 합니다.");
  assert.equal(capture.state.executionPrompt, "테스트");
  assert.equal(capture.state.recordPrompt, "테스트");
  assert.doesNotMatch(
    capture.state.executionPrompt,
    /office_retrieved_records/,
  );
  assert.doesNotMatch(capture.state.executionPrompt, /rag-1|record-1/);
  assert.doesNotMatch(capture.state.executionPrompt, /악성 채굴|XMRig/);
  assert.doesNotMatch(capture.state.executionPrompt, /비신뢰 참고 데이터/);
  assert.doesNotMatch(capture.state.executionPrompt, /이전 업무 기록 참고 자료/);
  assert.equal(
    capture.queries.some(({ text }) =>
      /searchable_rag_documents/.test(String(text ?? ""))
    ),
    false,
    "일반 업무 시작 경로가 RAG 문서를 검색하면 안 됩니다.",
  );
});

test("첨부 안내는 executionPrompt와 recordPrompt에 그대로 유지된다", async () => {
  const attachmentRoot = mkdtempSync(join(tmpdir(), "office-attachment-"));
  const attachmentPath = join(attachmentRoot, "보고서.png");
  writeFileSync(attachmentPath, "이미지");
  const { runtime, capture } = startedRuntimeState();
  runtime.workdir = attachmentRoot;

  try {
    await runtime.start({
      characterID: "left-woman",
      prompt: "이 화면 봐줘",
      conversationID: "conversation-3",
      attachmentPaths: [attachmentPath],
    });

    assert.match(capture.state.executionPrompt, /이 화면 봐줘/);
    assert.match(capture.state.executionPrompt, /보고서\.png/);
    assert.equal(
      capture.state.executionPrompt,
      capture.state.recordPrompt,
      "자동 주입 제거 후 실행 프롬프트와 기록 프롬프트는 같아야 합니다.",
    );
    assert.doesNotMatch(
      capture.state.executionPrompt,
      /office_retrieved_records/,
    );
  } finally {
    rmSync(attachmentRoot, { recursive: true, force: true });
  }
});

test("CLI 인수는 사용자 요청만 담고 과거 기록 블록을 만들지 않는다", () => {
  const executionPrompt = "세션 유지 상태를 확인해줘.";
  const codexArgumentsList = buildArguments({
    character: codexCharacter,
    prompt: executionPrompt,
    previousSessionID: "session-1",
  });
  const codexInstructions = codexArgumentsList.find((value) =>
    value.startsWith("developer_instructions=")
  );

  assert.equal(codexArgumentsList.at(-1), executionPrompt);
  assert.doesNotMatch(codexArgumentsList.at(-1), /office_retrieved_records/);
  assert.doesNotMatch(codexInstructions, /office_retrieved_records/);

  const claudeArgumentsList = buildArguments({
    character: claudeResumeCharacter,
    prompt: executionPrompt,
    previousSessionID: null,
  });
  const systemIndex = claudeArgumentsList.indexOf("--append-system-prompt");

  assert.equal(claudeArgumentsList[1], executionPrompt);
  assert.equal(
    claudeArgumentsList[systemIndex + 1],
    identityPromptWithStructuredResult(claudeResumeCharacter.identityPrompt),
  );
  assert.doesNotMatch(
    claudeArgumentsList[systemIndex + 1],
    /OFFICE_SOURCES|OFFICE_WIKI_PROPOSALS/,
  );
});

const claudeResumeCharacter = {
  backend: "claude",
  model: "claude-sonnet-5",
  effort: "high",
  fastMode: false,
  permission: "bypassPermissions",
  name: "클대리",
  seat: "좌측 아래",
  identityPrompt: "업무 지시를 정확히 이해한다.",
};

function withClaudeSessionHome(run) {
  const home = mkdtempSync(join(tmpdir(), "office-claude-home-"));
  const workdir = mkdtempSync(join(tmpdir(), "office-claude-workdir-"));
  const originalHome = process.env.HOME;
  process.env.HOME = home;
  try {
    return run({ workdir });
  } finally {
    if (originalHome === undefined) {
      delete process.env.HOME;
    } else {
      process.env.HOME = originalHome;
    }
    rmSync(home, { recursive: true, force: true });
    rmSync(workdir, { recursive: true, force: true });
  }
}

function writeClaudeSession(workdir, sessionID) {
  const path = claudeSessionPath(workdir, sessionID);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `{"sessionId":"${sessionID}"}\n`);
  return path;
}

test("Claude 세션 경로는 실행 디렉토리를 그대로 반영한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const path = claudeSessionPath(workdir, "session-1");
    const encoded = realpathSync(workdir).replace(/[/.]/g, "-");

    assert.equal(path.endsWith(join(encoded, "session-1.jsonl")), true);
    assert.equal(claudeSessionResumable(workdir, "session-1"), false);

    writeClaudeSession(workdir, "session-1");
    assert.equal(claudeSessionResumable(workdir, "session-1"), true);
  });
});

test("Claude 저장 ID는 유지하되 로컬 기록이 없으면 실행 전에 실패한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    assert.throws(
      () => prepareClaudeSessionResume({
        sessionID: "session-1",
        workdir,
      }),
      /저장된 Claude Code 세션 기록을 찾을 수 없습니다/,
    );
    const argumentsList = buildArguments({
      character: claudeResumeCharacter,
      prompt: "계속해줘.",
      previousSessionID: "session-1",
      workdir,
    });
    const resumeIndex = argumentsList.indexOf("--resume");

    assert.notEqual(resumeIndex, -1);
    assert.equal(argumentsList[resumeIndex + 1], "session-1");
    assert.equal(claudeSessionResumable(workdir, "session-1"), false);
  });
});

test("Claude는 작업 공간이 바뀌면 같은 transcript inode로 재개한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const previousWorkdir = mkdtempSync(join(tmpdir(), "office-claude-old-"));
    try {
      const source = writeClaudeSession(previousWorkdir, "session-1");
      const sourceSidecar = join(dirname(source), "session-1");
      mkdirSync(join(sourceSidecar, "subagents"), { recursive: true });
      mkdirSync(join(sourceSidecar, "tool-results"), { recursive: true });
      writeFileSync(
        join(sourceSidecar, "subagents", "agent-1.jsonl"),
        '{"message":"검토 완료"}\n',
      );
      assert.equal(claudeSessionResumable(workdir, "session-1"), false);

      const target = prepareClaudeSessionResume({
        sessionID: "session-1",
        workdir,
        previousWorkdir,
      });

      const argumentsList = buildArguments({
        character: claudeResumeCharacter,
        prompt: "계속해줘.",
        previousSessionID: "session-1",
        workdir,
      });
      const resumeIndex = argumentsList.indexOf("--resume");

      assert.notEqual(resumeIndex, -1);
      assert.equal(argumentsList[resumeIndex + 1], "session-1");
      assert.equal(claudeSessionResumable(workdir, "session-1"), true);
      assert.equal(existsSync(source), true);
      assert.equal(statSync(target).dev, statSync(source).dev);
      assert.equal(statSync(target).ino, statSync(source).ino);
      appendFileSync(target, '{"continued":true}\n');
      assert.equal(readFileSync(source, "utf8").includes("continued"), true);
      const targetSidecar = join(dirname(target), "session-1");
      assert.equal(realpathSync(targetSidecar), realpathSync(sourceSidecar));
      writeFileSync(
        join(targetSidecar, "tool-results", "result.txt"),
        "같은 부속 기록",
      );
      assert.equal(
        readFileSync(
          join(sourceSidecar, "tool-results", "result.txt"),
          "utf8",
        ),
        "같은 부속 기록",
      );
    } finally {
      rmSync(previousWorkdir, { recursive: true, force: true });
    }
  });
});

test("Claude 사용량 복구는 마지막 실제 메시지가 최신인 기록을 찾는다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const firstWorkdir = mkdtempSync(join(tmpdir(), "office-claude-first-"));
    const secondWorkdir = mkdtempSync(join(tmpdir(), "office-claude-second-"));
    try {
      const firstPath = writeClaudeSession(firstWorkdir, "session-1");
      const secondPath = writeClaudeSession(secondWorkdir, "session-1");
      writeFileSync(
        firstPath,
        '{"type":"assistant","timestamp":"2026-08-18T00:00:00Z"}\n' +
          "x".repeat(2_000),
      );
      writeFileSync(
        secondPath,
        '{"type":"assistant","timestamp":"2026-08-19T00:00:00Z"}\n',
      );
      utimesSync(firstPath, new Date(3_000), new Date(3_000));
      utimesSync(secondPath, new Date(2_000), new Date(2_000));

      assert.equal(findClaudeSessionPath("session-1"), secondPath);
    } finally {
      rmSync(firstWorkdir, { recursive: true, force: true });
      rmSync(secondWorkdir, { recursive: true, force: true });
    }
  });
});

test("Claude 재개는 기준 분기를 연결하고 다른 분기는 그대로 보존한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const previousWorkdir = mkdtempSync(join(tmpdir(), "office-claude-first-"));
    const divergentWorkdir = mkdtempSync(join(tmpdir(), "office-claude-second-"));
    try {
      const source = writeClaudeSession(previousWorkdir, "session-1");
      const divergent = writeClaudeSession(divergentWorkdir, "session-1");
      writeFileSync(
        divergent,
        '{"sessionId":"session-1","branch":"divergent"}\n',
      );

      const target = prepareClaudeSessionResume({
        sessionID: "session-1",
        workdir,
        previousWorkdir,
      });

      assert.equal(existsSync(source), true);
      assert.equal(existsSync(divergent), true);
      assert.equal(statSync(target).ino, statSync(source).ino);
      assert.notEqual(statSync(target).ino, statSync(divergent).ino);
    } finally {
      rmSync(previousWorkdir, { recursive: true, force: true });
      rmSync(divergentWorkdir, { recursive: true, force: true });
    }
  });
});

test("Claude 재개는 현재 작업 공간의 다른 분기를 덮어쓰지 않는다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const previousWorkdir = mkdtempSync(join(tmpdir(), "office-claude-old-"));
    try {
      const source = writeClaudeSession(previousWorkdir, "session-1");
      const target = writeClaudeSession(workdir, "session-1");
      writeFileSync(target, '{"sessionId":"session-1","branch":"other"}\n');

      assert.throws(
        () => prepareClaudeSessionResume({
          sessionID: "session-1",
          workdir,
          previousWorkdir,
        }),
        /서로 다른 Claude Code 세션 기록/,
      );
      assert.equal(existsSync(source), true);
      assert.equal(readFileSync(target, "utf8").includes("other"), true);
    } finally {
      rmSync(previousWorkdir, { recursive: true, force: true });
    }
  });
});

test("Claude 재개는 현재 작업 공간의 다른 부속 기록도 보존한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const previousWorkdir = mkdtempSync(join(tmpdir(), "office-claude-old-"));
    try {
      const source = writeClaudeSession(previousWorkdir, "session-1");
      mkdirSync(join(dirname(source), "session-1", "subagents"), {
        recursive: true,
      });
      const target = claudeSessionPath(workdir, "session-1");
      const targetSidecar = join(dirname(target), "session-1");
      mkdirSync(targetSidecar, { recursive: true });
      writeFileSync(join(targetSidecar, "keep.txt"), "보존");

      assert.throws(
        () => prepareClaudeSessionResume({
          sessionID: "session-1",
          workdir,
          previousWorkdir,
        }),
        /서로 다른 Claude Code 세션 부속 기록/,
      );
      assert.equal(existsSync(target), false);
      assert.equal(readFileSync(join(targetSidecar, "keep.txt"), "utf8"), "보존");
    } finally {
      rmSync(previousWorkdir, { recursive: true, force: true });
    }
  });
});

test("Claude는 같은 작업 공간에 세션이 남아 있으면 재개한다", () => {
  withClaudeSessionHome(({ workdir }) => {
    writeClaudeSession(workdir, "session-1");
    const argumentsList = buildArguments({
      character: claudeResumeCharacter,
      prompt: "계속해줘.",
      previousSessionID: "session-1",
      workdir,
    });
    const resumeIndex = argumentsList.indexOf("--resume");

    assert.notEqual(resumeIndex, -1);
    assert.equal(argumentsList[resumeIndex + 1], "session-1");
  });
});

// Fast 지원 여부는 Claude Code 모델 카탈로그가 알려주고 직원 설정 저장 때
// 검증하므로, 실행 인자는 모델 이름을 따지지 않고 설정값을 그대로 전달한다.
test("Claude는 Fast 설정을 모델 이름과 무관하게 그대로 명시한다", () => {
  const argumentsList = buildArguments({
    character: {
      backend: "claude",
      model: "claude-sonnet-5",
      effort: "high",
      fastMode: false,
      permission: "auto",
      name: "클대리",
      seat: "좌측 아래",
      identityPrompt: "업무를 정확히 처리한다.",
    },
    prompt: "상태를 확인해줘.",
    previousSessionID: null,
  });
  const settingsIndex = argumentsList.indexOf("--settings");
  assert.deepEqual(
    JSON.parse(argumentsList[settingsIndex + 1]),
    { fastMode: false },
  );

  const fastArguments = buildArguments({
    character: {
      backend: "claude",
      model: "opus[1m]",
      effort: "high",
      fastMode: true,
      permission: "auto",
      name: "클대리",
      seat: "좌측 아래",
      identityPrompt: "업무를 정확히 처리한다.",
    },
    prompt: "상태를 확인해줘.",
    previousSessionID: null,
  });
  assert.deepEqual(
    JSON.parse(fastArguments[fastArguments.indexOf("--settings") + 1]),
    { fastMode: true },
  );
  assert.equal(fastArguments[fastArguments.indexOf("--model") + 1], "opus[1m]");
});

test("같은 이벤트 ID의 시작과 완료는 한 활동 행을 갱신한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    turnID: "turn-1",
    character: { id: "right-man" },
    sequence: 0,
    lastActivity: null,
    activityRecords: new Map(),
  };

  await runtime.addActivity(state, {
    kind: "command",
    text: "swift test",
    eventKey: "command-1",
    status: "running",
  });
  await runtime.addActivity(state, {
    kind: "command",
    text: "swift test",
    eventKey: "command-1",
    status: "completed",
  });

  const activityQueries = queries.filter(({ text }) =>
    /turn_activities/.test(text)
  );
  assert.equal(activityQueries.length, 2);
  assert.match(activityQueries[0].text, /INSERT INTO turn_activities/);
  assert.match(activityQueries[1].text, /UPDATE turn_activities/);
  assert.equal(state.sequence, 1);
  assert.equal(
    state.activityRecords.get("command-1").status,
    "completed",
  );
});

test("동시에 도착한 활동은 서로 다른 순번과 이벤트 행을 유지한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        if (/INSERT INTO turn_activities/.test(text)) {
          await new Promise((resolve) => setImmediate(resolve));
        }
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();

  await Promise.all([
    runtime.addActivity(state, {
      kind: "tool",
      text: "첫 활동",
      eventKey: "activity-a",
      status: "running",
    }),
    runtime.addActivity(state, {
      kind: "tool",
      text: "둘째 활동",
      eventKey: "activity-b",
      status: "running",
    }),
  ]);
  await runtime.addActivity(state, {
    kind: "tool",
    text: "첫 활동 완료",
    eventKey: "activity-a",
    status: "completed",
  });

  assert.equal(state.activityRecords.get("activity-a").sequence, 1);
  assert.equal(state.activityRecords.get("activity-b").sequence, 2);
  const inserts = queries.filter(({ text }) =>
    /INSERT INTO turn_activities/.test(text)
  );
  assert.deepEqual(inserts.map(({ values }) => values[1]), [1, 2]);
  const update = queries.find(({ text }) =>
    /UPDATE turn_activities/.test(text) && /kind = \$3/.test(text)
  );
  assert.equal(update.values[1], 1);
});

test("협업 활동은 검토자 정보와 결과를 JSON으로 저장한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    turnID: "turn-1",
    character: { id: "boss" },
    sequence: 0,
    lastActivity: null,
    activityRecords: new Map(),
  };

  await runtime.addActivity(state, {
    kind: "collaboration",
    text: "접힘 정책을 검토해 주세요.",
    eventKey: "collaboration:spawn-1:reviewer-1",
    status: "running",
    collaboration: {
      action: "spawn",
      agentThreadId: "reviewer-1",
      prompt: "접힘 정책을 검토해 주세요.",
      agentStatus: "running",
    },
  });
  await runtime.addActivity(state, {
    kind: "collaboration",
    text: "회귀 위험이 없습니다.",
    eventKey: "collaboration:spawn-1:reviewer-1",
    status: "completed",
    collaboration: {
      action: "result",
      agentThreadId: "reviewer-1",
      message: "회귀 위험이 없습니다.",
      agentStatus: "completed",
    },
  });

  const activityQueries = queries.filter(({ text }) =>
    /turn_activities/.test(text)
  );
  assert.equal(activityQueries.length, 2);
  assert.match(activityQueries[0].text, /collaboration/);
  assert.equal(activityQueries[0].values[2], "collaboration");
  assert.deepEqual(activityQueries[0].values[6], {
    action: "spawn",
    agentThreadId: "reviewer-1",
    prompt: "접힘 정책을 검토해 주세요.",
    agentStatus: "running",
  });
  assert.match(activityQueries[1].text, /collaboration = \$6/);
  assert.deepEqual(activityQueries[1].values[5], {
    action: "result",
    agentThreadId: "reviewer-1",
    message: "회귀 위험이 없습니다.",
    agentStatus: "completed",
  });
});

test("부모 턴 종료는 남은 협업 카드의 구조화 상태도 함께 종료한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.activityRecords.set("collaboration:agent-1", {
    sequence: 1,
    kind: "collaboration",
    text: "검토 중",
    status: "running",
    collaboration: {
      action: "spawn",
      agentThreadId: "agent-1",
      agentStatus: "running",
    },
  });

  await runtime.finalizeRunningActivities(state, "completed");

  const query = queries.find(({ text }) =>
    /UPDATE turn_activities/.test(text) && /jsonb_set/.test(text)
  );
  assert.deepEqual(query.values, ["turn-1", "completed", "completed"]);
  assert.equal(
    state.activityRecords.get("collaboration:agent-1").status,
    "completed",
  );
  assert.equal(
    state.activityRecords.get("collaboration:agent-1")
      .collaboration.agentStatus,
    "completed",
  );
});

test("협업 활동 스키마와 실시간 API가 구조화 내용을 함께 제공한다", () => {
  const migrationSource = readFileSync(
    new URL(
      "../../database/migrations/020_collaboration_activities.sql",
      import.meta.url,
    ),
    "utf8",
  );
  const serverSource = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );

  assert.match(migrationSource, /'collaboration'/);
  assert.match(migrationSource, /'suggestion'/);
  assert.match(
    migrationSource,
    /ADD COLUMN IF NOT EXISTS collaboration jsonb/,
  );
  assert.match(
    serverSource,
    /'collaboration', activity\.collaboration/,
  );
});

test("다음 질문 추천 활동은 DB 허용 종류에 포함된다", () => {
  const migrationSource = readFileSync(
    new URL(
      "../../database/migrations/024_prompt_suggestion_activities.sql",
      import.meta.url,
    ),
    "utf8",
  );

  assert.match(migrationSource, /'suggestion'/);
  assert.match(migrationSource, /turn_activities_kind_check/);
});

test("같은 활동 상태가 반복되면 저장과 방송을 반복하지 않는다", async () => {
  const queries = [];
  const broadcasts = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: (event) => broadcasts.push(event),
  });
  const state = makeCodexActivityState();

  await runtime.addActivity(state, {
    kind: "tool",
    text: "도구 · Read · Feed.swift",
    eventKey: "tool-1",
    status: "running",
  });
  await runtime.addActivity(state, {
    kind: "tool",
    text: "도구 · Read · Feed.swift",
    eventKey: "tool-1",
    status: "running",
  });

  assert.equal(
    queries.filter(({ text }) => /turn_activities/.test(text)).length,
    1,
  );
  assert.equal(broadcasts.length, 1);
});

test("첫 실제 Codex reasoning은 다음 활동까지 같은 행의 실행 중 상태로 저장한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  const stream = Readable.from([
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "reason-1",
        type: "reasoning",
        text: "실제 구조를 확인하고 있습니다.",
      },
    })}\n`,
    `${JSON.stringify({
      type: "item.started",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "swift test",
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);

  const activityQueries = queries.filter(({ text }) =>
    /turn_activities/.test(text)
  );
  assert.equal(activityQueries.length, 3);
  assert.match(activityQueries[0].text, /INSERT INTO turn_activities/);
  assert.equal(activityQueries[0].values[3], "실제 구조를 확인하고 있습니다.");
  assert.equal(activityQueries[0].values[5], "running");
  assert.match(activityQueries[1].text, /INSERT INTO turn_activities/);
  assert.equal(activityQueries[1].values[4], "command-1");
  assert.match(activityQueries[2].text, /UPDATE turn_activities/);
  assert.equal(activityQueries[2].values[4], "completed");
  assert.doesNotMatch(activityQueries[2].text, /occurred_at\s*=/);
  assert.equal(state.activityRecords.get("reason-1").status, "completed");
});

test("첫 Codex reasoning 뒤 바로 스트림이 끝나도 실행 중 상태를 남기지 않는다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  const stream = Readable.from([
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "reason-1",
        type: "reasoning",
        text: "실제 추론 한 건입니다.",
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);

  const activityQueries = queries.filter(({ text }) =>
    /turn_activities/.test(text)
  );
  assert.equal(activityQueries.length, 2);
  assert.equal(activityQueries[0].values[5], "running");
  assert.equal(activityQueries[1].values[4], "completed");
  assert.equal(state.pendingInitialCodexReasoning, null);
});

test("두 번째 Codex reasoning은 시작 상태를 합성하지 않고 완료로 저장한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  const stream = Readable.from([
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "reason-1",
        type: "reasoning",
        text: "첫 실제 추론입니다.",
      },
    })}\n`,
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "reason-2",
        type: "reasoning",
        text: "두 번째 실제 추론입니다.",
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);

  const activityQueries = queries.filter(({ text }) =>
    /turn_activities/.test(text)
  );
  assert.equal(activityQueries.length, 3);
  assert.equal(activityQueries[0].values[5], "running");
  assert.equal(activityQueries[1].values[3], "두 번째 실제 추론입니다.");
  assert.equal(activityQueries[1].values[5], "completed");
  assert.equal(activityQueries[2].values[4], "completed");
});

test("공개 진행 설명은 활동에 남기고 응답 본문에도 누적 유지한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    turnID: "turn-1",
    character: { id: "right-man", backend: "codex" },
    externalSessionID: "session-1",
    sequence: 0,
    lastActivity: null,
    activityRecords: new Map(),
    pendingAgentMessage: null,
    visibleAgentMessages: [],
    streamMessageID: null,
    responseText: "",
    partialText: "",
    lastPartialPersistedAt: 0,
    warning: null,
    failure: null,
  };
  const lines = [
    {
      type: "item.completed",
      item: {
        id: "message-1",
        type: "agent_message",
        text: "현재 구조를 확인했습니다.",
      },
    },
    {
      type: "item.started",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "swift test",
      },
    },
    {
      type: "item.completed",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "swift test",
        exit_code: 0,
      },
    },
    {
      type: "item.completed",
      item: {
        id: "message-2",
        type: "agent_message",
        text: "검증을 통과했습니다.",
      },
    },
  ];
  const stream = Readable.from(
    lines.map((line) => `${JSON.stringify(line)}\n`),
  );

  await runtime.consumeOutput(state, stream);

  assert.equal(state.sequence, 3);
  assert.equal(state.responseText, "검증을 통과했습니다.");
  assert.deepEqual(
    state.visibleAgentMessages.map((message) => message.text),
    ["현재 구조를 확인했습니다.", "검증을 통과했습니다."],
  );
  assert.equal(
    state.activityRecords.get("message:message-1").text,
    "현재 구조를 확인했습니다.",
  );
  assert.equal(
    state.activityRecords.get("message:message-2").text,
    "검증을 통과했습니다.",
    "마지막 Codex 메시지도 후속 도구 없이 즉시 활동으로 저장한다",
  );
  assert.equal(
    state.activityRecords.get("command-1").status,
    "completed",
  );
  const responseDrafts = queries
    .filter(({ text }) => /UPDATE messages/.test(text))
    .map(({ values }) => values[1]);
  assert.equal(responseDrafts.includes(""), false);
  assert.equal(
    responseDrafts.at(-1),
    "현재 구조를 확인했습니다.\n\n검증을 통과했습니다.",
  );
  assert.equal(
    runtime.completedResponseText(state, {
      text: "검증을 통과했습니다.",
      needsInput: false,
    }),
    "현재 구조를 확인했습니다.\n\n검증을 통과했습니다.",
  );
  state.responseText = "";
  state.partialText = "";
  assert.equal(
    runtime.finalResponseCandidate(state),
    "검증을 통과했습니다.",
  );
});

test("여러 조각으로 나뉜 응답은 조각마다 기계 블록을 떼고 합친다", () => {
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 1 }) },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const early =
    "앞부분 분석입니다.\n\n[OFFICE_SOURCES]\n" +
    '[{"kind":"file","title":"러너","locator":"backend/src/migrate.mjs","excerpt":"e"}]';
  const finalRaw =
    "끝났습니다.\n\n[OFFICE_SOURCES]\n" +
    '[{"kind":"file","title":"완료","locator":"backend/src/db.mjs","excerpt":"e"}]';
  const state = {
    responseText: finalRaw,
    partialText: "",
    visibleAgentMessages: [
      { key: "message-1", text: early },
      { key: "message-2", text: finalRaw },
    ],
  };
  const decoded = decodeAgentResponse(finalRaw);

  const joined = runtime.completedResponseText(state, decoded);

  assert.equal(
    joined.includes("[OFFICE_SOURCES]"),
    false,
    "이어붙인 본문 어디에도 기계 블록이 날것으로 남지 않는다",
  );
  assert.equal(joined, "앞부분 분석입니다.\n\n끝났습니다.");
});

test("서로 다른 ID의 같은 문장은 각각 응답 순서에 남긴다", async () => {
  const runtime = new AgentRuntime({
    pool: {
      query: async () => ({ rowCount: 1 }),
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  const stream = Readable.from([
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "message-1",
        type: "agent_message",
        text: "같은 문장",
      },
    })}\n`,
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "message-2",
        type: "agent_message",
        text: "같은 문장",
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);

  assert.deepEqual(state.visibleAgentMessages, [
    { key: "message-1", text: "같은 문장" },
    { key: "message-2", text: "같은 문장" },
  ]);
  assert.equal(
    runtime.completedResponseText(state, {
      text: "같은 문장",
      needsInput: false,
    }),
    "같은 문장\n\n같은 문장",
  );
});

test("Codex 답변 필요 표식은 최종 메시지 활동에서도 제거한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  const stream = Readable.from([
    `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "message-1",
        type: "agent_message",
        text: "[NEED_INPUT]\n어느 색으로 할까요?",
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);
  await runtime.normalizeCompletedMessageActivity(state, {
    text: "어느 색으로 할까요?",
    needsInput: true,
  });

  assert.equal(
    state.activityRecords.get("message:message-1").text,
    "어느 색으로 할까요?",
  );
  assert.equal(
    state.visibleAgentMessages.at(-1).text,
    "어느 색으로 할까요?",
  );
  assert.equal(state.responseText, "어느 색으로 할까요?");
  const activityUpdates = queries.filter(({ text }) =>
    /UPDATE turn_activities/.test(text)
  );
  assert.equal(activityUpdates.length, 1);
  assert.doesNotMatch(activityUpdates[0].text, /occurred_at\s*=/);
});

test("Claude 최종 메시지 활동도 기계 블록을 뗀 본문으로 맞춘다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.character = { id: "left-woman", backend: "claude" };
  const rawMessage = "정리했습니다.\n\n[OFFICE_SOURCES]\n"
    + '[{"kind":"file","title":"설정","locator":"a.mjs:1","excerpt":"확인"}]';
  const stream = Readable.from([
    `${JSON.stringify({
      type: "assistant",
      message: {
        id: "msg-1",
        content: [{ type: "text", text: rawMessage }],
      },
    })}\n`,
    `${JSON.stringify({
      type: "assistant",
      message: {
        id: "msg-2",
        content: [
          { type: "tool_use", id: "tool-1", name: "Read", input: {} },
        ],
      },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);
  assert.equal(
    state.activityRecords.get("message:msg-1").text,
    rawMessage,
    "정리 전에는 기계 블록이 붙은 원문이 활동으로 남는다",
  );

  await runtime.normalizeCompletedMessageActivity(state, {
    text: "정리했습니다.",
    needsInput: false,
  });

  // 활동과 응답이 같아야 화면이 같은 답을 두 번 그리지 않는다.
  assert.equal(
    state.activityRecords.get("message:msg-1").text,
    "정리했습니다.",
  );
  assert.equal(state.visibleAgentMessages.at(-1).text, "정리했습니다.");
});

test("Antigravity 도구 사이의 중간 메시지는 발생한 자리에 message 활동으로 남는다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.character = { id: "left-woman", backend: "antigravity" };
  // 실제 CLI 스트림 순서다. 응답 단계는 ACTIVE 조각 뒤 DONE 조각으로 끝난다.
  const step = (index, extra) => `${JSON.stringify({
    event: "step_update",
    step_update: { conversation_id: "conv-1", step_index: index, ...extra },
  })}\n`;
  const stream = Readable.from([
    step(1, { state: "ACTIVE", step_type: "agent_response", text_delta: "첫 번째 메시지입니다." }),
    step(1, { state: "DONE", step_type: "agent_response", text_delta: "\n" }),
    step(2, { state: "ACTIVE", step_type: "tool", tool_name: "run_command", tool_info: { name: "run_command", parameters: { CommandLine: "echo hi" } } }),
    step(2, { state: "DONE", step_type: "tool", tool_name: "run_command", tool_info: { name: "run_command", parameters: { CommandLine: "echo hi" } } }),
    step(3, { state: "DONE", step_type: "agent_response", text_delta: "두 번째 메시지입니다.\n" }),
    step(4, { state: "DONE", step_type: "tool", tool_name: "run_command", tool_info: { name: "run_command", parameters: { CommandLine: "echo bye" } } }),
    step(5, { state: "DONE", step_type: "agent_response", text_delta: "끝났습니다.\n" }),
    // 실제 기록(5109)처럼 조각 없이 사용량만 있는 빈 단계가 뒤따르기도 한다.
    step(6, { state: "DONE", step_type: "agent_response", usage: { input_tokens: 10, output_tokens: 1 } }),
    `${JSON.stringify({
      event: "result",
      result: { conversation_id: "conv-1", status: "SUCCESS", response: "첫 번째 메시지입니다.\n두 번째 메시지입니다.\n끝났습니다.\n" },
    })}\n`,
  ]);

  await runtime.consumeOutput(state, stream);

  const inserted = queries
    .filter(({ text }) => /INSERT INTO turn_activities/.test(text))
    .map(({ values }) => [values[2], values[3]]);
  // 끝의 줄바꿈을 뗀 문장이어야 화면이 응답 초안에서 같은 문장을 걷어낸다.
  assert.deepEqual(inserted, [
    ["message", "첫 번째 메시지입니다."],
    ["command", "echo hi"],
    ["message", "두 번째 메시지입니다."],
    ["command", "echo bye"],
  ]);
  // 마지막 메시지는 활동으로 승격하지 않고 최종 응답으로 남는다.
  // 빈 단계가 앞 문장을 한 번 더 기록하지 않는다.
  assert.equal(state.pendingAgentMessage.text, "끝났습니다.");
  assert.equal(state.pendingAgentMessage.key, "antigravity:conv-1:5");
  assert.equal(state.responseText, "끝났습니다.");
  // 결과 이벤트의 이어 붙인 전문은 단계 메시지를 받았으면 쓰지 않는다.
  assert.deepEqual(
    state.visibleAgentMessages.map((message) => message.text),
    ["첫 번째 메시지입니다.", "두 번째 메시지입니다.", "끝났습니다."],
  );
});

test("Codex 파일 변경 통계는 현재 턴 rollout의 실제 patch diff로 계산한다", async () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-file-change-test-"));
  const filePath = join(workdir, "Feed.swift");
  const rolloutPath = join(workdir, "rollout.jsonl");
  writeFileSync(filePath, "old line\nkept line\n");
  writeFileSync(rolloutPath, "");
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir,
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.workdir = workdir;
  state.rolloutReader = {
    path: rolloutPath,
    offset: 0,
    remainder: "",
    pending: [],
  };

  async function* events() {
    yield `${JSON.stringify({
      type: "item.started",
      item: {
        id: "files-1",
        type: "file_change",
        changes: [{ kind: "update", path: "Feed.swift" }],
        status: "in_progress",
      },
    })}\n`;
    writeFileSync(filePath, "new line\nanother line\nkept line\n");
    appendFileSync(rolloutPath, `${JSON.stringify({
      type: "event_msg",
      payload: {
        type: "patch_apply_end",
        call_id: "call-1",
        status: "completed",
        success: true,
        changes: {
          [filePath]: {
            type: "update",
            move_path: null,
            unified_diff: [
              "--- a/Feed.swift",
              "+++ b/Feed.swift",
              "@@ -1,2 +1,3 @@",
              "-old line",
              "+new line",
              "+another line",
              " kept line",
            ].join("\n"),
          },
        },
      },
    })}\n`);
    yield `${JSON.stringify({
      type: "item.completed",
      item: {
        id: "files-1",
        type: "file_change",
        changes: [{ kind: "update", path: "Feed.swift" }],
        status: "completed",
      },
    })}\n`;
  }

  try {
    await runtime.consumeOutput(state, Readable.from(events()));
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }

  assert.equal(
    state.activityRecords.get("files-1").text,
    [
      "파일 1개를 편집했습니다",
      "+2 -1",
      "수정 Feed.swift",
    ].join("\n"),
  );
  assert.equal(state.activityRecords.get("files-1").status, "completed");
});

test("Codex rollout 협업 활동은 부분 행을 보존하고 같은 DB 행을 완료한다", async () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-collab-rollout-test-"));
  const rolloutPath = join(workdir, "rollout.jsonl");
  writeFileSync(rolloutPath, "");
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir,
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.workdir = workdir;
  state.rolloutReader = {
    path: rolloutPath,
    offset: 0,
    remainder: "",
    pending: [],
  };
  state.rolloutPollPromise = null;

  const spawnCall = JSON.stringify({
    type: "response_item",
    payload: {
      type: "function_call",
      name: "spawn_agent",
      namespace: "collaboration",
      call_id: "call-runtime-spawn",
      arguments: JSON.stringify({
        task_name: "runtime_review",
        message: "gAAAAABencrypted",
      }),
    },
  });
  const started = JSON.stringify({
    type: "event_msg",
    payload: {
      type: "sub_agent_activity",
      event_id: "call-runtime-spawn",
      agent_thread_id: "thread-runtime-review",
      agent_path: "/root/runtime_review",
      kind: "started",
    },
  });
  const finalAnswer = JSON.stringify({
    type: "response_item",
    payload: {
      type: "agent_message",
      author: "/root/runtime_review",
      recipient: "/root",
      content: [{
        type: "input_text",
        text: [
          "Message Type: FINAL_ANSWER",
          "Task name: /root",
          "Sender: /root/runtime_review",
          "Payload:",
          "런타임 연결 검토 완료",
        ].join("\n"),
      }],
    },
  });

  appendFileSync(
    rolloutPath,
    `${spawnCall}\n${started.slice(0, Math.floor(started.length / 2))}`,
  );
  await runtime.consumeCodexRolloutActivities(state);
  assert.equal(state.activityRecords.size, 0);

  appendFileSync(
    rolloutPath,
    `${started.slice(Math.floor(started.length / 2))}\n`,
  );
  await runtime.consumeCodexRolloutActivities(state);

  let inserts = queries.filter(({ text }) =>
    /INSERT INTO turn_activities/.test(text)
  );
  let updates = queries.filter(({ text }) =>
    /UPDATE turn_activities/.test(text)
  );
  assert.equal(inserts.length, 1);
  assert.equal(updates.length, 0);
  assert.equal(
    state.activityRecords.get(
      "collaboration:rollout:thread-runtime-review",
    ).status,
    "running",
  );

  appendFileSync(rolloutPath, `${finalAnswer}\n`);
  await runtime.consumeCodexRolloutActivities(state);

  inserts = queries.filter(({ text }) =>
    /INSERT INTO turn_activities/.test(text)
  );
  updates = queries.filter(({ text }) =>
    /UPDATE turn_activities/.test(text)
  );
  assert.equal(inserts.length, 1);
  assert.equal(updates.length, 1);
  assert.equal(state.activityRecords.size, 1);
  assert.deepEqual(
    state.activityRecords.get(
      "collaboration:rollout:thread-runtime-review",
    ),
    {
      sequence: 1,
      kind: "collaboration",
      text: "런타임 연결 검토 완료",
      status: "completed",
      collaboration: {
        action: "result",
        agentThreadId: "thread-runtime-review",
        agentLabel: "runtime review",
        prompt: "runtime review",
        message: "런타임 연결 검토 완료",
        agentStatus: "completed",
      },
    },
  );
  assert.doesNotMatch(JSON.stringify(queries), /gAAAAABencrypted/);

  rmSync(workdir, { recursive: true, force: true });
});

test("Codex rollout 협업 저장이 일시 실패하면 다음 poll에서 재시도한다", async () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-collab-retry-test-"));
  const rolloutPath = join(workdir, "rollout.jsonl");
  const records = [
    {
      type: "response_item",
      payload: {
        type: "function_call",
        name: "spawn_agent",
        namespace: "collaboration",
        call_id: "call-retry-spawn",
        arguments: JSON.stringify({ task_name: "retry_review" }),
      },
    },
    {
      type: "event_msg",
      payload: {
        type: "sub_agent_activity",
        event_id: "call-retry-spawn",
        agent_thread_id: "thread-retry-review",
        agent_path: "/root/retry_review",
        kind: "started",
      },
    },
  ];
  writeFileSync(
    rolloutPath,
    `${records.map((record) => JSON.stringify(record)).join("\n")}\n`,
  );
  let shouldFail = true;
  let insertAttempts = 0;
  const runtime = new AgentRuntime({
    pool: {
      query: async (text) => {
        if (/INSERT INTO turn_activities/.test(text)) {
          insertAttempts += 1;
          if (shouldFail) {
            shouldFail = false;
            throw new Error("temporary database error");
          }
        }
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir,
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.workdir = workdir;
  state.rolloutReader = {
    path: rolloutPath,
    offset: 0,
    remainder: "",
    pending: [],
  };
  state.rolloutPollPromise = null;

  try {
    await assert.rejects(
      runtime.consumeCodexRolloutActivities(state),
      /temporary database error/,
    );
    assert.equal(state.rolloutReader.collaborationPending.length, 1);

    await runtime.consumeCodexRolloutActivities(state);
    assert.equal(insertAttempts, 2);
    assert.equal(state.rolloutReader.collaborationPending.length, 0);
    assert.equal(
      state.activityRecords.get(
        "collaboration:rollout:thread-retry-review",
      ).status,
      "running",
    );
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
});

test("새 Codex 세션의 rollout 파일이 늦게 생겨도 다음 poll에서 재부착한다", async () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-collab-attach-test-"));
  const rolloutPath = join(workdir, "rollout.jsonl");
  writeFileSync(
    rolloutPath,
    [
      JSON.stringify({
        type: "response_item",
        payload: {
          type: "function_call",
          name: "spawn_agent",
          namespace: "collaboration",
          call_id: "call-late-spawn",
          arguments: JSON.stringify({ task_name: "late_review" }),
        },
      }),
      JSON.stringify({
        type: "event_msg",
        payload: {
          type: "sub_agent_activity",
          event_id: "call-late-spawn",
          agent_thread_id: "thread-late-review",
          agent_path: "/root/late_review",
          kind: "started",
        },
      }),
      "",
    ].join("\n"),
  );
  const readerStartModes = [];
  let factoryCalls = 0;
  const runtime = new AgentRuntime({
    pool: {
      query: async () => ({ rowCount: 1 }),
    },
    withTransaction: async () => {},
    workdir,
    broadcast: () => {},
    rolloutReaderFactory: (_sessionID, startAtEnd) => {
      factoryCalls += 1;
      readerStartModes.push(startAtEnd);
      if (factoryCalls === 1) {
        return null;
      }
      return {
        path: rolloutPath,
        offset: startAtEnd ? readFileSync(rolloutPath).length : 0,
        remainder: "",
        pending: [],
      };
    },
  });
  const state = makeCodexActivityState();
  state.workdir = workdir;
  state.rolloutReader = null;
  state.rolloutPollPromise = null;
  state.resumedCodexSession = false;

  try {
    await runtime.consumeCodexRolloutActivities(state);
    assert.equal(state.activityRecords.size, 0);
    await runtime.consumeCodexRolloutActivities(state);

    assert.equal(factoryCalls, 2);
    assert.deepEqual(readerStartModes, [false, false]);
    assert.equal(
      state.activityRecords.get(
        "collaboration:rollout:thread-late-review",
      ).status,
      "running",
    );
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
});

test("업무 프롬프트에 보관된 첨부 경로를 기록한다", () => {
  const prompt = promptWithAttachments("분석해줘.", [
    {
      name: "report.pdf",
      path: "/workspace/.office-attachments/01-report.pdf",
    },
  ]);

  assert.match(prompt, /첨부 파일/);
  assert.match(prompt, /report\.pdf/);
  assert.match(prompt, /\.office-attachments/);
});

test("신규 업무는 worktree 없이 공유 프로젝트에서 실행된다", async () => {
  let beginCount = 0;
  let executeCount = 0;
  let failCount = 0;
  const queries = [];
  const executionGate = deferred();
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 0, rows: [] };
      },
    },
    withTransaction: async () => {},
    workdir: "/repo",
    broadcast: () => {},
  });
  runtime.prepareTurn = async ({ prompt, conversationID }) => ({
    turnID: "turn-1",
    sessionID: "session-1",
    conversationID,
    externalSessionID: null,
    character: { id: "boss", backend: "codex" },
    prompt,
  });
  runtime.ensureWorkspace = async () => {
    assert.fail("신규 업무에서 worktree를 준비하면 안 됩니다.");
  };
  runtime.beginPreparedTurn = async () => {
    beginCount += 1;
  };
  runtime.failPreparedTurn = async () => {
    failCount += 1;
  };
  runtime.execute = async () => {
    executeCount += 1;
    await executionGate.promise;
  };

  const result = await runtime.start({
    characterID: "boss",
    prompt: "세션 유지 상태를 확인해줘.",
    conversationID: "11111111-1111-1111-1111-111111111111",
  });
  const state = runtime.running.get("boss");
  assert.equal(result.status, "running");
  assert.equal(beginCount, 1);
  assert.equal(executeCount, 1);
  assert.equal(failCount, 0);
  assert.equal(state.workspace, null);
  assert.equal(state.workdir, "/repo");
  assert.equal(state.executionPrompt, state.recordPrompt);
  assert.doesNotMatch(state.executionPrompt, /office_retrieved_records/);
  assert.equal(
    queries.some(({ text }) =>
      /searchable_rag_documents/.test(String(text ?? ""))
    ),
    false,
    "공유 폴더 업무 시작도 RAG 문서를 검색하면 안 됩니다.",
  );
  executionGate.resolve();
});

test("첨부 참조는 작업 기록에 남고 검색 JSON은 어디에도 섞이지 않는다", async () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-record-attachment-"));
  const source = join(workdir, "report.pdf");
  writeFileSync(source, "attachment body");
  const executionGate = deferred();
  const queries = [];
  let storedPrompt = null;
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM searchable_rag_documents AS document/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          ragDocumentId: "rag-1",
          workRecordId: "record-1",
          title: "이전 기록",
          excerpt: "이전 결과",
        }],
      };
    }
    if (/WITH selected_project AS/.test(text)) {
      return {
        rowCount: 1,
        rows: [{ workRecordId: "new-record-1" }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir,
    repositoryRoot: "/repo-root",
    broadcast: () => {},
  });
  runtime.prepareTurn = async ({ prompt, conversationID }) => ({
    turnID: "turn-1",
    sessionID: "session-1",
    conversationID,
    externalSessionID: null,
    character: {
      id: "boss",
      backend: "codex",
      model: "gpt-5.6-sol",
      effort: "high",
      fastMode: false,
      permission: "workspace-write",
      name: "백부장",
      seat: "상단",
      identityPrompt: "업무를 처리한다.",
    },
    prompt,
  });
  runtime.ensureWorkspace = async () => {
    assert.fail("첨부 업무도 worktree를 준비하면 안 됩니다.");
  };
  runtime.beginPreparedTurn = async (_turnID, prompt) => {
    storedPrompt = prompt;
  };
  runtime.execute = async () => await executionGate.promise;

  try {
    await runtime.start({
      characterID: "boss",
      prompt: "첨부를 분석해줘.",
      conversationID: "11111111-1111-1111-1111-111111111111",
      attachmentPaths: [source],
    });
    const state = runtime.running.get("boss");
    assert.match(storedPrompt, /report\.pdf/);
    assert.match(storedPrompt, /\.office-attachments/);
    assert.equal(state.recordPrompt, storedPrompt);
    assert.doesNotMatch(state.recordPrompt, /rag-1|office_retrieved_records/);
    assert.match(state.executionPrompt, /report\.pdf/);
    assert.doesNotMatch(
      state.executionPrompt,
      /rag-1|ragDocumentId|office_retrieved_records/,
    );
    assert.equal(state.executionPrompt, state.recordPrompt);

    await runtime.complete(state, {
      text: "첨부 분석을 완료했습니다.",
      needsInput: false,
    });
    const workRecordInsert = queries.find(({ text }) =>
      /WITH selected_project AS/.test(text)
    );
    assert.equal(workRecordInsert.values[0], "/repo-root");
    assert.match(workRecordInsert.values[6], /report\.pdf/);
    assert.match(workRecordInsert.values[6], /\.office-attachments/);
    assert.doesNotMatch(workRecordInsert.values[6], /rag-1/);
    executionGate.resolve();
  } finally {
    executionGate.resolve();
    rmSync(workdir, { recursive: true, force: true });
  }
});

test("첨부 원본을 작업 폴더에 보관한다", () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-attachment-test-"));
  const source = join(workdir, "source.txt");
  writeFileSync(source, "attachment body");

  try {
    const [attachment] = stageAttachments({
      attachmentPaths: [source],
      workdir,
    });

    assert.equal(existsSync(attachment.path), true);
    assert.equal(readFileSync(attachment.path, "utf8"), "attachment body");
    assert.match(attachment.path, /\.office-attachments/);
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
});

test("Antigravity 첨부도 공통 작업 폴더에 두고 add-dir로 읽는다", () => {
  const workdir = mkdtempSync(join(tmpdir(), "office-agy-attachment-test-"));
  const source = join(workdir, "screen.png");
  writeFileSync(source, "png");

  try {
    const [attachment] = stageAttachments({
      attachmentPaths: [source],
      workdir,
    });

    assert.equal(existsSync(attachment.path), true);
    assert.match(attachment.path, /\.office-attachments/);
    assert.equal(attachment.isCodexImage, true);
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
});

test("실행 중단은 턴을 interrupted로 저장하고 실행 목록에서 제거한다", async () => {
  const queries = [];
  const broadcasts = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: (event) => broadcasts.push(event),
  });
  runtime.running.set("boss", {
    turnID: "turn-1",
    process: null,
    cancelRequested: false,
  });

  const result = await runtime.cancel("boss");

  assert.deepEqual(result, {
    turnId: "turn-1",
    status: "interrupted",
  });
  assert.equal(runtime.running.has("boss"), false);
  const turnUpdate = queries.find(({ text }) =>
    /status = 'interrupted'/.test(text)
  );
  assert.deepEqual(turnUpdate.values, [
    "turn-1",
    "사용자가 업무를 중단했습니다.",
  ]);
  assert.equal(broadcasts.length, 1);
});

test("Claude 실행 중단은 워커가 받은 중단 result의 비용을 사용량 기록으로 저장한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    turnID: "turn-1",
    character: { id: "boss", backend: "claude", model: "claude-opus-5" },
    process: null,
    cancelRequested: false,
    externalSessionID: null,
    claudeWorker: {
      // 실제 워커는 interrupt 제어 요청 뒤 받은 result로 사용량을 채운다.
      cancelCurrent: async () => {
        state.usage = {
          inputTokens: 1_218,
          outputTokens: 27,
          cachedInputTokens: 0,
          reasoningOutputTokens: null,
          cacheWriteInputTokens: 0,
          cacheWrite5mInputTokens: null,
          cacheWrite1hInputTokens: null,
          reportedCostUsd: 0.002706,
        };
      },
    },
  };
  runtime.running.set("boss", state);

  await runtime.cancel("boss");

  const usageInsert = queries.find(({ text }) =>
    /INSERT INTO usage_records/.test(text)
  );
  assert.deepEqual(usageInsert.values, [
    "turn-1",
    1_218,
    27,
    0,
    null,
    0.002706,
    0,
    null,
    null,
  ]);
});

test("실행 중단은 남은 실행 중 활동도 실패 상태로 닫는다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    character: { id: "boss", backend: "codex" },
    process: null,
    cancelRequested: false,
    sequence: 1,
    activityRecords: new Map([["command-1", {
      sequence: 1,
      kind: "command",
      text: "swift test",
      status: "running",
    }]]),
  };
  runtime.running.set("boss", state);

  await runtime.cancel("boss");

  const activityUpdate = queries.find(({ text }) =>
    /UPDATE turn_activities/.test(text)
  );
  assert.equal(activityUpdate.values[1], "failed");
  assert.equal(state.activityRecords.get("command-1").status, "failed");
});

test("실패한 턴은 남은 실행 중 활동도 실패 상태로 닫는다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    return { rowCount: 1 };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    character: { id: "boss", backend: "codex" },
    sequence: 1,
    activityRecords: new Map([["command-1", {
      sequence: 1,
      kind: "command",
      text: "swift test",
      status: "running",
    }]]),
  };
  runtime.running.set("boss", state);

  await runtime.fail(state, new Error("실패"));

  const activityUpdate = queries.find(({ text }) =>
    /UPDATE turn_activities/.test(text)
  );
  assert.equal(activityUpdate.values[1], "failed");
  assert.equal(state.activityRecords.get("command-1").status, "failed");
});

test("이전 턴의 늦은 실패는 같은 직원의 새 턴을 제거하지 않는다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    return { rowCount: 1 };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/tmp",
    broadcast: () => {},
  });
  const oldState = {
    ...makeCodexActivityState(),
    turnID: "old-turn",
    character: { id: "boss", backend: "codex" },
  };
  const newState = {
    ...makeCodexActivityState(),
    turnID: "new-turn",
    character: { id: "boss", backend: "codex" },
  };
  runtime.running.set("boss", newState);

  await runtime.fail(oldState, new Error("늦은 실패"));

  assert.equal(runtime.running.get("boss"), newState);
  assert.equal(queries.length, 0);
});

test("백엔드 복구는 중단된 턴의 실행 중 활동도 닫는다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text) => {
        queries.push(text);
        return { rowCount: 2 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });

  const count = await runtime.recoverInterruptedJobs();

  assert.equal(count, 2);
  assert.match(queries[0], /WHERE status IN \('pending', 'running'\)/);
  assert.match(queries[0], /UPDATE turn_activities AS activity/);
  assert.match(queries[0], /activity\.status = 'running'/);
  assert.match(queries[0], /existing_terminal_turns/);
  assert.match(queries[0], /turn\.status = 'completed'/);
  assert.match(queries[0], /THEN 'completed'/);
});

test("백엔드 복구는 중단된 provisioning 기록을 Git 정리한 뒤 실패로 닫는다", async () => {
  const events = [];
  const row = workspaceDatabaseRow({
    status: "provisioning",
    review_turn_id: null,
    review_tree: null,
    changed_files: [],
  });
  const query = async (text) => {
    if (
      /SELECT \*\s+FROM task_workspaces\s+WHERE status = 'provisioning'/.test(
        text,
      )
    ) {
      events.push("db:read-provisioning");
      return { rowCount: 1, rows: [row] };
    }
    if (/WHERE status IN \('provisioning', 'merging'\)/.test(text)) {
      events.push("db:mark-failed");
      return { rowCount: 1, rows: [] };
    }
    return { rowCount: 0, rows: [] };
  };
  const cleaned = [];
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async () => {},
    workdir: "/repo",
    workspaceManager: {
      cleanupProvisioning: async (workspace) => {
        events.push("git:cleanup-provisioning");
        cleaned.push(workspace);
      },
      cleanup: async () => {
        assert.fail("provisioning 복구는 일반 cleanup을 사용하면 안 됩니다.");
      },
    },
    broadcast: () => {},
  });

  await runtime.recoverInterruptedJobs();

  assert.equal(cleaned.length, 1);
  assert.equal(cleaned[0].cliSessionID, "session-1");
  assert.equal(cleaned[0].status, "provisioning");
  assert.deepEqual(events, [
    "db:read-provisioning",
    "git:cleanup-provisioning",
    "db:mark-failed",
  ]);
});

test("백엔드 복구는 유효하지 않은 active workspace만 실패 처리하고 세션은 유지한다", async () => {
  const events = [];
  const queries = [];
  const row = workspaceDatabaseRow({
    status: "active",
    review_turn_id: null,
    review_tree: null,
    changed_files: [],
  });
  const query = async (text, values) => {
    queries.push({ text, values });
    if (
      /SELECT \*\s+FROM task_workspaces\s+WHERE status = 'active'/.test(text)
    ) {
      events.push("db:read-active");
      return { rowCount: 1, rows: [row] };
    }
    if (
      /UPDATE task_workspaces/.test(text) &&
      /SET status = 'failed'/.test(text) &&
      /AND status = 'active'/.test(text)
    ) {
      events.push("db:workspace-failed");
      return { rowCount: 1, rows: [] };
    }
    if (/DELETE FROM active_cli_sessions/.test(text)) {
      events.push("db:delete-active-session");
      return { rowCount: 1, rows: [] };
    }
    if (/UPDATE cli_sessions/.test(text) && /ended_at/.test(text)) {
      events.push("db:end-cli-session");
      return { rowCount: 1, rows: [] };
    }
    return { rowCount: 0, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => {
      events.push("db:transaction");
      return body({ query });
    },
    workdir: "/repo",
    workspaceManager: {
      validateWorkspace: async (workspace) => {
        events.push("git:validate-active");
        assert.equal(workspace.cliSessionID, "session-1");
        assert.equal(workspace.status, "active");
        throw new Error("worktree missing");
      },
    },
    broadcast: () => {},
  });

  const count = await runtime.recoverInterruptedJobs();

  assert.equal(count, 0);
  assert.deepEqual(events, [
    "db:read-active",
    "git:validate-active",
    "db:transaction",
    "db:workspace-failed",
  ]);
  const failed = queries.find(({ text }) =>
    /SET status = 'failed'/.test(text) && /AND status = 'active'/.test(text)
  );
  assert.ok(failed);
  assert.deepEqual(failed.values, [
    "workspace-1",
    "활성 작업 공간을 복구할 수 없습니다. worktree missing",
  ]);
  assert.equal(
    queries.some(({ text, values }) =>
      /DELETE FROM active_cli_sessions/.test(text) &&
      values?.[0] === "session-1"
    ),
    false,
  );
  assert.equal(
    queries.some(({ text, values }) =>
      /UPDATE cli_sessions/.test(text) &&
      /ended_at/.test(text) &&
      values?.[0] === "session-1"
    ),
    false,
  );
});

test("중단 중 늦게 저장된 keyless 활동도 출력 종료 뒤 닫는다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    character: { id: "boss", backend: "codex" },
    cancelRequested: true,
  };
  await runtime.addActivity(state, {
    kind: "command",
    text: "buffered command",
    status: "running",
  });

  const settled = await runtime.settleCancelledOutput(state);

  assert.equal(settled, true);
  const closingUpdate = queries.findLast(({ text }) =>
    /UPDATE turn_activities/.test(text) && /status = \$2/.test(text)
  );
  assert.deepEqual(closingUpdate.values, ["turn-1", "failed", "errored"]);
});

test("실행 중인 업무가 없으면 중단 요청을 거절한다", async () => {
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });

  await assert.rejects(
    runtime.cancel("boss"),
    AgentJobNotFoundError,
  );
});

test("Codex rollout 끝에서 직전 누적 사용량을 찾는다", () => {
  const directory = mkdtempSync(join(tmpdir(), "office-usage-"));
  const path = join(directory, "rollout.jsonl");
  try {
    writeFileSync(path, [
      JSON.stringify({
        payload: {
          type: "token_count",
          info: {
            total_token_usage: {
              input_tokens: 124_509_396,
              cached_input_tokens: 120_069_888,
              cache_write_input_tokens: 0,
              output_tokens: 458_293,
              reasoning_output_tokens: 226_498,
            },
          },
        },
      }),
      JSON.stringify({
        payload: {
          type: "agent_message",
          message: "x".repeat(70_000),
        },
      }),
      "",
    ].join("\n"));

    assert.deepEqual(latestCodexUsageFromRollout(path), {
      inputTokens: 124_509_396,
      outputTokens: 458_293,
      cachedInputTokens: 120_069_888,
      cacheWriteInputTokens: 0,
      cacheWrite5mInputTokens: null,
      cacheWrite1hInputTokens: null,
      reasoningOutputTokens: 226_498,
      serviceTier: null,
      speed: null,
      inferenceGeo: null,
      reportedCostUsd: null,
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("Claude 세션 기록 끝에서 마지막 사용량을 읽는다", () => {
  const directory = mkdtempSync(join(tmpdir(), "office-claude-usage-"));
  try {
    const path = join(directory, "session.jsonl");
    writeFileSync(path, [
      JSON.stringify({
        type: "assistant",
        message: { usage: { input_tokens: 1, output_tokens: 2 } },
      }),
      JSON.stringify({
        type: "assistant",
        message: {
          usage: {
            input_tokens: 11,
            output_tokens: 826,
            cache_read_input_tokens: 635_024,
            cache_creation_input_tokens: 888,
            cache_creation: {
              ephemeral_5m_input_tokens: 0,
              ephemeral_1h_input_tokens: 888,
            },
            service_tier: "standard",
          },
        },
      }),
      JSON.stringify({ type: "user", message: { content: [] } }),
      "",
    ].join("\n"));

    const usage = latestClaudeUsageFromSession(path);

    assert.equal(usage.inputTokens, 11);
    assert.equal(usage.outputTokens, 826);
    assert.equal(usage.cachedInputTokens, 635_024);
    assert.equal(usage.cacheWriteInputTokens, 888);
    assert.equal(usage.cacheWrite1hInputTokens, 888);
    assert.equal(usage.serviceTier, "standard");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("중단된 Claude 턴의 사용량을 세션 기록에서 되살린다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const previousWorkdir = mkdtempSync(
      join(tmpdir(), "office-claude-previous-"),
    );
    try {
      const path = writeClaudeSession(previousWorkdir, "session-1");
      writeFileSync(path, `${JSON.stringify({
        type: "assistant",
        message: { usage: { input_tokens: 5, output_tokens: 7 } },
      })}\n`);
      const state = {
        character: { backend: "claude", model: "claude-sonnet-5" },
        workdir,
        externalSessionID: "session-1",
      };

      const usage = recoverInterruptedUsage(state);

      assert.equal(usage.inputTokens, 5);
      assert.equal(state.usage.outputTokens, 7);
      assert.equal(claudeSessionResumable(workdir, "session-1"), false);
    } finally {
      rmSync(previousWorkdir, { recursive: true, force: true });
    }
  });
});

test("이미 사용량이 있는 턴은 세션 기록으로 덮어쓰지 않는다", () => {
  withClaudeSessionHome(({ workdir }) => {
    const path = writeClaudeSession(workdir, "session-1");
    writeFileSync(path, `${JSON.stringify({
      type: "assistant",
      message: { usage: { input_tokens: 5, output_tokens: 7 } },
    })}\n`);
    const existing = { inputTokens: 99, outputTokens: 99 };
    const state = {
      character: { backend: "claude", model: "claude-sonnet-5" },
      workdir,
      externalSessionID: "session-1",
      usage: existing,
    };

    assert.equal(recoverInterruptedUsage(state), null);
    assert.equal(state.usage, existing);
  });
});

test("중단된 Antigravity 재개 턴은 로컬 누적값에서 현재 턴만 복구한다", () => {
  const baseline = {
    inputTokens: 1_000,
    outputTokens: 100,
    cachedInputTokens: 5_000,
    reasoningOutputTokens: 50,
  };
  const recorded = {
    inputTokens: 1_200,
    outputTokens: 130,
    cachedInputTokens: 5_900,
    reasoningOutputTokens: 62,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
  const state = {
    character: { backend: "antigravity", model: "gemini-3.7-flash" },
    externalSessionID: "11111111-2222-4333-8444-555555555555",
    resumedAntigravitySession: true,
    usageBaseline: baseline,
    antigravityUsageReader: () => recorded,
    usage: null,
  };

  const usage = recoverInterruptedUsage(state);

  assert.equal(usage.inputTokens, 200);
  assert.equal(usage.outputTokens, 30);
  assert.equal(usage.cachedInputTokens, 900);
  assert.equal(usage.reasoningOutputTokens, 12);
  assert.deepEqual(state.usage, usage);
});

test("Codex 재개 세션의 누적 사용량을 현재 턴 증분으로 바꾼다", () => {
  const baseline = {
    inputTokens: 124_509_396,
    outputTokens: 458_293,
    cachedInputTokens: 120_069_888,
    cacheWriteInputTokens: 0,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: 226_498,
  };
  const usage = {
    inputTokens: 124_946_225,
    outputTokens: 459_261,
    cachedInputTokens: 120_500_480,
    cacheWriteInputTokens: 0,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: 226_746,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };

  assert.deepEqual(codexUsageDelta(usage, baseline), {
    ...usage,
    inputTokens: 436_829,
    outputTokens: 968,
    cachedInputTokens: 430_592,
    cacheWriteInputTokens: 0,
    reasoningOutputTokens: 248,
  });
});

test("Codex가 이미 턴 사용량을 보고하면 누적값으로 차감하지 않는다", () => {
  assert.equal(codexUsageDelta(
    {
      inputTokens: 10_000,
      outputTokens: 500,
      cachedInputTokens: 9_000,
    },
    {
      inputTokens: 100_000,
      outputTokens: 5_000,
      cachedInputTokens: 90_000,
    },
  ), null);
});

test("Codex 완료 이벤트는 재개 시점 기준 증분만 상태에 남긴다", async () => {
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 1 }) },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = makeCodexActivityState();
  state.resumedCodexSession = true;
  state.usageBaseline = {
    inputTokens: 124_509_396,
    outputTokens: 458_293,
    cachedInputTokens: 120_069_888,
    cacheWriteInputTokens: 0,
    reasoningOutputTokens: 226_498,
  };
  state.usage = null;

  await runtime.consumeOutput(state, Readable.from([
    `${JSON.stringify({
      type: "turn.completed",
      usage: {
        input_tokens: 124_946_225,
        output_tokens: 459_261,
        cached_input_tokens: 120_500_480,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 226_746,
      },
    })}\n`,
  ]));

  assert.equal(state.usage.inputTokens, 436_829);
  assert.equal(state.usage.cachedInputTokens, 430_592);
  assert.equal(state.usage.outputTokens, 968);
  assert.equal(state.usage.reasoningOutputTokens, 248);
});

test("완료 사용량과 추정 비용을 같은 턴의 사용량 기록으로 저장한다", async () => {
  const queries = [];
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return { rowCount: 1 };
      },
    },
    withTransaction: async () => {},
    workdir: "/tmp",
    broadcast: () => {},
  });
  const state = {
    turnID: "turn-1",
    character: {
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: true,
    },
    usage: {
      inputTokens: 1_000,
      outputTokens: 50,
      cachedInputTokens: 200,
      reasoningOutputTokens: 10,
      cacheWriteInputTokens: 100,
      cacheWrite5mInputTokens: null,
      cacheWrite1hInputTokens: null,
    },
  };

  await runtime.persistUsageRecord(runtime.pool, state);

  assert.equal(queries.length, 1);
  assert.match(queries[0].text, /INSERT INTO usage_records/);
  assert.deepEqual(queries[0].values, [
    "turn-1",
    1_000,
    50,
    200,
    10,
    0.00876,
    100,
    null,
    null,
  ]);
});

function workspaceDatabaseRow(overrides = {}) {
  return {
    id: "workspace-1",
    cli_session_id: "session-1",
    status: "awaiting_approval",
    repository_root: "/repo",
    source_workdir: "/repo",
    worktree_path: "/worktrees/workspace-1",
    execution_workdir: "/worktrees/workspace-1",
    branch_name: "officestra/boss/workspace-1",
    base_branch: "main",
    base_commit: "base-commit",
    review_turn_id: "turn-1",
    review_tree: "review-tree",
    head_commit: "head-commit",
    changed_files: [{ status: "M", path: "README.md" }],
    task_commit: null,
    merged_commit: null,
    error_message: null,
    created_at: new Date("2026-08-01T00:00:00Z"),
    updated_at: new Date("2026-08-01T00:00:00Z"),
    review_requested_at: new Date("2026-08-01T00:00:00Z"),
    merged_at: null,
    rejected_at: null,
    characterID: "boss",
    characterName: "보스",
    ...overrides,
  };
}

test("기존 CLI 세션도 새 Git 업무의 격리 작업 공간을 만든다", async () => {
  const queries = [];
  const provisioned = {
    repositoryRoot: "/repo",
    sourceWorkdir: "/repo/subdir",
    worktreePath: "/worktrees/workspace-1",
    executionWorkdir: "/worktrees/workspace-1/subdir",
    branchName: "officestra/boss/workspace-1",
    baseBranch: "main",
    baseCommit: "base-commit",
  };
  const workspaceManager = {
    provision: async (input) => {
      assert.deepEqual(input, {
        workspaceID: "workspace-1",
        characterID: "boss",
      });
      return provisioned;
    },
  };
  const runtime = new AgentRuntime({
    pool: {
      query: async (text, values) => {
        queries.push({ text, values });
        return {
          rowCount: 1,
          rows: [workspaceDatabaseRow({
            status: "active",
            source_workdir: provisioned.sourceWorkdir,
            execution_workdir: provisioned.executionWorkdir,
            review_turn_id: null,
            review_tree: null,
            changed_files: [],
          })],
        };
      },
    },
    withTransaction: async () => {},
    workdir: "/repo/subdir",
    workspaceManager,
    broadcast: () => {},
  });

  const workspace = await runtime.ensureWorkspace({
    turnID: "turn-1",
    sessionID: "session-1",
    workspaceID: "workspace-1",
    reusedSession: true,
    workspace: null,
    character: { id: "boss" },
  });

  assert.equal(workspace.executionWorkdir, provisioned.executionWorkdir);
  assert.match(queries[0].text, /INSERT INTO task_workspaces/);
  assert.deepEqual(queries[0].values.slice(2, -1), [
    provisioned.repositoryRoot,
    provisioned.sourceWorkdir,
    provisioned.worktreePath,
    provisioned.executionWorkdir,
    provisioned.branchName,
    provisioned.baseBranch,
    provisioned.baseCommit,
  ]);
});

test("새 Git 업무는 provisioning을 먼저 기록한 뒤 계획한 worktree를 활성화한다", async () => {
  const events = [];
  const plan = {
    repositoryRoot: "/repo",
    sourceWorkdir: "/repo/subdir",
    worktreePath: "/worktrees/workspace-1",
    executionWorkdir: "/worktrees/workspace-1/subdir",
    branchName: "officestra/boss/workspace-1",
    baseBranch: "main",
    baseCommit: "base-commit",
  };
  const activeRow = workspaceDatabaseRow({
    status: "active",
    source_workdir: plan.sourceWorkdir,
    execution_workdir: plan.executionWorkdir,
    review_turn_id: null,
    review_tree: null,
    changed_files: [],
  });
  const query = async (text, values) => {
    if (/INSERT INTO task_workspaces/.test(text)) {
      events.push("db:provisioning");
      assert.match(text, /'provisioning'/);
      assert.deepEqual(values.slice(2, -1), [
        plan.repositoryRoot,
        plan.sourceWorkdir,
        plan.worktreePath,
        plan.executionWorkdir,
        plan.branchName,
        plan.baseBranch,
        plan.baseCommit,
      ]);
      return { rowCount: 1, rows: [] };
    }
    if (
      /UPDATE task_workspaces/.test(text) &&
      /status = 'active'/.test(text)
    ) {
      events.push("db:active");
      assert.match(text, /AND status = 'provisioning'/);
      return { rowCount: 1, rows: [activeRow] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async () => {},
    workdir: "/repo/subdir",
    workspaceManager: {
      planProvision: async (input) => {
        events.push("git:plan");
        assert.deepEqual(input, {
          workspaceID: "workspace-1",
          characterID: "boss",
        });
        return plan;
      },
      provisionPlanned: async (receivedPlan) => {
        events.push("git:provision");
        assert.equal(receivedPlan, plan);
        return plan;
      },
      cleanup: async () => {
        assert.fail("정상 provisioning에서는 cleanup을 호출하면 안 됩니다.");
      },
    },
    broadcast: () => {},
  });

  const workspace = await runtime.ensureWorkspace({
    turnID: "turn-1",
    sessionID: "session-1",
    workspaceID: "workspace-1",
    reusedSession: false,
    isolateGitWorkdir: true,
    workspace: null,
    character: { id: "boss" },
  });

  assert.equal(workspace.status, "active");
  assert.equal(workspace.executionWorkdir, plan.executionWorkdir);
  assert.deepEqual(events, [
    "git:plan",
    "db:provisioning",
    "git:provision",
    "db:active",
  ]);
});

test("동시 workspace 준비 경합은 PostgreSQL 제약 문구 대신 재시도 안내를 반환한다", async () => {
  const plan = {
    repositoryRoot: "/repo",
    sourceWorkdir: "/repo",
    worktreePath: "/worktrees/workspace-2",
    executionWorkdir: "/worktrees/workspace-2",
    branchName: "officestra/boss/workspace-2",
    baseBranch: "main",
    baseCommit: "base-commit",
  };
  let provisionCalled = false;
  const runtime = new AgentRuntime({
    pool: {
      query: async () => {
        const error = new Error("duplicate key value violates unique constraint");
        error.code = "23505";
        error.constraint = "task_workspaces_one_open_per_session_idx";
        throw error;
      },
    },
    withTransaction: async () => {},
    workdir: "/repo",
    workspaceManager: {
      planProvision: async () => plan,
      provisionPlanned: async () => {
        provisionCalled = true;
        return plan;
      },
    },
    broadcast: () => {},
  });

  await assert.rejects(
    runtime.ensureWorkspace({
      turnID: "turn-2",
      sessionID: "session-1",
      workspaceID: "workspace-2",
      isolateGitWorkdir: true,
      workspace: null,
      character: { id: "boss" },
    }),
    (error) =>
      error instanceof AgentBusyError &&
      /다른 업무가 작업 공간을 준비 중/.test(error.message),
  );
  assert.equal(provisionCalled, false);
});

test("workspace 인덱스는 검토본과 별개로 실제 실행 workspace만 하나로 제한한다", () => {
  const migrationSource = readFileSync(
    new URL(
      "../../database/migrations/026_remove_automatic_workspace_approval.sql",
      import.meta.url,
    ),
    "utf8",
  );
  const predicate = migrationSource.slice(
    migrationSource.lastIndexOf("CREATE UNIQUE INDEX"),
  );

  assert.match(
    migrationSource,
    /DROP INDEX IF EXISTS task_workspaces_one_open_per_session_idx/,
  );
  assert.match(predicate, /status IN \('provisioning', 'active'\)/);
  assert.doesNotMatch(predicate, /'awaiting_approval'/);
  assert.doesNotMatch(predicate, /'conflict'/);
  assert.doesNotMatch(predicate, /'merging'/);
});

test("Git 프로젝트 신규 업무도 worktree 검사 없이 공유 폴더에서 시작한다", async () => {
  const events = [];
  const executionGate = deferred();
  const runtime = new AgentRuntime({
    pool: { query: async () => ({ rowCount: 1, rows: [] }) },
    withTransaction: async () => {},
    workdir: "/repo",
    workspaceManager: {
      isRepository: async () => {
        assert.fail("신규 업무에서 Git 저장소 검사를 하면 안 됩니다.");
      },
      planProvision: async () => {
        assert.fail("신규 업무에서 worktree 계획을 만들면 안 됩니다.");
      },
      provisionPlanned: async () => {
        assert.fail("신규 업무에서 worktree를 생성하면 안 됩니다.");
      },
    },
    broadcast: () => {},
  });
  runtime.prepareTurn = async (input) => {
    events.push("db:prepare-turn");
    assert.equal(input.isolateGitWorkdir, false);
    return {
      turnID: "turn-1",
      sessionID: "session-1",
      workspaceID: null,
      conversationID: input.conversationID,
      externalSessionID: null,
      character: { id: "boss", backend: "codex" },
      prompt: input.prompt,
      workspace: null,
      reusedSession: false,
      isolateGitWorkdir: false,
    };
  };
  runtime.beginPreparedTurn = async () => {
    events.push("db:begin-turn");
  };
  runtime.execute = async (state) => {
    events.push(`cli:${state.workdir}`);
    await executionGate.promise;
  };

  const result = await runtime.start({
    characterID: "boss",
    prompt: "업무",
    conversationID: "11111111-1111-1111-1111-111111111111",
  });

  assert.deepEqual(events, [
    "db:prepare-turn",
    "db:begin-turn",
    "cli:/repo",
  ]);
  assert.equal(result.status, "running");
  assert.equal(runtime.running.get("boss").workspace, null);
  executionGate.resolve();
});

test("완료된 변경 업무는 자동 통합 없이 통합 대기로 저장한다", async () => {
  const queries = [];
  const broadcasts = [];
  let approvalCount = 0;
  const review = {
    hasChanges: true,
    reviewTree: "review-tree",
    headCommit: "head-commit",
    changedFiles: [{ status: "M", path: "README.md" }],
  };
  const query = async (text, values) => {
    queries.push({ text, values });
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async () => review,
    },
    broadcast: (event) => broadcasts.push(event),
  });
  runtime.approveWorkspace = async () => {
    approvalCount += 1;
  };
  const state = {
    ...makeCodexActivityState(),
    sessionID: "session-1",
    character: {
      id: "boss",
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: true,
    },
    workspace: {
      ...workspaceDatabaseRow({ status: "active" }),
      id: "workspace-1",
      cliSessionID: "session-1",
      repositoryRoot: "/repo",
      sourceWorkdir: "/repo",
      worktreePath: "/worktrees/workspace-1",
      executionWorkdir: "/worktrees/workspace-1",
      branchName: "officestra/boss/workspace-1",
      baseBranch: "main",
      baseCommit: "base-commit",
    },
    initialGeneratedImages: new Set(),
    externalSessionID: null,
    responseText: "완료했습니다.",
    visibleAgentMessages: [{ key: "message-1", text: "완료했습니다." }],
    usage: null,
    cancelRequested: false,
  };
  runtime.running.set("boss", state);

  await runtime.complete(state, {
    text: "완료했습니다.",
    needsInput: false,
  });

  const workspaceUpdate = queries.find(({ text }) =>
    /UPDATE task_workspaces/.test(text)
  );
  assert.equal(workspaceUpdate.values[1], "awaiting_approval");
  assert.equal(workspaceUpdate.values[2], true);
  assert.equal(workspaceUpdate.values[3], "turn-1");
  assert.equal(workspaceUpdate.values[4], "review-tree");
  assert.equal(state.workspace.status, "awaiting_approval");
  assert.equal(approvalCount, 0);
  assert.equal(
    broadcasts.some((event) =>
      event.type === "workspace.changed" &&
      event.status === "awaiting_approval"
    ),
    true,
  );
});

test("사용자 답이 필요한 완료 턴은 workspace 검토를 시작하지 않는다", async () => {
  let reviewCount = 0;
  const query = async () => ({ rowCount: 1 });
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async () => {
        reviewCount += 1;
        return { hasChanges: true };
      },
    },
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    sessionID: "session-1",
    character: {
      id: "boss",
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: true,
    },
    workspace: { status: "active" },
    initialGeneratedImages: new Set(),
    externalSessionID: null,
    responseText: "선택해 주세요.",
    visibleAgentMessages: [{ key: "message-1", text: "선택해 주세요." }],
    usage: null,
    cancelRequested: false,
  };
  runtime.running.set("boss", state);

  await runtime.complete(state, {
    text: "선택해 주세요.",
    needsInput: true,
  });

  assert.equal(reviewCount, 0);
  assert.equal(state.workspace.status, "active");
});

test("파생 RAG 실패는 완료 턴과 작업 기록 저장을 되돌리지 않는다", async (t) => {
  const warnings = [];
  t.mock.method(console, "warn", (...values) => warnings.push(values));
  const queries = [];
  let transactionCount = 0;
  const committedTransactions = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/WITH selected_project AS/.test(text)) {
      return {
        rowCount: 1,
        rows: [{ workRecordId: "record-1" }],
      };
    }
    if (/DELETE FROM rag_documents/.test(text)) {
      throw new Error("RAG unavailable");
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => {
      transactionCount += 1;
      const current = transactionCount;
      const result = await body({ query });
      committedTransactions.push(current);
      return result;
    },
    workdir: "/repo",
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    prompt: "작업 기록을 저장해줘.",
    recordPrompt: "작업 기록을 저장해줘.",
    workdir: "/repo",
    character: {
      id: "boss",
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: false,
    },
    workspace: null,
    initialGeneratedImages: new Set(),
    externalSessionID: null,
    responseText: "완료했습니다.",
    visibleAgentMessages: [{ key: "message-1", text: "완료했습니다." }],
    usage: null,
    cancelRequested: false,
  };
  runtime.running.set("boss", state);

  await runtime.complete(state, {
    text: "완료했습니다.",
    needsInput: false,
  });

  assert.equal(transactionCount, 2);
  assert.deepEqual(committedTransactions, [1]);
  assert.equal(
    queries.some(({ text }) => /status = 'completed'/.test(text)),
    true,
  );
  assert.equal(
    queries.some(({ text }) => /WITH selected_project AS/.test(text)),
    true,
  );
  assert.equal(runtime.running.has("boss"), false);
  assert.equal(warnings.length, 1);
});

test("완료 응답의 출처 블록은 본문에서 숨기고 별도 행으로 저장한다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/INSERT INTO turn_response_sources/.test(text)) {
      return {
        rowCount: 1,
        rows: [{ id: "source-1", sourceKind: "file" }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });
  const rawResponse = [
    "완료했습니다.",
    "[OFFICE_SOURCES]",
    '[{"kind":"file","title":"README","locator":"README.md:8"}]',
  ].join("\n");
  const state = {
    ...makeCodexActivityState(),
    workdir: "/repo",
    character: { id: "boss", backend: "codex" },
    workspace: null,
    initialGeneratedImages: new Set(),
    externalSessionID: null,
    responseText: rawResponse,
    visibleAgentMessages: [{ key: "message-1", text: rawResponse }],
    usage: null,
    cancelRequested: false,
  };
  runtime.running.set("boss", state);

  await runtime.complete(state, {
    text: "완료했습니다.",
    needsInput: false,
    sources: [{
      ordinal: 0,
      sourceKind: "file",
      title: "README",
      locator: "/repo/README.md:8",
      excerpt: null,
      ragDocumentID: null,
      workRecordID: null,
      metadata: {},
    }],
  });

  const messageUpdate = queries.find(({ text }) =>
    /UPDATE messages/.test(text)
  );
  const sourceInsert = queries.find(({ text }) =>
    /INSERT INTO turn_response_sources/.test(text)
  );
  const workRecordInsert = queries.find(({ text }) =>
    /WITH selected_project AS/.test(text)
  );
  assert.equal(messageUpdate.values[1], "완료했습니다.");
  assert.equal(sourceInsert.values[2], "file");
  assert.equal(sourceInsert.values[4], "README.md:8");
  assert.equal(
    JSON.parse(workRecordInsert.values[7]).responseSourceCount,
    1,
  );
});

test("존재하지 않는 출처 참조는 응답을 살리고 경고로 표시한다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/SELECT id::text FROM rag_documents/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });
  const state = {
    ...makeCodexActivityState(),
    workdir: "/repo",
    character: { id: "boss", backend: "codex" },
    workspace: null,
    initialGeneratedImages: new Set(),
    externalSessionID: null,
    responseText: '{"ok":true}',
    visibleAgentMessages: [{ key: "message-1", text: '{"ok":true}' }],
    usage: null,
    cancelRequested: false,
  };

  await runtime.complete(state, {
    text: '{"ok":true}',
    needsInput: false,
    sources: [{
      ordinal: 0,
      sourceKind: "rag",
      title: "없는 문서",
      locator: "rag_documents/missing",
      excerpt: null,
      ragDocumentID: "44444444-4444-4444-8444-444444444444",
      workRecordID: null,
      metadata: {},
    }],
  });

  const messageUpdate = queries.find(({ text }) =>
    /UPDATE messages/.test(text)
  );
  const turnUpdate = queries.find(({ text }) =>
    /response_source_warning/.test(text)
  );
  const workRecordInsert = queries.find(({ text }) =>
    /WITH selected_project AS/.test(text)
  );
  const workRecordMetadata = JSON.parse(workRecordInsert.values[7]);
  assert.equal(messageUpdate.values[1], '{"ok":true}');
  assert.match(turnUpdate.values[2], /RAG 출처 문서 참조를 찾을 수 없습니다/);
  assert.equal(workRecordMetadata.responseSourceCount, 0);
  assert.match(
    workRecordMetadata.responseSourceWarning,
    /RAG 출처 문서 참조를 찾을 수 없습니다/,
  );
  assert.equal(
    queries.some(({ text }) => /INSERT INTO turn_response_sources/.test(text)),
    false,
  );
});

test("provider 전환 전에 변경이 있으면 검토 대기로 바꾸고 세션 종료를 막는다", async () => {
  const queries = [];
  const broadcasts = [];
  let cleanupCount = 0;
  const row = workspaceDatabaseRow({
    status: "active",
    review_turn_id: null,
    review_tree: null,
    changed_files: [],
    reviewCandidateTurnID: "turn-1",
  });
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM turns AS turn/.test(text) && /turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (
      /FROM task_workspaces AS workspace/.test(text) &&
      /reviewCandidateTurnID/.test(text)
    ) {
      return { rowCount: 1, rows: [row] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async () => ({
        hasChanges: true,
        reviewTree: "provider-review-tree",
        headCommit: "provider-head-commit",
        changedFiles: [{ status: "M", path: "README.md" }],
      }),
      cleanup: async () => {
        cleanupCount += 1;
      },
    },
    broadcast: (event) => broadcasts.push(event),
  });

  await assert.rejects(
    runtime.prepareWorkspaceForSessionEnd("boss"),
    AgentBusyError,
  );

  const reviewUpdate = queries.find(({ text, values }) =>
    /UPDATE task_workspaces/.test(text) &&
    values?.includes("provider-review-tree")
  );
  assert.ok(reviewUpdate);
  assert.match(reviewUpdate.text, /status\s*=\s*'awaiting_approval'/);
  assert.equal(
    queries.some(({ text }) => /WITH updated_record AS/.test(text)),
    true,
  );
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    false,
  );
  assert.equal(cleanupCount, 0);
  assert.equal(
    broadcasts.some((event) =>
      event.type === "workspace.changed" &&
      event.status === "awaiting_approval"
    ),
    true,
  );
});

test("provider 전환 전 변경이 없으면 세션과 빈 worktree를 정리한다", async () => {
  const queries = [];
  const cleaned = [];
  const row = workspaceDatabaseRow({
    status: "active",
    review_turn_id: null,
    review_tree: null,
    changed_files: [],
    reviewCandidateTurnID: "turn-1",
  });
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM turns AS turn/.test(text) && /turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (
      /FROM task_workspaces AS workspace/.test(text) &&
      /reviewCandidateTurnID/.test(text)
    ) {
      return { rowCount: 1, rows: [row] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async () => ({
        hasChanges: false,
        reviewTree: "base-tree",
        headCommit: "base-commit",
        changedFiles: [],
      }),
      cleanup: async (workspace) => {
        cleaned.push(workspace);
      },
    },
    broadcast: () => {},
  });

  const result = await runtime.prepareWorkspaceForSessionEnd("boss");

  assert.deepEqual(result, { ended: true });
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    true,
  );
  assert.equal(
    queries.some(({ text }) =>
      /UPDATE cli_sessions/.test(text) && /ended_at/.test(text)
    ),
    true,
  );
  assert.equal(cleaned.length, 1);
  assert.equal(cleaned[0].cliSessionID, "session-1");
  const workRecordUpdate = queries.find(({ text }) =>
    /WITH updated_record AS/.test(text)
  );
  assert.equal(workRecordUpdate.values[0], "turn-1");
  assert.equal(workRecordUpdate.values[1], "not_required");
});

test("provider 전환 때 활성 workspace가 없으면 종료할 세션이 없다고 알린다", async () => {
  let cleanupCount = 0;
  const query = async () => ({ rowCount: 0, rows: [] });
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      cleanup: async () => {
        cleanupCount += 1;
      },
    },
    broadcast: () => {},
  });

  assert.deepEqual(
    await runtime.prepareWorkspaceForSessionEnd("boss"),
    { ended: false },
  );
  assert.equal(cleanupCount, 0);
});

test("검토 대기 변경이 base tree로 돌아오면 fetch가 active 상태를 복원한다", async () => {
  const queries = [];
  const broadcasts = [];
  const reviewedRow = workspaceDatabaseRow();
  const activeRow = workspaceDatabaseRow({
    status: "active",
    review_turn_id: null,
    review_tree: null,
    head_commit: "base-commit",
    changed_files: [],
    review_requested_at: null,
  });
  let diffWorkspace;
  const query = async (text, values) => {
    queries.push({ text, values });
    if (
      /UPDATE task_workspaces/.test(text) &&
      /status = 'active'/.test(text) &&
      /review_turn_id = NULL/.test(text)
    ) {
      return { rowCount: 1, rows: [activeRow] };
    }
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [reviewedRow] };
    }
    return { rowCount: 0, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async (workspace) => {
        assert.equal(workspace.status, "awaiting_approval");
        assert.equal(workspace.reviewTree, "review-tree");
        return {
          hasChanges: false,
          reviewTree: "base-tree",
          headCommit: "base-commit",
          changedFiles: [],
        };
      },
      diff: async (workspace) => {
        diffWorkspace = workspace;
        return { diff: "", diffTruncated: false };
      },
    },
    broadcast: (event) => broadcasts.push(event),
  });

  const result = await runtime.fetchWorkspaceReview("turn-1");

  assert.equal(result.workspace.status, "active");
  assert.equal(result.workspace.reviewTurnId, null);
  assert.equal(result.workspace.reviewTree, null);
  assert.deepEqual(result.workspace.changedFiles, []);
  assert.equal(diffWorkspace.status, "active");
  assert.equal(diffWorkspace.reviewTurnID, null);
  assert.equal(diffWorkspace.reviewTree, null);
  const released = queries.find(({ text }) =>
    /review_turn_id = NULL/.test(text) && /review_tree = NULL/.test(text)
  );
  assert.ok(released);
  assert.deepEqual(released.values, [
    "workspace-1",
    "base-commit",
    "review-tree",
  ]);
  assert.deepEqual(broadcasts, [{
    type: "workspace.changed",
    turnId: "turn-1",
    characterId: "boss",
    status: "active",
  }]);
});

test("승인은 저장소 advisory lock 뒤 병합하고 활성 세션을 유지한다", async () => {
  const queries = [];
  const calls = [];
  let reviewTransition;
  const row = workspaceDatabaseRow();
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [row] };
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      approve: async (workspace, options) => {
        calls.push({ kind: "approve", workspace, options });
        return {
          taskCommit: "task-commit",
          mergedCommit: "merged-commit",
        };
      },
      cleanup: async (workspace) => {
        calls.push({ kind: "cleanup", workspace });
      },
    },
    broadcast: () => {},
  });
  runtime.transitionWorkRecordReviewBestEffort = async (options) => {
    reviewTransition = options;
  };
  const result = await runtime.approveWorkspace("turn-1", "review-tree");

  assert.equal(result.workspace.status, "merged");
  assert.equal(result.workspace.mergedCommit, "merged-commit");
  assert.equal(calls[0].options.expectedReviewTree, "review-tree");
  assert.equal(calls[1].kind, "cleanup");
  assert.equal(reviewTransition.actorType, "user");
  assert.equal(
    queries.some(({ text, values }) =>
      /pg_advisory_xact_lock/.test(text) && values[0] === "/repo"
    ),
    true,
  );
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    false,
  );
  assert.equal(
    queries.some(({ text }) =>
      /UPDATE cli_sessions/.test(text) && /ended_at/.test(text)
    ),
    false,
  );
});

test("파생 RAG 실패에도 Git 승인은 merged로 끝난다", async (t) => {
  const warnings = [];
  t.mock.method(console, "warn", (...values) => warnings.push(values));
  const calls = [];
  let transactionCount = 0;
  let approvalErrorCount = 0;
  const query = async (text) => {
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [workspaceDatabaseRow()] };
    }
    if (/WITH updated_record AS/.test(text)) {
      return {
        rowCount: 1,
        rows: [{ workRecordId: "record-1" }],
      };
    }
    if (/DELETE FROM rag_documents/.test(text)) {
      throw new Error("RAG unavailable");
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => {
      transactionCount += 1;
      return body({ query });
    },
    workdir: "/repo",
    workspaceManager: {
      approve: async () => {
        calls.push("approve");
        return {
          taskCommit: "task-commit",
          mergedCommit: "merged-commit",
        };
      },
      cleanup: async () => calls.push("cleanup"),
    },
    broadcast: () => {},
  });
  runtime.recordWorkspaceApprovalError = async () => {
    approvalErrorCount += 1;
  };
  const result = await runtime.approveWorkspace("turn-1", "review-tree");

  assert.equal(result.workspace.status, "merged");
  assert.equal(result.workspace.mergedCommit, "merged-commit");
  assert.deepEqual(calls, ["approve", "cleanup"]);
  assert.equal(transactionCount, 3);
  assert.equal(approvalErrorCount, 0);
  assert.equal(warnings.length, 1);
});

test("작업 기록 상태 전환 실패에도 Git 승인은 merged로 끝난다", async (t) => {
  const warnings = [];
  t.mock.method(console, "warn", (...values) => warnings.push(values));
  let transactionCount = 0;
  let cleanupCount = 0;
  const query = async (text) => {
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [workspaceDatabaseRow()] };
    }
    if (/WITH updated_record AS/.test(text)) {
      throw new Error("work record unavailable");
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => {
      transactionCount += 1;
      return body({ query });
    },
    workdir: "/repo",
    workspaceManager: {
      approve: async () => ({
        taskCommit: "task-commit",
        mergedCommit: "merged-commit",
      }),
      cleanup: async () => {
        cleanupCount += 1;
      },
    },
    broadcast: () => {},
  });
  const result = await runtime.approveWorkspace("turn-1", "review-tree");

  assert.equal(result.workspace.status, "merged");
  assert.equal(result.workspace.mergedCommit, "merged-commit");
  assert.equal(transactionCount, 2);
  assert.equal(cleanupCount, 1);
  assert.equal(warnings.length, 1);
});

test("충돌 상태는 같은 review tree로 다시 병합할 수 있다", async () => {
  const queries = [];
  const calls = [];
  const row = workspaceDatabaseRow({ status: "conflict" });
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [row] };
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      approve: async (workspace, options) => {
        calls.push({ workspace, options });
        return {
          taskCommit: "retried-task-commit",
          mergedCommit: "retried-merged-commit",
        };
      },
      cleanup: async () => {},
    },
    broadcast: () => {},
  });
  const result = await runtime.approveWorkspace("turn-1", "review-tree");

  assert.equal(result.workspace.status, "merged");
  assert.equal(result.workspace.mergedCommit, "retried-merged-commit");
  assert.equal(calls[0].options.expectedReviewTree, "review-tree");
  assert.equal(
    queries.some(({ text }) =>
      /status IN \('awaiting_approval', 'conflict'\)/.test(text)
    ),
    true,
  );
});

test("충돌 재시도의 stale tree 오류는 충돌 상태를 유지한다", async () => {
  const queries = [];
  const broadcasts = [];
  const row = workspaceDatabaseRow({ status: "conflict" });
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [row] };
    }
    if (/RETURNING\s+session\.character_id/s.test(text)) {
      return {
        rowCount: 1,
        rows: [{ characterID: "boss", status: "conflict" }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      approve: async () => {
        assert.fail("stale review tree는 Git 병합을 시작하면 안 됩니다.");
      },
    },
    broadcast: (event) => broadcasts.push(event),
  });

  await assert.rejects(
    runtime.approveWorkspace("turn-1", "stale-review-tree"),
    AgentBusyError,
  );

  assert.equal(
    queries.some(({ text }) => /UPDATE task_workspaces AS workspace/.test(text)),
    false,
  );
  assert.deepEqual(broadcasts, []);
});

test("승인은 사용자가 실제로 확인한 review tree가 아니면 병합을 시작하지 않는다", async () => {
  const queries = [];
  let approveCount = 0;
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [workspaceDatabaseRow()] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      approve: async () => {
        approveCount += 1;
      },
    },
    broadcast: () => {},
  });

  await assert.rejects(
    runtime.approveWorkspace("turn-1", "stale-review-tree"),
    AgentBusyError,
  );

  assert.equal(approveCount, 0);
  assert.equal(
    queries.some(({ text }) => /status = 'merging'/.test(text)),
    false,
  );
});

test("검토 뒤 tree가 바뀌면 새 diff 메타데이터를 저장하고 재승인을 요구한다", async () => {
  const queries = [];
  const changedError = new Error("검토 뒤 변경사항이 달라졌습니다.");
  changedError.code = "changed-after-review";
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return { rowCount: 1, rows: [workspaceDatabaseRow()] };
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      approve: async () => {
        throw changedError;
      },
      prepareReview: async () => ({
        hasChanges: true,
        reviewTree: "new-review-tree",
        headCommit: "new-head-commit",
        changedFiles: [{ status: "A", path: "new.txt" }],
      }),
    },
    broadcast: () => {},
  });

  await assert.rejects(
    runtime.approveWorkspace("turn-1", "review-tree"),
    (error) => error === changedError,
  );

  const refreshed = queries.find(({ text, values }) =>
    /review_tree = CASE WHEN \$7 THEN \$2 ELSE NULL END/.test(text) &&
    values?.[1] === "new-review-tree"
  );
  assert.ok(refreshed);
  assert.equal(refreshed.values[2], "new-head-commit");
  assert.equal(refreshed.values[3], JSON.stringify([
    { status: "A", path: "new.txt" },
  ]));
});

test("검토 갱신 오류는 동시에 거절된 workspace를 다시 승인 대기로 만들지 않는다", async () => {
  const changedError = new Error("검토 뒤 변경사항이 달라졌습니다.");
  changedError.code = "changed-after-review";
  let workspaceStatus = "rejected";
  const query = async (text) => {
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return {
        rowCount: 1,
        rows: [workspaceDatabaseRow({ status: workspaceStatus })],
      };
    }
    if (/UPDATE task_workspaces/.test(text)) {
      const whereClause = text.split(/\bWHERE\b/i).slice(1).join(" WHERE ");
      const guardsReviewableStatus =
        /status\s+(?:=|IN\b)/.test(whereClause) &&
        /'awaiting_approval'/.test(whereClause);
      const isReviewable = [
        "awaiting_approval",
        "conflict",
      ].includes(workspaceStatus);
      if (!guardsReviewableStatus || isReviewable) {
        workspaceStatus = "awaiting_approval";
        return { rowCount: 1, rows: [] };
      }
      return { rowCount: 0, rows: [] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    workspaceManager: {
      prepareReview: async () => ({
        hasChanges: true,
        reviewTree: "new-review-tree",
        headCommit: "new-head-commit",
        changedFiles: [{ status: "A", path: "new.txt" }],
      }),
    },
    broadcast: () => {},
  });

  await runtime.recordWorkspaceApprovalError("turn-1", changedError);

  assert.equal(workspaceStatus, "rejected");
});

test("충돌한 workspace를 거절해도 세션을 유지하고 worktree는 보존한다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM task_workspaces AS workspace/.test(text)) {
      return {
        rowCount: 1,
        rows: [workspaceDatabaseRow({ status: "conflict" })],
      };
    }
    if (/RETURNING id/.test(text)) {
      return { rowCount: 1, rows: [{ id: "workspace-1" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });
  const result = await runtime.rejectWorkspace("turn-1");

  assert.equal(result.workspace.status, "rejected");
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    false,
  );
  assert.equal(
    queries.some(({ text }) => /worktree remove|DELETE FROM task_workspaces/.test(text)),
    false,
  );
});

test("worktree가 없는 활성 CLI 세션은 종료하지 않고 다음 업무에 재사용한다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM characters/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "보스",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "high",
          fastMode: true,
          permission: "workspace-write",
          identityPrompt: "업무를 처리한다.",
          config: {},
        }],
      };
    }
    if (/turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (
      /SELECT[\s\S]*workspace\.review_turn_id/.test(text) &&
      !/active_cli_sessions/.test(text)
    ) {
      return { rowCount: 0, rows: [] };
    }
    if (/FROM active_cli_sessions AS active/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "session-1",
          externalSessionID: "external-1",
          conversationID: "11111111-1111-1111-1111-111111111111",
          conversationWorkdir: "/repo",
          sessionRepositoryRoot: null,
          workspaceID: null,
          workspaceStatus: null,
          resumeExecutionWorkdir: "/old-worktree",
        }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });

  const prepared = await runtime.prepareTurn({
    characterID: "boss",
    prompt: "다음 업무",
    conversationID: "22222222-2222-2222-2222-222222222222",
    isolateGitWorkdir: true,
    senderCharacterID: "right-man",
  });

  const senderInsert = queries.find(({ text }) => /INSERT INTO turns/.test(text));
  assert.match(senderInsert.text, /sender_character_id/);
  assert.equal(senderInsert.values[8], "right-man");

  assert.equal(prepared.sessionID, "session-1");
  assert.equal(prepared.externalSessionID, "external-1");
  assert.equal(prepared.reusedSession, true);
  assert.equal(prepared.workspace, null);
  assert.equal(prepared.resumeExecutionWorkdir, "/old-worktree");
  assert.equal(
    queries.some(({ text }) =>
      /completed_turn\.status = 'completed'/.test(text) &&
      /resume_workspace/.test(text)
    ),
    true,
  );
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    false,
  );
  assert.equal(
    queries.some(({ text }) =>
      /UPDATE cli_sessions/.test(text) && /ended_at/.test(text)
    ),
    false,
  );
});

test("검토 대기 workspace가 있어도 기존 CLI 세션으로 다음 업무를 시작한다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM characters/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "보스",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "high",
          fastMode: true,
          permission: "workspace-write",
          identityPrompt: "업무를 처리한다.",
          config: {},
        }],
      };
    }
    if (/turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (/FROM active_cli_sessions AS active/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "session-1",
          externalSessionID: "external-1",
          conversationID: "11111111-1111-1111-1111-111111111111",
          conversationWorkdir: "/repo",
          sessionRepositoryRoot: null,
          workspaceID: null,
          workspaceStatus: null,
        }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });

  const prepared = await runtime.prepareTurn({
    characterID: "boss",
    prompt: "다음 업무",
    conversationID: "11111111-1111-1111-1111-111111111111",
  });
  assert.equal(prepared.sessionID, "session-1");
  assert.equal(prepared.externalSessionID, "external-1");
  assert.equal(prepared.reusedSession, true);
  assert.equal(prepared.workspace, null);
  assert.match(queries[0].text, /pg_advisory_xact_lock/);
  assert.deepEqual(queries[0].values, ["officestra:character:boss"]);
});

test("다른 repository의 활성 CLI 세션은 보존하되 다음 업무에 재사용하지 않는다", async () => {
  const queries = [];
  const query = async (text, values) => {
    queries.push({ text, values });
    if (/FROM characters/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "보스",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "high",
          fastMode: true,
          permission: "workspace-write",
          identityPrompt: "업무를 처리한다.",
          config: {},
        }],
      };
    }
    if (/turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (/FROM active_cli_sessions AS active/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "old-session",
          externalSessionID: "old-external",
          conversationID: "11111111-1111-1111-1111-111111111111",
          conversationWorkdir: "/other-repository",
          sessionRepositoryRoot: "/other-repository",
          workspaceID: null,
          workspaceStatus: null,
        }],
      };
    }
    if (/INSERT INTO cli_sessions/.test(text)) {
      return { rowCount: 1, rows: [{ id: "new-session" }] };
    }
    if (/SELECT id[\s\S]*FROM cli_sessions/.test(text)) {
      return { rowCount: 1, rows: [{ id: "old-session" }] };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo/subdirectory",
    repositoryRoot: "/repo",
    broadcast: () => {},
  });

  const prepared = await runtime.prepareTurn({
    characterID: "boss",
    prompt: "새 프로젝트 업무",
    conversationID: "22222222-2222-2222-2222-222222222222",
  });

  assert.equal(prepared.sessionID, "new-session");
  assert.equal(prepared.externalSessionID, null);
  assert.equal(prepared.conversationID, "22222222-2222-2222-2222-222222222222");
  assert.equal(prepared.reusedSession, false);
  assert.equal(
    queries.some(({ text }) => /DELETE FROM active_cli_sessions/.test(text)),
    false,
  );
  assert.equal(
    queries.some(({ text }) =>
      /UPDATE cli_sessions/.test(text) && /ended_at/.test(text)
    ),
    false,
  );
});

test("같은 canonical repository의 다른 하위 workdir에서는 활성 세션을 재사용한다", async () => {
  const query = async (text) => {
    if (/FROM characters/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "보스",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "high",
          fastMode: true,
          permission: "workspace-write",
          identityPrompt: "업무를 처리한다.",
          config: {},
        }],
      };
    }
    if (/turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (/FROM active_cli_sessions AS active/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "session-1",
          externalSessionID: "external-1",
          conversationID: "11111111-1111-1111-1111-111111111111",
          conversationWorkdir: "/repo/old-subdirectory",
          sessionRepositoryRoot: "/repo",
          workspaceID: null,
          workspaceStatus: null,
        }],
      };
    }
    return { rowCount: 1, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo/new-subdirectory",
    repositoryRoot: "/repo",
    broadcast: () => {},
  });

  const prepared = await runtime.prepareTurn({
    characterID: "boss",
    prompt: "같은 프로젝트 업무",
    conversationID: "22222222-2222-2222-2222-222222222222",
  });

  assert.equal(prepared.sessionID, "session-1");
  assert.equal(prepared.externalSessionID, "external-1");
  assert.equal(prepared.reusedSession, true);
});

test("활성 CLI 세션이 없으면 검토 대기 workspace와 분리해 새 세션을 만든다", async () => {
  const query = async (text) => {
    if (/FROM characters/.test(text)) {
      return {
        rowCount: 1,
        rows: [{
          id: "boss",
          name: "보스",
          backend: "codex",
          model: "gpt-5.6-sol",
          effort: "high",
          fastMode: true,
          permission: "workspace-write",
          identityPrompt: "업무를 처리한다.",
          config: {},
        }],
      };
    }
    if (/turn\.status IN/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (/FROM active_cli_sessions AS active/.test(text)) {
      return { rowCount: 0, rows: [] };
    }
    if (/INSERT INTO cli_sessions/.test(text)) {
      return { rowCount: 1, rows: [{ id: "new-session" }] };
    }
    return { rowCount: 0, rows: [] };
  };
  const runtime = new AgentRuntime({
    pool: { query },
    withTransaction: async (body) => body({ query }),
    workdir: "/repo",
    broadcast: () => {},
  });

  const prepared = await runtime.prepareTurn({
    characterID: "boss",
    prompt: "다음 업무",
    conversationID: "11111111-1111-1111-1111-111111111111",
  });
  assert.equal(prepared.sessionID, "new-session");
  assert.equal(prepared.reusedSession, false);
});

test("실시간 피드는 과거 workspace 검토 턴을 최근 제한 밖에 고정하지 않는다", () => {
  const serverSource = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  const queryStart = serverSource.indexOf("async function queryTurnFeed");
  const queryEnd = serverSource.indexOf(
    "async function workspaceReview",
    queryStart,
  );
  assert.ok(queryStart >= 0 && queryEnd > queryStart);
  const querySource = serverSource.slice(queryStart, queryEnd);

  const selectedStart = querySource.indexOf("selected_turn_ids AS (");
  const selectedEnd = querySource.indexOf("\n      SELECT\n        t.id", selectedStart);
  assert.ok(selectedStart >= 0 && selectedEnd > selectedStart);
  assert.doesNotMatch(
    querySource.slice(selectedStart, selectedEnd),
    /task_workspace|awaiting_approval|merging|conflict/,
  );
});

test("실시간 피드는 직원별 최신 대화를 최근 제한 밖에서도 보존한다", () => {
  const serverSource = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  const queryStart = serverSource.indexOf("async function queryTurnFeed");
  const queryEnd = serverSource.indexOf(
    "async function workspaceReview",
    queryStart,
  );
  assert.ok(queryStart >= 0 && queryEnd > queryStart);
  const querySource = serverSource.slice(queryStart, queryEnd);

  assert.match(
    serverSource,
    /const liveFeedMinimumTurnsPerCharacter = 10;/,
  );
  assert.match(
    querySource,
    /row_number\(\) OVER \([\s\S]*PARTITION BY matching\.character_id/,
  );
  assert.match(
    querySource,
    /UNION[\s\S]*SELECT ranked\.id[\s\S]*ranked\.character_rank <= \$6::integer/,
  );
  assert.match(
    querySource,
    /\$5::boolean[\s\S]*\$3::text IS NULL[\s\S]*\$4::integer = 0/,
  );
  assert.match(
    querySource,
    /includesCharacterMinimums,[\s\S]*liveFeedMinimumTurnsPerCharacter/,
  );
  assert.match(
    querySource,
    /ORDER BY started_at DESC, id DESC[\s\S]*ORDER BY t\.started_at DESC, t\.id DESC/,
  );

  const liveStart = serverSource.indexOf("async function queryLiveFeed");
  const archiveStart = serverSource.indexOf(
    "async function queryArchiveFeed",
    liveStart,
  );
  const contextStart = serverSource.indexOf(
    "function withSessionContext",
    archiveStart,
  );
  assert.match(
    serverSource.slice(liveStart, archiveStart),
    /includesCharacterMinimums: true/,
  );
  assert.match(
    serverSource.slice(archiveStart, contextStart),
    /includesCharacterMinimums: false/,
  );
});

test("대화 보관함은 전체 검색을 12건 페이지로 요청한다", () => {
  const serverSource = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  const archiveStart = serverSource.indexOf("async function archiveFeed");
  const archiveEnd = serverSource.indexOf("async function liveFeedTurn", archiveStart);
  const archiveSource = serverSource.slice(archiveStart, archiveEnd);
  const queryStart = serverSource.indexOf("async function queryArchiveFeed");
  const queryEnd = serverSource.indexOf("function withSessionContext", queryStart);
  const querySource = serverSource.slice(queryStart, queryEnd);

  assert.match(archiveSource, /limit"\) \?\? 12/);
  assert.match(archiveSource, /offset/);
  assert.match(querySource, /ILIKE '%' \|\| \$1 \|\| '%'/);
  assert.match(querySource, /includesCharacterMinimums: false/);
});

// 추론은 단계가 끝나기 전부터 자란다. 폴링할 때마다 자란 만큼 카드가 갱신되고,
// 끝나기 전에는 진행 중으로 남아야 화면에서 실시간으로 보인다.
test("Antigravity 추론은 자라는 도중에도 진행 중 카드로 갱신된다", async () => {
  const added = [];
  const runtime = antigravityReasoningRuntime([
    [{ stepIndex: 1, text: "상자 수를 센다.", done: false }],
    [{ stepIndex: 1, text: "상자 수를 센다. 검산한다.", done: false }],
    [{ stepIndex: 1, text: "상자 수를 센다. 검산한다.", done: true }],
  ], added);
  const state = antigravityReasoningState();

  await runtime.consumeAntigravityReasoning(state);
  await runtime.consumeAntigravityReasoning(state);
  await runtime.consumeAntigravityReasoning(state);

  assert.deepEqual(added, [
    {
      kind: "thinking",
      text: "상자 수를 센다.",
      eventKey: "antigravity:conversation-1:1:thinking",
      status: "running",
      preserveText: false,
      messageScoped: false,
    },
    {
      kind: "thinking",
      text: "상자 수를 센다. 검산한다.",
      eventKey: "antigravity:conversation-1:1:thinking",
      status: "running",
      preserveText: false,
      messageScoped: false,
    },
    {
      kind: "thinking",
      text: "상자 수를 센다. 검산한다.",
      eventKey: "antigravity:conversation-1:1:thinking",
      status: "completed",
      preserveText: false,
      messageScoped: false,
    },
  ]);
});

test("내용과 상태가 그대로면 같은 추론을 다시 올리지 않는다", async () => {
  const added = [];
  const runtime = antigravityReasoningRuntime([
    [{ stepIndex: 1, text: "같은 추론", done: false }],
    [{ stepIndex: 1, text: "같은 추론", done: false }],
  ], added);
  const state = antigravityReasoningState();

  await runtime.consumeAntigravityReasoning(state);
  await runtime.consumeAntigravityReasoning(state);

  assert.equal(added.length, 1);
});

test("턴이 끝나면 진행 중이던 추론 카드를 완료로 확정한다", async () => {
  const added = [];
  const runtime = antigravityReasoningRuntime([
    [{ stepIndex: 2, text: "끝내 완료 표시가 오지 않았다.", done: false }],
    [{ stepIndex: 2, text: "끝내 완료 표시가 오지 않았다.", done: false }],
  ], added);
  const state = antigravityReasoningState();

  await runtime.consumeAntigravityReasoning(state);
  await runtime.stopAntigravityReasoningMonitor(state);

  assert.deepEqual(added.map((activity) => activity.status), [
    "running",
    "completed",
  ]);
});

test("추론 폴링은 Antigravity 이외의 백엔드와 대화 ID 없는 상태를 건너뛴다", async () => {
  const added = [];
  let reads = 0;
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    antigravityReasoningReader: () => {
      reads += 1;
      return [{ stepIndex: 1, text: "읽으면 안 된다.", done: true }];
    },
  });
  runtime.addParsedActivity = async (_state, activity) => {
    added.push(activity);
  };

  await runtime.consumeAntigravityReasoning({
    character: { id: "left-man", backend: "claude" },
    externalSessionID: "session-1",
  });
  await runtime.consumeAntigravityReasoning({
    character: { id: "right-man", backend: "antigravity" },
    externalSessionID: null,
  });

  assert.equal(reads, 0);
  assert.equal(added.length, 0);
});

test("추론 읽기가 실패해도 턴 종료를 막지 않는다", async () => {
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    antigravityReasoningReader: () => {
      throw new Error("대화 DB가 잠겨 있습니다.");
    },
  });

  await runtime.stopAntigravityReasoningMonitor(antigravityReasoningState());
});

function antigravityReasoningRuntime(readings, added) {
  let call = 0;
  const runtime = new AgentRuntime({
    pool: {},
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
    antigravityReasoningReader: () => readings[
      Math.min(call++, readings.length - 1)
    ],
  });
  runtime.promotePendingAgentMessage = async () => {};
  runtime.scopedActivity = (_state, activity) => activity;
  runtime.addParsedActivity = async (_state, activity) => {
    added.push(activity);
  };
  return runtime;
}

function antigravityReasoningState() {
  return {
    character: { id: "right-man", backend: "antigravity" },
    externalSessionID: "conversation-1",
    reasoningMonitorTimer: null,
    reasoningPollPromise: null,
    emittedReasoning: new Map(),
  };
}

// Antigravity 터미널 턴은 CLI가 사용량을 직접 알려주므로 디스크를 다시 읽지 않는다.
test("터미널 턴이 사용량을 이미 받았으면 기록에서 다시 읽지 않는다", async () => {
  const runtime = terminalTurnRuntime();
  let reads = 0;
  runtime.terminalTurnUsage = async () => {
    reads += 1;
    return { inputTokens: 999, outputTokens: 999 };
  };
  let captured = null;
  runtime.complete = async (state) => {
    captured = state;
  };

  await runtime.completeTerminalTurn({
    characterID: "right-man",
    turnID: "turn-1",
    response: "완료했습니다.",
    usage: { inputTokens: 10, outputTokens: 2 },
  });

  assert.equal(reads, 0);
  assert.deepEqual(captured.usage, { inputTokens: 10, outputTokens: 2 });
});

test("사용량 없이 끝난 터미널 턴은 CLI 기록에서 사용량을 채운다", async () => {
  const runtime = terminalTurnRuntime();
  const windows = [];
  runtime.terminalTurnUsage = async (state, startedAt) => {
    windows.push({ startedAt, endedAt: state.endedAt });
    return { inputTokens: 12, outputTokens: 34 };
  };
  let captured = null;
  runtime.complete = async (state) => {
    captured = state;
  };
  const endedAt = new Date("2026-09-02T14:01:00.000Z");

  await runtime.completeTerminalTurn({
    characterID: "left-man",
    turnID: "turn-1",
    response: "완료했습니다.",
    endedAt,
  });

  assert.deepEqual(captured.usage, { inputTokens: 12, outputTokens: 34 });
  assert.deepEqual(windows, [{
    startedAt: new Date("2026-09-02T14:00:00.000Z"),
    endedAt,
  }]);
});

test("세 CLI 터미널 활동은 기존 DB 활동 경로로 저장하며 본문을 바꾸지 않는다", async () => {
  for (const backend of ["codex", "claude", "antigravity"]) {
    const runtime = terminalTurnRuntime();
    const originalQuery = runtime.pool.query;
    const writes = [];
    runtime.pool.query = async (sql, values) => {
      if (sql.includes("SELECT")) {
        const result = await originalQuery(sql, values);
        result.rows[0].executionBackend = backend;
        return result;
      }
      writes.push({ sql, values });
      return { rowCount: 1, rows: [] };
    };
    let result;
    runtime.complete = async (state, decoded) => { result = { state, decoded }; };
    const activities = [
      { kind: "message", text: "진행 설명", eventKey: "terminal:message:1", status: "completed" },
      { kind: "thinking", text: "공개 요약", eventKey: "terminal:thinking:1", status: "completed" },
      { kind: "command", text: "swift test", eventKey: "terminal:command:1", status: "completed" },
    ];
    await runtime.completeTerminalTurn({ characterID: "left-man", turnID: "turn-1", response: "최종 답변", usage: {}, activities });
    const inserts = writes.filter(({ sql }) => sql.includes("INSERT INTO turn_activities"));
    assert.equal(inserts.length, 3);
    assert.deepEqual(inserts.map(({ values }) => values.slice(1, 4)), [[1, "message", "진행 설명"], [2, "thinking", "공개 요약"], [3, "command", "swift test"]]);
    assert.equal(result.state.responseText, "최종 답변");
    assert.equal(runtime.completedResponseText(result.state, result.decoded), "최종 답변");
  }
});

test("터미널 사용량 읽기는 대화 ID나 시작 시각이 없으면 건너뛴다", async () => {
  const runtime = terminalTurnRuntime();
  const startedAt = new Date("2026-09-02T14:00:00.000Z");

  assert.equal(
    await runtime.terminalTurnUsage(
      { character: { backend: "claude" }, endedAt: new Date() },
      startedAt,
    ),
    null,
  );
  assert.equal(
    await runtime.terminalTurnUsage(
      {
        character: { backend: "codex" },
        externalSessionID: "session-1",
        endedAt: new Date(),
      },
      null,
    ),
    null,
  );
  assert.equal(
    await runtime.terminalTurnUsage(
      {
        character: { backend: "antigravity" },
        externalSessionID: "session-1",
        endedAt: new Date(),
      },
      startedAt,
    ),
    null,
  );
});

test("완료 턴 후속 갱신은 Antigravity 터미널에만 허용한다", async () => {
  const runtime = terminalTurnRuntime();
  let sql;
  const original = runtime.pool.query;
  runtime.pool.query = async (query, values) => {
    sql = query;
    assert.deepEqual(values, ["turn-1", "left-man", true]);
    return original(query, values);
  };
  await assert.rejects(runtime.completeTerminalTurn({
    characterID: "left-man", turnID: "turn-1", response: "후속 응답", refreshCompleted: true,
  }), /Antigravity 터미널 후속 응답만/);
  assert.match(sql, /turn.origin = 'terminal'/);
  assert.match(sql, /\$3::boolean AND turn.backend = 'antigravity' AND turn.status = 'completed'/);
  assert.doesNotMatch(sql, /status = 'interrupted'/);
});

function terminalTurnRuntime() {
  return new AgentRuntime({
    pool: {
      query: async () => ({
        rowCount: 1,
        rows: [{
          id: "turn-1",
          sessionID: "session-row",
          prompt: "질문",
          executionBackend: "claude",
          executionModel: "claude-sonnet-5",
          executionEffort: "high",
          executionFastMode: false,
          startedAt: new Date("2026-09-02T14:00:00.000Z"),
          conversationID: "conversation-1",
          externalSessionID: "external-1",
          characterID: "left-man",
          name: "직원",
          seat: "left-man",
          backend: "claude",
          model: "claude-sonnet-5",
          effort: "high",
          fastMode: false,
          autoCompactPercent: 30,
          permission: "accept-edits",
          identityPrompt: "",
          config: {},
        }],
      }),
    },
    withTransaction: async (operation) => await operation({}),
    workdir: "/repo",
    repositoryRoot: "/repo",
    broadcast: () => {},
  });
}
