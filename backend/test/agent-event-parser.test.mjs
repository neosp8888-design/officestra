// 이 파일은 CLI 원문에서 공개 가능한 실시간 이벤트만 추출되는지 검증한다.

import assert from "node:assert/strict";
import test from "node:test";

import {
  decodeAgentResponse,
  parseAgentEvent,
} from "../src/agent-event-parser.mjs";

test("Codex 생각 요약을 공개 활동으로 변환한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "reason-1",
        type: "reasoning",
        text: "레이아웃 구조를 확인했습니다.",
      },
    }),
    "codex",
  );

  assert.deepEqual(event.activity, {
    kind: "thinking",
    text: "레이아웃 구조를 확인했습니다.",
    eventKey: "reason-1",
    status: "completed",
    preserveText: false,
    messageScoped: false,
    isCodexReasoning: true,
  });
});

test("Codex reasoning에 실제 문구가 없으면 가짜 활동을 만들지 않는다", () => {
  for (const type of ["item.started", "item.updated", "item.completed"]) {
    const event = parseAgentEvent(
      JSON.stringify({
        type,
        item: {
          id: "reason-1",
          type: "reasoning",
        },
      }),
      "codex",
    );

    assert.equal(event, null);
  }
});

test("JSON 객체가 아닌 이벤트 행은 무시한다", () => {
  for (const value of ["null", "[]", '"event"', "1", "true"]) {
    assert.equal(parseAgentEvent(value, "codex"), null);
    assert.equal(parseAgentEvent(value, "claude"), null);
    assert.equal(parseAgentEvent(value, "antigravity"), null);
  }
});

test("Antigravity 초기화에서 재개 가능한 대화 ID를 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "init",
      conversation_id: "4df207a4-fbc5-4a28-994d-2e69ca276599",
      init: { model: "gemini-3.7-flash" },
    }),
    "antigravity",
  );

  assert.deepEqual(event, {
    sessionID: "4df207a4-fbc5-4a28-994d-2e69ca276599",
  });
});

test("Antigravity 응답 조각과 단계별 사용량을 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "step_update",
      step_update: {
        conversation_id: "conversation-1",
        step_index: 3,
        state: "DONE",
        step_type: "agent_response",
        text_delta: "완료했습니다.\n",
        usage: {
          input_tokens: 1_200,
          output_tokens: 30,
          thinking_tokens: 12,
          cache_read_tokens: 900,
          total_tokens: 1_242,
        },
      },
    }),
    "antigravity",
  );

  assert.equal(event.responseDelta, "완료했습니다.\n");
  assert.equal(event.responseStepKey, "antigravity:conversation-1:3");
  assert.equal(event.responseStepDone, true);
  assert.equal(event.usageIsDelta, true);
  assert.deepEqual(event.usage, {
    inputTokens: 1_200,
    outputTokens: 30,
    cachedInputTokens: 900,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: 12,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  });
});

test("Antigravity 추론 단계 이벤트에서 thinking 활동을 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "step_update",
      step_update: {
        conversation_id: "conversation-1",
        step_index: 1,
        state: "DONE",
        step_type: "thinking",
        thinking: "코드 구조를 분석하고 리팩토링 계획을 수립합니다.",
      },
    }),
    "antigravity",
  );

  assert.deepEqual(event.activity, {
    kind: "thinking",
    text: "코드 구조를 분석하고 리팩토링 계획을 수립합니다.",
    eventKey: "antigravity:conversation-1:1:thinking",
    status: "completed",
    preserveText: false,
    messageScoped: false,
  });
});

test("Antigravity 응답 조각과 함께 전달된 thinking도 활동으로 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "step_update",
      step_update: {
        conversation_id: "conversation-1",
        step_index: 2,
        state: "DONE",
        step_type: "agent_response",
        text_delta: "작업을 완료했습니다.",
        thinking: "결과를 정리하여 사용자에게 보고합니다.",
      },
    }),
    "antigravity",
  );

  assert.equal(event.responseDelta, "작업을 완료했습니다.");
  assert.deepEqual(event.activities, [
    {
      kind: "thinking",
      text: "결과를 정리하여 사용자에게 보고합니다.",
      eventKey: "antigravity:conversation-1:2:thinking",
      status: "completed",
      preserveText: false,
      messageScoped: false,
    },
  ]);
});

test("Antigravity 명령 도구는 출력 없이 안전한 명령만 공개한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "step_update",
      step_update: {
        conversation_id: "conversation-1",
        step_index: 2,
        state: "DONE",
        step_type: "tool",
        tool_name: "run_command",
        tool_info: {
          name: "run_command",
          parameters: { CommandLine: "pwd" },
          output: "/private/tmp/antigravity/scratch\n",
        },
      },
    }),
    "antigravity",
  );

  assert.deepEqual(event.activity, {
    kind: "command",
    text: "pwd",
    eventKey: "antigravity:conversation-1:2",
    status: "completed",
    preserveText: false,
    messageScoped: false,
  });
  assert.doesNotMatch(JSON.stringify(event), /scratch/);
});

test("Antigravity 최종 결과 사용량은 단계 사용량이 없을 때만 쓰는 fallback이다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      event: "result",
      result: {
        conversation_id: "conversation-1",
        status: "SUCCESS",
        response: "완료했습니다.\n",
        usage: {
          input_tokens: 2_000,
          output_tokens: 50,
          thinking_tokens: 20,
          cache_read_tokens: 1_400,
        },
      },
    }),
    "antigravity",
  );

  assert.equal(event.sessionID, "conversation-1");
  assert.equal(event.responseText, undefined);
  assert.equal(event.responseFallback, "완료했습니다.");
  assert.equal(event.usage, undefined);
  assert.equal(event.usageFallback.inputTokens, 2_000);
});

test("Codex 완료 이벤트에서 토큰 사용량을 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "turn.completed",
      usage: {
        input_tokens: 2_006,
        cached_input_tokens: 1_920,
        cache_write_input_tokens: 0,
        output_tokens: 300,
        reasoning_output_tokens: 17,
      },
    }),
    "codex",
  );

  assert.deepEqual(event.usage, {
    inputTokens: 2_006,
    outputTokens: 300,
    cachedInputTokens: 1_920,
    cacheWriteInputTokens: 0,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: 17,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  });
});

test("Claude 결과에서 전체 query pipeline 사용량과 비용을 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "result",
      subtype: "success",
      session_id: "session-1",
      result: "완료했습니다.",
      total_cost_usd: 0.00123,
      usage: {
        input_tokens: 2,
        cache_creation_input_tokens: 30,
        cache_read_input_tokens: 100,
        output_tokens: 4,
        service_tier: "standard",
        speed: "fast",
        inference_geo: "global",
        cache_creation: {
          ephemeral_5m_input_tokens: 20,
          ephemeral_1h_input_tokens: 10,
        },
      },
      modelUsage: {
        "claude-sonnet-5": {
          inputTokens: 2,
          outputTokens: 4,
          cacheReadInputTokens: 100,
          cacheCreationInputTokens: 30,
          costUSD: 0.001,
        },
        "claude-haiku-4-5": {
          inputTokens: 3,
          outputTokens: 5,
          cacheReadInputTokens: 200,
          cacheCreationInputTokens: 40,
          costUSD: 0.00023,
        },
      },
    }),
    "claude",
  );

  assert.equal(event.sessionID, "session-1");
  assert.equal(event.responseText, "완료했습니다.");
  assert.deepEqual(event.usage, {
    inputTokens: 5,
    outputTokens: 9,
    cachedInputTokens: 300,
    cacheWriteInputTokens: 70,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: null,
    serviceTier: "standard",
    speed: "fast",
    inferenceGeo: "global",
    reportedCostUsd: 0.00123,
    reportedSonnet5CostUsd: 0.001,
  });
});

test("Codex 공개 진행 설명은 최종 응답 후보와 분리한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "message-1",
        type: "agent_message",
        text: "현재 구조를 확인하고 있습니다.",
      },
    }),
    "codex",
  );

  assert.deepEqual(event, {
    agentMessage: "현재 구조를 확인하고 있습니다.",
    agentMessageKey: "message-1",
  });
});

test("Codex 명령은 민감 인자를 숨기고 실행 대상을 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.started",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "deploy --token secret-value",
      },
    }),
    "codex",
  );

  assert.equal(
    event.activity.text,
    "deploy [민감 인자 숨김]",
  );
  assert.equal(event.activity.status, "running");
  assert.equal(event.activity.eventKey, "command-1");
  assert.equal(event.activity.text.includes("secret-value"), false);
});

test("Codex 명령 완료는 종료 코드와 안전한 명령을 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "swift test --filter ParserTests",
        exit_code: 0,
      },
    }),
    "codex",
  );

  assert.equal(
    event.activity.text,
    "swift test --filter ParserTests",
  );
  assert.equal(event.activity.status, "completed");
  assert.equal(event.activity.eventKey, "command-1");
});

test("Codex 파일 변경은 변경 종류와 경로를 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "files-1",
        type: "file_change",
        changes: [
          { kind: "update", path: "Sources/OfficeGame/Feed.swift" },
          { kind: "add", path: "Tests/FeedTests.swift" },
        ],
      },
    }),
    "codex",
  );

  assert.equal(
    event.activity.text,
    [
      "파일 2개를 편집했습니다",
      "수정 Sources/OfficeGame/Feed.swift",
      "추가 Tests/FeedTests.swift",
    ].join("\n"),
  );
  assert.equal(event.activity.status, "completed");
});

test("Codex 협업 요청은 검토자별 구조화 활동으로 변환한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "spawn-1",
        type: "collab_tool_call",
        tool: "spawn_agent",
        sender_thread_id: "root-thread",
        receiver_thread_ids: ["reviewer-1"],
        prompt: "스크롤 정책을 독립적으로 검토해 주세요.",
        agents_states: {
          "reviewer-1": { status: "running", message: null },
        },
        status: "completed",
      },
    }),
    "codex",
  );

  assert.deepEqual(event.activities, [{
    kind: "collaboration",
    text: "스크롤 정책을 독립적으로 검토해 주세요.",
    eventKey: "collaboration:spawn-1:reviewer-1",
    status: "running",
    preserveText: false,
    messageScoped: false,
    collaboration: {
      action: "spawn",
      agentThreadId: "reviewer-1",
      agentLabel: null,
      prompt: "스크롤 정책을 독립적으로 검토해 주세요.",
      agentStatus: "running",
    },
  }]);
});

test("Codex 협업 대기는 완료된 검토 결과만 공개한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "wait-1",
        type: "collab_tool_call",
        tool: "wait",
        receiver_thread_ids: ["reviewer-1", "reviewer-2"],
        agents_states: {
          "reviewer-1": {
            status: "completed",
            message: "스크롤 위치 보존에 회귀 위험이 있습니다.",
          },
          "reviewer-2": { status: "running", message: null },
        },
        status: "completed",
      },
    }),
    "codex",
  );

  assert.equal(event.activities.length, 1);
  assert.equal(event.activities[0].status, "completed");
  assert.equal(
    event.activities[0].collaboration.agentThreadId,
    "reviewer-1",
  );
  assert.equal(
    event.activities[0].collaboration.message,
    "스크롤 위치 보존에 회귀 위험이 있습니다.",
  );
});

test("Claude의 네이티브 다음 질문 추천을 공개 활동으로 변환한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "prompt_suggestion",
      suggestion: "이 변경의 테스트 결과도 확인할까요?",
      uuid: "suggestion-1",
      session_id: "session-1",
    }),
    "claude",
  );

  assert.deepEqual(event.activity, {
    kind: "suggestion",
    text: "이 변경의 테스트 결과도 확인할까요?",
    eventKey: "suggestion:suggestion-1",
    status: "completed",
    preserveText: false,
    messageScoped: false,
  });
});

test("Codex에는 존재하지 않는 다음 질문 이벤트를 만들어내지 않는다", () => {
  assert.equal(
    parseAgentEvent(
      JSON.stringify({
        type: "prompt_suggestion",
        suggestion: "가짜 추천",
      }),
      "codex",
    ),
    null,
  );
});

test("Codex 협업 추가 지시는 해당 검토자의 후속 요청으로 보존한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "input-1",
        type: "collab_tool_call",
        tool: "send_input",
        receiver_thread_ids: ["reviewer-1"],
        prompt: "CPU 측정값도 함께 확인해 주세요.",
        status: "completed",
      },
    }),
    "codex",
  );

  assert.equal(event.activities[0].status, "running");
  assert.deepEqual(event.activities[0].collaboration, {
    action: "follow_up",
    agentThreadId: "reviewer-1",
    agentLabel: null,
    prompt: "CPU 측정값도 함께 확인해 주세요.",
    agentStatus: "running",
  });
});

test("Codex 협업의 대기 시작과 종료 정리는 화면 활동으로 만들지 않는다", () => {
  for (const object of [
    {
      type: "item.started",
      item: {
        id: "wait-1",
        type: "collab_tool_call",
        tool: "wait",
        status: "in_progress",
      },
    },
    {
      type: "item.completed",
      item: {
        id: "close-1",
        type: "collab_tool_call",
        tool: "close_agent",
        receiver_thread_ids: ["reviewer-1"],
        status: "completed",
      },
    },
  ]) {
    assert.equal(parseAgentEvent(JSON.stringify(object), "codex"), null);
  }
});

test("긴 절대 변경 경로는 업무 폴더 상대경로로 보존한다", () => {
  const workdir = [
    "/Users/example/.officestra/worktrees/03ffd78858a8",
    "right-woman-70d95def-eae4-441f-b0ec-e5cd2e230dee",
  ].join("/");
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "files-long-path",
        type: "file_change",
        changes: [{
          kind: "update",
          path: `${workdir}/Sources/OfficeGame/AgentDirector.swift`,
        }],
      },
    }),
    "codex",
    workdir,
  );

  assert.equal(
    event.activity.text,
    [
      "파일 1개를 편집했습니다",
      "수정 Sources/OfficeGame/AgentDirector.swift",
    ].join("\n"),
  );
});

test("업무 폴더 안의 긴 상대경로는 축약하지 않는다", () => {
  const workdir = "/Users/example/office";
  const path = [
    "packages",
    ...Array.from({ length: 10 }, (_, index) => `nested-${index}`),
    "Sources",
    "Feature.swift",
  ].join("/");
  assert.ok(path.length > 96);

  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "files-long-relative-path",
        type: "file_change",
        changes: [{ kind: "update", path }],
      },
    }),
    "codex",
    workdir,
  );

  assert.equal(
    event.activity.text,
    `파일 1개를 편집했습니다\n수정 ${path}`,
  );
});

test("업무 폴더 밖의 긴 절대경로는 상대경로로 바꾸지 않는다", () => {
  const workdir = "/Users/example/office";
  const outsidePath = [
    "/Users/example/another-worktree",
    ...Array.from({ length: 8 }, () => "very-long-segment"),
    "Sources",
    "Outside.swift",
  ].join("/");
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "files-outside-workdir",
        type: "file_change",
        changes: [{ kind: "update", path: outsidePath }],
      },
    }),
    "codex",
    workdir,
  );

  assert.equal(
    event.activity.text,
    [
      "파일 1개를 편집했습니다",
      "수정 …/very-long-segment/Sources/Outside.swift",
    ].join("\n"),
  );
});

test("Codex 파일 변경 시작은 실행 중 활동과 스냅샷 메타데이터를 만든다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.started",
      item: {
        id: "files-1",
        type: "file_change",
        changes: [{
          kind: "update",
          path: "Sources/OfficeGame/Feed.swift",
        }],
      },
    }),
    "codex",
  );

  assert.equal(event.activity.text, "파일 변경을 적용하는 중 · 1개");
  assert.equal(event.activity.status, "running");
  assert.deepEqual(event.fileChange, {
    eventKey: "files-1",
    phase: "item.started",
    changes: [{
      path: "Sources/OfficeGame/Feed.swift",
      kind: "update",
    }],
  });
});

test("Codex가 거절한 명령은 실패 활동으로 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "command-1",
        type: "command_execution",
        command: "rm protected.txt",
        status: "declined",
      },
    }),
    "codex",
  );

  assert.equal(event.activity.status, "failed");
});

test("Codex 항목 오류는 실패 활동으로 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.completed",
      item: {
        id: "error-1",
        type: "error",
        message: "도구 연결이 끊겼습니다.",
      },
    }),
    "codex",
  );

  assert.equal(event.activity.text, "오류 · 도구 연결이 끊겼습니다.");
  assert.equal(event.activity.status, "failed");
});

test("Codex MCP 호출은 서버와 도구명을 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "item.started",
      item: {
        id: "tool-1",
        type: "mcp_tool_call",
        server: "computer-use",
        actionName: "inspect",
      },
    }),
    "codex",
  );

  assert.equal(
    event.activity.text,
    "computer-use/inspect",
  );
  assert.equal(event.activity.status, "running");
  assert.equal(event.activity.eventKey, "tool-1");
});

test("Claude 공개 응답 조각을 실시간으로 추출한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "stream_event",
      event: {
        type: "content_block_delta",
        delta: {
          type: "text_delta",
          text: "현재 파일을 확인",
        },
      },
    }),
    "claude",
  );

  assert.equal(event.responseDelta, "현재 파일을 확인");
});

test("Claude가 제공한 thinking 원문을 완료 활동으로 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        id: "message-1",
        content: [{
          type: "thinking",
          thinking: "상자별 참과 거짓을 대조한다.",
        }],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    "상자별 참과 거짓을 대조한다.",
  );
  assert.equal(event.activities[0].status, "completed");
  assert.equal(event.activities[0].eventKey, "message-1:block:0");
});

test("Claude thinking 시작은 현재 메시지 범위의 실행 중 활동이다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "stream_event",
      event: {
        type: "content_block_start",
        index: 0,
        content_block: { type: "thinking", thinking: "" },
      },
    }),
    "claude",
  );

  assert.equal(event.activity.text, "추론 중");
  assert.equal(event.activity.status, "running");
  assert.equal(event.activity.eventKey, "block:0");
  assert.equal(event.activity.messageScoped, true);
});

test("Claude 도구는 이름과 안전한 파일 경로를 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [
          {
            type: "tool_use",
            name: "Read",
            input: { file_path: "Sources/OfficeGame/Feed.swift" },
          },
        ],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Read · Sources/OfficeGame/Feed.swift",
  );
  assert.equal(event.activities[0].status, "running");
});

test("Claude Bash 도구의 민감 인자는 노출하지 않는다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [
          {
            type: "tool_use",
            name: "Bash",
            input: { command: "curl --token secret-value example.com" },
          },
        ],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Bash · curl [민감 인자 숨김]",
  );
  assert.equal(
    event.activities[0].text.includes("secret-value"),
    false,
  );
});

test("Claude 계획 도구는 단계 진행과 항목을 함께 보존한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [
          {
            type: "tool_use",
            name: "TodoWrite",
            input: {
              todos: [
                { content: "활동 형식 확인", status: "completed" },
                { content: "타임라인 구현", status: "in_progress" },
                { content: "테스트 실행", status: "pending" },
              ],
            },
          },
        ],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    [
      "도구 · TodoWrite · 1/3단계",
      "[x] 활동 형식 확인",
      "[~] 타임라인 구현",
      "[ ] 테스트 실행",
    ].join("\n"),
  );
});

test("Claude 위임 도구는 담당 유형과 설명을 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [
          {
            type: "tool_use",
            name: "Task",
            input: {
              subagent_type: "Explore",
              description: "대화창 구현 위치 조사",
            },
          },
        ],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Task · Explore · 대화창 구현 위치 조사",
  );
});

test("Claude 노트북 편집도 안전한 경로를 표시한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [
          {
            type: "tool_use",
            name: "NotebookEdit",
            input: { notebook_path: "analysis/report.ipynb" },
          },
        ],
      },
    }),
    "claude",
  );

  assert.equal(
    event.activities[0].text,
    "도구 · NotebookEdit · analysis/report.ipynb",
  );
});

test("Claude 편집 경로도 업무 폴더 상대경로로 보존한다", () => {
  const workdir = [
    "/Users/example/.officestra/worktrees/03ffd78858a8",
    "left-woman-f2b5a998-f546-42db-b447-cf74977e810c",
  ].join("/");
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{
          type: "tool_use",
          name: "Edit",
          input: {
            file_path: `${workdir}/Sources/OfficeGame/OfficeGameApp.swift`,
          },
        }],
      },
    }),
    "claude",
    workdir,
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Edit · Sources/OfficeGame/OfficeGameApp.swift",
  );
});

test("Claude 편집 도구는 입력에서 센 줄 수를 둘째 줄로 붙인다", () => {
  const workdir = "/Users/example/office";
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{
          type: "tool_use",
          name: "Edit",
          input: {
            file_path: `${workdir}/Sources/OfficeGame/AgentDirector.swift`,
            old_string: "let a = 1\nlet b = 2",
            new_string: "let a = 1\nlet b = 3\nlet c = 4",
          },
        }],
      },
    }),
    "claude",
    workdir,
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Edit · Sources/OfficeGame/AgentDirector.swift\n+3 -2",
  );
});

test("Claude MultiEdit는 편집 묶음 전체 줄 수를 합산한다", () => {
  const workdir = "/Users/example/office";
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{
          type: "tool_use",
          name: "MultiEdit",
          input: {
            file_path: `${workdir}/backend/src/server.mjs`,
            edits: [
              { old_string: "a", new_string: "a\nb" },
              { old_string: "c\nd", new_string: "e" },
            ],
          },
        }],
      },
    }),
    "claude",
    workdir,
  );

  assert.equal(
    event.activities[0].text,
    "도구 · MultiEdit · backend/src/server.mjs\n+3 -3",
  );
});

test("Claude Write는 이전 내용을 모르므로 추가 줄만 센다", () => {
  const workdir = "/Users/example/office";
  const event = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{
          type: "tool_use",
          name: "Write",
          input: {
            file_path: `${workdir}/docs/새문서.md`,
            content: "제목\n본문\n끝",
          },
        }],
      },
    }),
    "claude",
    workdir,
  );

  assert.equal(
    event.activities[0].text,
    "도구 · Write · docs/새문서.md\n+3 -0",
  );
});

test("Claude 비편집 도구와 통계 없는 편집은 첫 줄 형식을 그대로 둔다", () => {
  const workdir = "/Users/example/office";
  const read = parseAgentEvent(
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{
          type: "tool_use",
          name: "Read",
          input: { file_path: `${workdir}/README.md` },
        }],
      },
    }),
    "claude",
    workdir,
  );
  assert.equal(read.activities[0].text, "도구 · Read · README.md");

  const partialEdit = parseAgentEvent(
    JSON.stringify({
      type: "stream_event",
      event: {
        type: "content_block_start",
        content_block: {
          id: "tool-stream-edit",
          type: "tool_use",
          name: "Edit",
          input: { file_path: `${workdir}/README.md` },
        },
      },
    }),
    "claude",
    workdir,
  );
  assert.equal(partialEdit.activity.text, "도구 · Edit · README.md");
});

test("Claude 스트리밍 편집 경로도 업무 폴더 상대경로로 보존한다", () => {
  const workdir = "/Users/example/office";
  const event = parseAgentEvent(
    JSON.stringify({
      type: "stream_event",
      event: {
        type: "content_block_start",
        content_block: {
          id: "tool-stream-edit",
          type: "tool_use",
          name: "Edit",
          input: {
            file_path: `${workdir}/Sources/OfficeGame/AgentDirector.swift`,
          },
        },
      },
    }),
    "claude",
    workdir,
  );

  assert.equal(
    event.activity.text,
    "도구 · Edit · Sources/OfficeGame/AgentDirector.swift",
  );
});

test("Claude 도구 결과는 같은 도구 행을 완료 상태로 갱신한다", () => {
  const event = parseAgentEvent(
    JSON.stringify({
      type: "user",
      message: {
        content: [{
          type: "tool_result",
          tool_use_id: "tool-1",
          content: "완료",
        }],
      },
    }),
    "claude",
  );

  assert.equal(event.activities[0].eventKey, "tool-1");
  assert.equal(event.activities[0].status, "completed");
  assert.equal(event.activities[0].preserveText, true);
});

test("사용자 확인 표식을 질문 상태로 분리한다", () => {
  assert.deepEqual(
    decodeAgentResponse("[NEED_INPUT]\n어느 색으로 할까요?"),
    {
      text: "어느 색으로 할까요?",
      needsInput: true,
      sources: [],
      proposals: [],
      wikiProposalError: null,
    },
  );
});

test("설명 뒤의 단독 사용자 확인 표식도 질문 상태로 분리한다", () => {
  const decoded = decodeAgentResponse(`확인한 내용을 먼저 설명합니다.

[NEED_INPUT]
어느 색으로 할까요?

[OFFICE_SOURCES]
[{"kind":"file","title":"설정","locator":"config.json:3"}]`);

  assert.equal(
    decoded.text,
    "확인한 내용을 먼저 설명합니다.\n\n어느 색으로 할까요?",
  );
  assert.equal(decoded.needsInput, true);
  assert.equal(decoded.sources.length, 1);
  assert.equal(decoded.sources[0].locator, "config.json:3");
});

test("여러 단독 사용자 확인 표식 중 마지막 줄만 제거한다", () => {
  const decoded = decodeAgentResponse(`첫 번째 표식입니다.
[NEED_INPUT]
설명을 이어갑니다.
[NEED_INPUT]
최종 질문입니다.`);

  assert.equal(
    decoded.text,
    "첫 번째 표식입니다.\n[NEED_INPUT]\n설명을 이어갑니다.\n최종 질문입니다.",
  );
  assert.equal(decoded.needsInput, true);
});

test("문장 안의 사용자 확인 표식은 질문 상태로 해석하지 않는다", () => {
  const text = "형식 예시는 [NEED_INPUT]처럼 작성합니다.";

  assert.deepEqual(decodeAgentResponse(text), {
    text,
    needsInput: false,
    sources: [],
    proposals: [],
    wikiProposalError: null,
  });
});

test("응답 끝의 출처 블록을 본문과 분리한다", () => {
  assert.deepEqual(
    decodeAgentResponse(`완료했습니다.

[OFFICE_SOURCES]
[{"kind":"file","title":"설정","locator":"/repo/config.json:3"},{"kind":"database","title":"업무 기록","locator":"work_records/5e7aa706-3c02-4d7a-8d9d-bfe735731fcb","workRecordId":"5e7aa706-3c02-4d7a-8d9d-bfe735731fcb","excerpt":"세션 유지"}]`),
    {
      text: "완료했습니다.",
      needsInput: false,
      proposals: [],
      wikiProposalError: null,
      sources: [
        {
          ordinal: 0,
          sourceKind: "file",
          title: "설정",
          locator: "/repo/config.json:3",
          excerpt: null,
          ragDocumentID: null,
          workRecordID: null,
          metadata: {},
        },
        {
          ordinal: 1,
          sourceKind: "database",
          title: "업무 기록",
          locator: "work_records/5e7aa706-3c02-4d7a-8d9d-bfe735731fcb",
          excerpt: "세션 유지",
          ragDocumentID: null,
          workRecordID: "5e7aa706-3c02-4d7a-8d9d-bfe735731fcb",
          metadata: {},
        },
      ],
    },
  );
});

test("웹과 도구와 스킬 출처를 본문과 분리한다", () => {
  assert.deepEqual(
    decodeAgentResponse(`확인했습니다.

[OFFICE_SOURCES]
[{"kind":"web","title":"공식 문서","locator":"https://example.com/docs"},{"kind":"tool","title":"웹 조회","locator":"web/search_query"},{"kind":"skill","title":"브라우저 절차","locator":"playwright-cli"}]`),
    {
      text: "확인했습니다.",
      needsInput: false,
      proposals: [],
      wikiProposalError: null,
      sources: [
        {
          ordinal: 0,
          sourceKind: "web",
          title: "공식 문서",
          locator: "https://example.com/docs",
          excerpt: null,
          ragDocumentID: null,
          workRecordID: null,
          metadata: {},
        },
        {
          ordinal: 1,
          sourceKind: "tool",
          title: "웹 조회",
          locator: "web/search_query",
          excerpt: null,
          ragDocumentID: null,
          workRecordID: null,
          metadata: {},
        },
        {
          ordinal: 2,
          sourceKind: "skill",
          title: "브라우저 절차",
          locator: "playwright-cli",
          excerpt: null,
          ragDocumentID: null,
          workRecordID: null,
          metadata: {},
        },
      ],
    },
  );
});

test("잘못된 출처 블록도 기계 판독용 내용을 화면에서 숨긴다", () => {
  const response = "완료했습니다.\n[OFFICE_SOURCES]\nnot-json";
  assert.deepEqual(decodeAgentResponse(response), {
    text: "완료했습니다.",
    needsInput: false,
    sources: [],
    proposals: [],
    wikiProposalError: null,
    sourceError: "응답 근거 형식을 읽지 못했습니다.",
  });
});

test("검증에 걸린 출처 블록은 어긴 규칙을 그대로 알려 준다", () => {
  const duplicated = JSON.stringify([
    { kind: "tool", title: "조회", locator: "api/live-feed", excerpt: "가" },
    { kind: "tool", title: "재조회", locator: "api/live-feed", excerpt: "나" },
  ]);
  const response = `완료했습니다.\n[OFFICE_SOURCES]\n${duplicated}`;

  // "형식을 읽지 못했다"로 뭉뚱그리면 직원이 무엇을 고칠지 알 수 없다.
  assert.equal(
    decodeAgentResponse(response).sourceError,
    "응답 근거를 저장하지 못했습니다. 같은 출처가 중복됐습니다.",
  );
  assert.deepEqual(decodeAgentResponse(response).sources, []);
});

test("출처 블록 뒤에 글이 더 있으면 자리를 잘못 잡았다고 알린다", () => {
  const response = [
    "[OFFICE_SOURCES]",
    '[{"kind":"file","title":"화면","locator":"a.swift","excerpt":"완료"}]',
    "빌드까지 모두 끝냈습니다.",
  ].join("\n");

  // 조용히 지나가면 근거가 사라진 채 마커와 JSON이 화면에 그대로 남는다.
  const decoded = decodeAgentResponse(response);
  assert.equal(
    decoded.sourceError,
    "응답 근거 블록은 응답 맨 끝에 두고 뒤에 아무것도 쓰지 마십시오.",
  );
  assert.deepEqual(decoded.sources, []);
});

test("같은 내용도 블록을 맨 끝에 두면 그대로 저장한다", () => {
  const response = [
    "빌드까지 모두 끝냈습니다.",
    "",
    "[OFFICE_SOURCES]",
    '[{"kind":"file","title":"화면","locator":"a.swift","excerpt":"완료"}]',
  ].join("\n");

  const decoded = decodeAgentResponse(response);
  assert.equal(decoded.sourceError, undefined);
  assert.equal(decoded.sources.length, 1);
  assert.equal(decoded.text, "빌드까지 모두 끝냈습니다.");
});

test("정상 블록이 있으면 본문에 적어 둔 형식 설명은 경고하지 않는다", () => {
  const response = [
    "형식은 이렇습니다.",
    "",
    "[OFFICE_SOURCES]",
    '[{"kind":"file","title":"예시","locator":"a.mjs","excerpt":"예"}]',
    "",
    "위처럼 쓰시면 됩니다.",
    "",
    "[OFFICE_SOURCES]",
    '[{"kind":"file","title":"실제","locator":"b.mjs","excerpt":"실"}]',
  ].join("\n");

  const decoded = decodeAgentResponse(response);
  assert.equal(decoded.sourceError, undefined);
  assert.equal(decoded.sources.length, 1);
  assert.equal(decoded.sources[0].locator, "b.mjs");
});

test("위키 수정안 블록도 뒤에 글이 붙으면 자리를 알려 준다", () => {
  const response = [
    "[OFFICE_WIKI_PROPOSALS]",
    '[{"pageKey":"a","kind":"decision","title":"제목","body":"본문",'
      + '"approvalTier":"user"}]',
    "확인 부탁드립니다.",
  ].join("\n");

  assert.equal(
    decodeAgentResponse(response).wikiProposalError,
    "위키 수정안 블록은 응답 맨 끝에 두고 뒤에 아무것도 쓰지 마십시오.",
  );
});

test("일반 응답은 빈 위키 수정안 계약 외에는 그대로 유지한다", () => {
  assert.deepEqual(decodeAgentResponse("일반 답변입니다."), {
    text: "일반 답변입니다.",
    needsInput: false,
    sources: [],
    proposals: [],
    wikiProposalError: null,
  });
});

test("응답 끝의 위키 수정안을 검증해 본문과 분리한다", () => {
  assert.deepEqual(
    decodeAgentResponse(`정책을 정리했습니다.

[OFFICE_WIKI_PROPOSALS]
[{"pageKey":"release-policy","kind":"decision","title":"릴리스 승인 원칙","body":"배포 전 사용자 승인을 받습니다.","approvalTier":"user"},{"pageKey":"secret-handling","kind":"constraint","title":"비밀값 저장 금지","body":"토큰은 저장소 밖에 둡니다.","approvalTier":"peer"}]`),
    {
      text: "정책을 정리했습니다.",
      needsInput: false,
      sources: [],
      proposals: [
        {
          pageKey: "release-policy",
          kind: "decision",
          title: "릴리스 승인 원칙",
          body: "배포 전 사용자 승인을 받습니다.",
          approvalTier: "user",
        },
        {
          pageKey: "secret-handling",
          kind: "constraint",
          title: "비밀값 저장 금지",
          body: "토큰은 저장소 밖에 둡니다.",
          approvalTier: "peer",
        },
      ],
      wikiProposalError: null,
    },
  );
});

test("권장 기계 블록 순서는 NEED_INPUT 본문, 위키 수정안, 응답 근거다", () => {
  const decoded = decodeAgentResponse(`[NEED_INPUT]
이 원칙을 위키에 반영할까요?

[OFFICE_WIKI_PROPOSALS]
[{"pageKey":"deploy-approval","kind":"decision","title":"배포 승인","body":"사용자 승인 뒤 배포합니다.","approvalTier":"user"}]

[OFFICE_SOURCES]
[{"kind":"file","title":"배포 규칙","locator":"README.md:10"}]`);

  assert.equal(decoded.text, "이 원칙을 위키에 반영할까요?");
  assert.equal(decoded.needsInput, true);
  assert.deepEqual(decoded.proposals, [{
    pageKey: "deploy-approval",
    kind: "decision",
    title: "배포 승인",
    body: "사용자 승인 뒤 배포합니다.",
    approvalTier: "user",
  }]);
  assert.equal(decoded.wikiProposalError, null);
  assert.equal(decoded.sources.length, 1);
  assert.equal(decoded.sources[0].locator, "README.md:10");
});

test("위키 수정안과 응답 근거의 역순도 각각 안전하게 분리한다", () => {
  const decoded = decodeAgentResponse(`확인했습니다.

[OFFICE_SOURCES]
[{"kind":"file","title":"장애 기록","locator":"incidents.md:4"}]

[OFFICE_WIKI_PROPOSALS]
[{"pageKey":"incident-queue","kind":"incident","title":"작업 대기열 장애","body":"대기열 복구 절차를 기록합니다.","approvalTier":"peer"}]`);

  assert.equal(decoded.text, "확인했습니다.");
  assert.equal(decoded.sources[0].locator, "incidents.md:4");
  assert.deepEqual(decoded.proposals, [{
    pageKey: "incident-queue",
    kind: "incident",
    title: "작업 대기열 장애",
    body: "대기열 복구 절차를 기록합니다.",
    approvalTier: "peer",
  }]);
  assert.equal(decoded.wikiProposalError, null);
});

test("끝이 아닌 위키 수정안 표식은 일반 본문으로 보존한다", () => {
  const response = `형식 예시입니다.
[OFFICE_WIKI_PROPOSALS]
[]
이 문장은 블록 뒤의 일반 설명입니다.`;

  assert.deepEqual(decodeAgentResponse(response), {
    text: response,
    needsInput: false,
    sources: [],
    proposals: [],
    wikiProposalError: null,
  });
});

test("잘못된 위키 수정안은 본문을 보존하고 임의 제안을 만들지 않는다", () => {
  assert.deepEqual(
    decodeAgentResponse(`정상 답변입니다.

[OFFICE_WIKI_PROPOSALS]
not-json`),
    {
      text: "정상 답변입니다.",
      needsInput: false,
      sources: [],
      proposals: [],
      wikiProposalError: "위키 수정안 형식을 읽지 못했습니다.",
    },
  );
});

test("잘못된 위키 수정안과 정상 응답 근거는 서로의 결과를 오염시키지 않는다", () => {
  for (const suffix of [
    `[OFFICE_WIKI_PROPOSALS]\nnot-json\n[OFFICE_SOURCES]\n[{"kind":"file","title":"정상 근거","locator":"README.md:3"}]`,
    `[OFFICE_SOURCES]\n[{"kind":"file","title":"정상 근거","locator":"README.md:3"}]\n[OFFICE_WIKI_PROPOSALS]\nnot-json`,
  ]) {
    const decoded = decodeAgentResponse(`[NEED_INPUT]\n승인할까요?\n${suffix}`);
    assert.equal(decoded.text, "승인할까요?");
    assert.equal(decoded.needsInput, true);
    assert.equal(decoded.sources.length, 1);
    assert.equal(decoded.sources[0].locator, "README.md:3");
    assert.deepEqual(decoded.proposals, []);
    assert.equal(
      decoded.wikiProposalError,
      "위키 수정안 형식을 읽지 못했습니다.",
    );
  }
});

test("위키 수정안은 최대 3건과 각 필드 경계를 허용한다", () => {
  const proposals = Array.from({ length: 3 }, (_, index) => ({
    pageKey: `${index}${"a".repeat(79)}`,
    kind: ["decision", "constraint", "incident"][index],
    title: "가".repeat(120),
    body: "나".repeat(12_000),
    approvalTier: index === 1 ? "peer" : "user",
  }));
  const decoded = decodeAgentResponse(`완료
[OFFICE_WIKI_PROPOSALS]
${JSON.stringify(proposals)}`);

  assert.deepEqual(decoded.proposals, proposals);
  assert.equal(decoded.wikiProposalError, null);
  assert.equal(decoded.text, "완료");
});

test("위키 수정안 제한 위반은 배열 전체를 거절한다", () => {
  const valid = {
    pageKey: "safe-page",
    kind: "decision",
    title: "안전한 제목",
    body: "안전한 본문",
    approvalTier: "peer",
  };
  const invalidCases = [
    [valid, valid, valid, valid],
    [{ ...valid, pageKey: "Uppercase" }],
    [{ ...valid, pageKey: `a${"b".repeat(80)}` }],
    [{ ...valid, kind: "note" }],
    [{ ...valid, title: "가".repeat(121) }],
    [{ ...valid, body: "나".repeat(12_001) }],
    [{ ...valid, approvalTier: "system" }],
    [{ ...valid, sourceTurnId: "직원이-만든-ID" }],
    [{
      pageKey: valid.pageKey,
      kind: valid.kind,
      title: valid.title,
      approvalTier: valid.approvalTier,
    }],
  ];

  for (const proposals of invalidCases) {
    const decoded = decodeAgentResponse(`답변 보존
[OFFICE_WIKI_PROPOSALS]
${JSON.stringify(proposals)}`);
    assert.equal(decoded.text, "답변 보존");
    assert.deepEqual(decoded.proposals, []);
    assert.equal(
      decoded.wikiProposalError,
      "위키 수정안 형식을 읽지 못했습니다.",
    );
  }
});

test("중복 위키 수정안 블록은 부분 채택 없이 거절한다", () => {
  const proposal = JSON.stringify([{
    pageKey: "one-page",
    kind: "decision",
    title: "한 건",
    body: "한 건만 허용합니다.",
    approvalTier: "peer",
  }]);
  const decoded = decodeAgentResponse(`답변
[OFFICE_WIKI_PROPOSALS]
${proposal}
[OFFICE_WIKI_PROPOSALS]
${proposal}`);

  assert.equal(decoded.text, "답변");
  assert.deepEqual(decoded.proposals, []);
  assert.equal(
    decoded.wikiProposalError,
    "위키 수정안 블록은 하나만 사용할 수 있습니다.",
  );
});

test("Claude 시작 이벤트와 본 대화 응답에서 실제 모델 ID를 읽는다", () => {
  const init = parseAgentEvent(JSON.stringify({
    type: "system",
    subtype: "init",
    session_id: "session-1",
    model: "claude-opus-5-5",
  }), "claude");
  assert.equal(init.sessionID, "session-1");
  assert.equal(init.resolvedModel, "claude-opus-5-5");

  const assistant = (extra) => parseAgentEvent(JSON.stringify({
    type: "assistant",
    message: { model: "claude-sonnet-5", content: [{ type: "text", text: "확인" }] },
    ...extra,
  }), "claude");
  assert.equal(assistant({}).resolvedModel, "claude-sonnet-5");
  // 하위 에이전트 응답과 CLI가 만든 응답은 턴 모델로 쓰지 않는다.
  assert.equal(assistant({ parent_tool_use_id: "tool-1" })?.resolvedModel, undefined);
  assert.equal(parseAgentEvent(JSON.stringify({
    type: "assistant",
    message: { model: "<synthetic>", content: [{ type: "text", text: "중단" }] },
  }), "claude")?.resolvedModel, undefined);
});
