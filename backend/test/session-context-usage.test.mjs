// 이 파일은 CLI 기록에서 대화 시점의 컨텍스트 사용량을 뽑아내는 동작을 검증한다.

import assert from "node:assert/strict";
import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  claudeContextEntry,
  claudeContextWindow,
  codexContextEntry,
  sessionContextUsage,
} from "../src/session-context-usage.mjs";

function claudeAssistantLine({
  timestamp,
  input,
  cacheRead,
  cacheWrite,
  isSidechain = false,
}) {
  return JSON.stringify({
    type: "assistant",
    isSidechain,
    timestamp,
    message: {
      model: "claude-opus-5",
      usage: {
        input_tokens: input,
        cache_read_input_tokens: cacheRead,
        cache_creation_input_tokens: cacheWrite,
        output_tokens: 100,
      },
    },
  });
}

function codexTokenCountLine({ timestamp, lastInput, window }) {
  return JSON.stringify({
    timestamp,
    type: "event_msg",
    payload: {
      type: "token_count",
      info: {
        total_token_usage: { input_tokens: 999_999, output_tokens: 1 },
        last_token_usage: { input_tokens: lastInput, output_tokens: 1 },
        model_context_window: window,
      },
    },
  });
}

test("Claude 기록은 입력과 캐시를 합쳐 컨텍스트 점유로 계산한다", () => {
  const entry = claudeContextEntry(
    claudeAssistantLine({
      timestamp: "2026-07-31T19:40:22.710Z",
      input: 1,
      cacheRead: 37_274,
      cacheWrite: 231_269,
    }),
  );

  assert.equal(entry.usedTokens, 268_544);
  assert.equal(entry.limitTokens, 1_000_000);
});

test("Claude 압축 경계는 압축 뒤 컨텍스트 점유를 반영한다", () => {
  const entry = claudeContextEntry(JSON.stringify({
    type: "system",
    subtype: "compact_boundary",
    timestamp: "2026-08-20T01:02:03.000Z",
    compact_metadata: {
      pre_tokens: 905_000,
      post_tokens: 47_500,
    },
  }));

  assert.equal(entry.usedTokens, 47_500);
  assert.equal(entry.limitTokens, null);
  assert.equal(entry.at, Date.parse("2026-08-20T01:02:03.000Z"));
});

test("Claude 부속 대화와 사용량 없는 줄은 건너뛴다", () => {
  assert.equal(
    claudeContextEntry(
      claudeAssistantLine({
        timestamp: "2026-07-31T19:40:22.710Z",
        input: 1,
        cacheRead: 10,
        cacheWrite: 0,
        isSidechain: true,
      }),
    ),
    null,
  );
  assert.equal(
    claudeContextEntry(
      JSON.stringify({ type: "user", timestamp: "2026-07-31T19:40:00.000Z" }),
    ),
    null,
  );
});

test("Codex 기록은 마지막 요청 입력과 모델 한도를 사용한다", () => {
  const entry = codexContextEntry(
    codexTokenCountLine({
      timestamp: "2026-08-01T05:05:06.005Z",
      lastInput: 57_218,
      window: 258_400,
    }),
  );

  assert.equal(entry.usedTokens, 57_218);
  assert.equal(entry.limitTokens, 258_400);
});

test("Antigravity 세션은 로컬 대화 메타데이터의 실제 한도를 사용한다", () => {
  let request;
  const result = sessionContextUsage({
    backend: "antigravity",
    sessionID: "11111111-2222-4333-8444-555555555555",
    model: "gemini-3.7-flash",
    at: "2026-08-28T00:00:00Z",
    antigravityRoot: "/tmp/antigravity-conversations",
    antigravityUsageReader: (options) => {
      request = options;
      return { usedTokens: 58_559, limitTokens: 256_000 };
    },
  });

  assert.deepEqual(result, { usedTokens: 58_559, limitTokens: 256_000 });
  assert.equal(
    request.sessionID,
    "11111111-2222-4333-8444-555555555555",
  );
  assert.equal(request.root, "/tmp/antigravity-conversations");
  assert.equal(request.at, Date.parse("2026-08-28T00:00:00Z"));
});

test("누적 사용량은 컨텍스트 점유로 오인하지 않는다", () => {
  const entry = codexContextEntry(
    codexTokenCountLine({
      timestamp: "2026-08-01T05:05:06.005Z",
      lastInput: 38_195,
      window: 258_400,
    }),
  );

  assert.notEqual(entry.usedTokens, 999_999);
});

test("Claude 모델 문자열로 컨텍스트 한도를 찾는다", () => {
  assert.equal(claudeContextWindow("fable"), 1_000_000);
  assert.equal(claudeContextWindow("claude-opus-5"), 1_000_000);
  assert.equal(claudeContextWindow("claude-sonnet-5"), 1_000_000);
  assert.equal(claudeContextWindow("claude-haiku-4-5-20251001"), 200_000);
  assert.equal(claudeContextWindow("gpt-5.6-sol"), null);
  assert.equal(claudeContextWindow(null), null);
});

test("Claude 세션은 모델 기본 컨텍스트 한도를 사용한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-claude-"));
  try {
    const project = join(root, "-Users-neo-office");
    mkdirSync(project);
    const sessionID = "2206f58e-0bd8-43dd-8068-780090adbaa8";
    writeFileSync(
      join(project, `${sessionID}.jsonl`),
      [
        claudeAssistantLine({
          timestamp: "2026-07-31T19:30:00.000Z",
          input: 2,
          cacheRead: 36_310,
          cacheWrite: 964,
        }),
        claudeAssistantLine({
          timestamp: "2026-07-31T19:40:22.710Z",
          input: 1,
          cacheRead: 37_274,
          cacheWrite: 231_269,
        }),
        claudeAssistantLine({
          timestamp: "2026-07-31T20:10:00.000Z",
          input: 5,
          cacheRead: 900_000,
          cacheWrite: 0,
        }),
      ].join("\n") + "\n",
    );

    assert.deepEqual(
      sessionContextUsage({
        backend: "claude",
        sessionID,
        model: "claude-opus-5",
        at: "2026-07-31T19:40:23.259Z",
        claudeRoot: root,
      }),
      { usedTokens: 268_544, limitTokens: 1_000_000 },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Claude 세션은 설정이 별칭이어도 기록된 모델 ID로 한도를 찾는다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-claude-alias-"));
  try {
    const project = join(root, "-Users-neo-office");
    mkdirSync(project);
    const sessionID = "dcff4a46-7a34-459a-baa8-8ee75956377f";
    writeFileSync(
      join(project, `${sessionID}.jsonl`),
      [
        claudeAssistantLine({
          timestamp: "2026-09-27T10:00:00.000Z",
          input: 3,
          cacheRead: 120_000,
          cacheWrite: 500,
        }),
        JSON.stringify({
          type: "system",
          subtype: "compact_boundary",
          timestamp: "2026-09-27T11:00:00.000Z",
          compact_metadata: { pre_tokens: 800_000, post_tokens: 40_000 },
        }),
      ].join("\n") + "\n",
    );
    const usage = (at) => sessionContextUsage({
      backend: "claude",
      sessionID,
      model: "opus",
      at,
      claudeRoot: root,
    });

    assert.deepEqual(
      usage("2026-09-27T10:30:00.000Z"),
      { usedTokens: 120_503, limitTokens: 1_000_000 },
    );
    assert.deepEqual(
      usage("2026-09-27T11:30:00.000Z"),
      { usedTokens: 40_000, limitTokens: 1_000_000 },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Claude 세션은 중복 기록 중 마지막으로 쓰인 경로를 다시 고른다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-claude-duplicates-"));
  try {
    const staleProject = join(root, "a-stale");
    const activeProject = join(root, "z-active");
    mkdirSync(staleProject);
    mkdirSync(activeProject);
    const sessionID = "2206f58e-0bd8-43dd-8068-780090adbaa8";
    const stalePath = join(staleProject, `${sessionID}.jsonl`);
    const activePath = join(activeProject, `${sessionID}.jsonl`);
    writeFileSync(
      stalePath,
      claudeAssistantLine({
        timestamp: "2026-07-31T19:30:00.000Z",
        input: 2,
        cacheRead: 100,
        cacheWrite: 0,
      }) + "\n" + "x".repeat(4_000) + "\n",
    );
    utimesSync(stalePath, new Date(1_000), new Date(1_000));

    assert.deepEqual(
      sessionContextUsage({
        backend: "claude",
        sessionID,
        model: "claude-opus-5",
        at: "2026-07-31T20:00:00.000Z",
        claudeRoot: root,
      }),
      { usedTokens: 102, limitTokens: 1_000_000 },
    );

    writeFileSync(
      activePath,
      [
        claudeAssistantLine({
          timestamp: "2026-07-31T19:30:00.000Z",
          input: 2,
          cacheRead: 100,
          cacheWrite: 0,
        }),
        claudeAssistantLine({
          timestamp: "2026-07-31T19:40:00.000Z",
          input: 3,
          cacheRead: 700,
          cacheWrite: 11,
        }),
      ].join("\n") + "\n",
    );
    utimesSync(activePath, new Date(2_000), new Date(2_000));

    assert.deepEqual(
      sessionContextUsage({
        backend: "claude",
        sessionID,
        model: "claude-opus-5",
        at: "2026-07-31T20:00:00.000Z",
        claudeRoot: root,
      }),
      { usedTokens: 714, limitTokens: 1_000_000 },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("큰 Claude 기록은 마지막 구간을 읽어 최신 점유를 유지한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-claude-large-"));
  try {
    const project = join(root, "project");
    mkdirSync(project);
    const sessionID = "2206f58e-0bd8-43dd-8068-780090adbaa8";
    writeFileSync(
      join(project, `${sessionID}.jsonl`),
      `${"x".repeat(2_000)}\n` +
        claudeAssistantLine({
          timestamp: "2026-07-31T19:40:00.000Z",
          input: 4,
          cacheRead: 800,
          cacheWrite: 20,
        }) + "\n",
    );

    assert.deepEqual(
      sessionContextUsage({
        backend: "claude",
        sessionID,
        model: "claude-opus-5",
        at: "2026-07-31T20:00:00.000Z",
        claudeRoot: root,
        maxReadBytes: 1_024,
      }),
      { usedTokens: 824, limitTokens: 1_000_000 },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("이어붙은 기록은 다시 읽어 최신 점유를 반영한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-codex-"));
  try {
    const day = join(root, "2026", "08", "01");
    mkdirSync(day, { recursive: true });
    const sessionID = "019fbbb6-0c8f-7ac2-b1c7-755cf64a5440";
    const path = join(day, `rollout-2026-08-01T14-04-58-${sessionID}.jsonl`);
    writeFileSync(
      path,
      codexTokenCountLine({
        timestamp: "2026-08-01T05:05:06.005Z",
        lastInput: 38_195,
        window: 258_400,
      }) + "\n",
    );

    const first = sessionContextUsage({
      backend: "codex",
      sessionID,
      at: "2026-08-01T06:00:00.000Z",
      codexRoot: root,
    });
    assert.deepEqual(first, { usedTokens: 38_195, limitTokens: 258_400 });

    appendFileSync(
      path,
      codexTokenCountLine({
        timestamp: "2026-08-01T05:30:00.000Z",
        lastInput: 57_218,
        window: 258_400,
      }) + "\n",
    );

    assert.deepEqual(
      sessionContextUsage({
        backend: "codex",
        sessionID,
        at: "2026-08-01T06:00:00.000Z",
        codexRoot: root,
      }),
      { usedTokens: 57_218, limitTokens: 258_400 },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("기록이나 한도를 찾지 못하면 값을 만들지 않는다", () => {
  const root = mkdtempSync(join(tmpdir(), "officellm-empty-"));
  try {
    assert.equal(
      sessionContextUsage({
        backend: "claude",
        sessionID: "missing-session",
        model: "claude-opus-5",
        at: "2026-08-01T06:00:00.000Z",
        claudeRoot: root,
      }),
      null,
    );
    assert.equal(
      sessionContextUsage({
        backend: "claude",
        sessionID: null,
        model: "claude-opus-5",
        at: "2026-08-01T06:00:00.000Z",
        claudeRoot: root,
      }),
      null,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
