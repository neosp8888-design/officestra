// 터미널 턴 사용량을 CLI 기록에서 턴 구간만큼 합산하는지 검증한다.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  claudeTranscriptTurnUsage,
  codexRolloutTurnUsage,
} from "../src/terminal-usage.mjs";

const startedAt = "2026-09-02T14:00:00.000Z";
const endedAt = "2026-09-02T14:01:00.000Z";

test("Codex 반복 token_count를 제거하되 동일 크기의 별도 요청은 보존한다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-request-usage-"));
  const path = join(root, "session.jsonl");
  const prefix = "이전 기록\n";
  const last = { input_tokens: 150000, output_tokens: 100 };
  const first = codexLine("2026-09-02T14:00:10.000Z", { last, total: last });
  const context = JSON.stringify({ type: "turn_context", payload: {
    turn_id: "turn-1", model: "gpt-6-astra", service_tier: "fast",
  } });
  try {
    writeFileSync(path, prefix + [context, first, first,
      codexLine("2026-09-02T14:00:20.000Z", {
        last, total: { input_tokens: 300000, output_tokens: 200 },
      }),
      codexLine("2026-09-02T14:02:20.000Z", {
        last, total: { input_tokens: 450000, output_tokens: 300 },
      }),
    ].join("\n"));
    const usage = await codexRolloutTurnUsage(path, {
      startedAt, endedAt, scope: "window", startOffset: Buffer.byteLength(prefix),
    });
    assert.equal(usage.inputTokens, 300000);
    assert.equal(usage.outputTokens, 200);
    assert.equal(usage.requestUsages.length, 2);
    assert.equal(usage.requestUsages[0].model, "gpt-6-astra");
    assert.equal(usage.requestUsages[0].fastMode, true);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test("Claude 기록은 턴 구간의 응답만 더하고 같은 응답을 두 번 세지 않는다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-terminal-usage-"));
  const path = join(root, "session.jsonl");
  try {
    writeFileSync(
      path,
      [
        // 턴 이전 응답은 제외한다.
        claudeLine("2026-09-02T13:59:59.000Z", "msg_before", {
          input_tokens: 900,
          output_tokens: 900,
        }),
        claudeLine("2026-09-02T14:00:10.000Z", "msg_1", {
          input_tokens: 2,
          output_tokens: 100,
          cache_read_input_tokens: 1_000,
          cache_creation_input_tokens: 40,
          cache_creation: {
            ephemeral_5m_input_tokens: 10,
            ephemeral_1h_input_tokens: 30,
          },
        }),
        // 같은 응답이 두 줄로 남아도 한 번만 센다.
        claudeLine("2026-09-02T14:00:11.000Z", "msg_1", {
          input_tokens: 2,
          output_tokens: 100,
          cache_read_input_tokens: 1_000,
          cache_creation_input_tokens: 40,
        }),
        claudeLine("2026-09-02T14:00:30.000Z", "msg_2", {
          input_tokens: 3,
          output_tokens: 50,
          cache_read_input_tokens: 500,
        }),
        // 턴 이후 응답도 제외한다.
        claudeLine("2026-09-02T14:01:30.000Z", "msg_after", {
          input_tokens: 700,
          output_tokens: 700,
        }),
        "깨진 줄",
      ].join("\n"),
      "utf8",
    );

    const usage = await claudeTranscriptTurnUsage(path, {
      startedAt,
      endedAt,
    });

    assert.equal(usage.inputTokens, 5);
    assert.equal(usage.outputTokens, 150);
    assert.equal(usage.cachedInputTokens, 1_500);
    assert.equal(usage.cacheWriteInputTokens, 40);
    assert.equal(usage.cacheWrite5mInputTokens, 10);
    assert.equal(usage.cacheWrite1hInputTokens, 30);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Codex 기록은 누적 총량이 아니라 요청별 사용량을 더한다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-terminal-usage-"));
  const path = join(root, "rollout.jsonl");
  try {
    writeFileSync(path, codexRollout(), "utf8");

    const usage = await codexRolloutTurnUsage(path, { startedAt });

    assert.equal(usage.inputTokens, 2_000);
    assert.equal(usage.outputTokens, 50);
    assert.equal(usage.cachedInputTokens, 1_500);
    assert.equal(usage.cacheWriteInputTokens, 10);
    assert.equal(usage.reasoningOutputTokens, 20);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// 종료 시각은 초 단위로 잘려 저장되므로 마지막 요청이 그 뒤에 기록된다.
// 시간 구간이 아니라 rollout의 턴 경계를 따라야 이 요청을 놓치지 않는다.
test("Codex 마지막 요청이 종료 시각보다 늦게 기록돼도 합산한다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-terminal-usage-"));
  const path = join(root, "rollout.jsonl");
  try {
    writeFileSync(path, codexRollout(), "utf8");

    const usage = await codexRolloutTurnUsage(path, { startedAt });
    const withoutLastRequest = 1_200;

    assert.ok(usage.inputTokens > withoutLastRequest);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Codex 턴 시작 기록이 없으면 사용량을 만들지 않는다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-terminal-usage-"));
  const path = join(root, "rollout.jsonl");
  try {
    writeFileSync(path, codexRollout(), "utf8");

    assert.equal(
      await codexRolloutTurnUsage(path, {
        startedAt: "2026-09-02T15:00:00.000Z",
      }),
      null,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("기록이 없거나 구간을 알 수 없으면 사용량 없이 넘어간다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-terminal-usage-"));
  const path = join(root, "session.jsonl");
  try {
    writeFileSync(
      path,
      claudeLine("2026-09-02T14:00:10.000Z", "msg_1", {
        input_tokens: 2,
        output_tokens: 100,
      }),
      "utf8",
    );

    assert.equal(
      await claudeTranscriptTurnUsage(null, { startedAt, endedAt }),
      null,
    );
    assert.equal(
      await claudeTranscriptTurnUsage(join(root, "없음.jsonl"), {
        startedAt,
        endedAt,
      }),
      null,
    );
    assert.equal(
      await claudeTranscriptTurnUsage(path, { startedAt: null, endedAt }),
      null,
    );
    assert.equal(await codexRolloutTurnUsage(null, { startedAt }), null);
    assert.equal(await codexRolloutTurnUsage(path, { startedAt }), null);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

function claudeLine(timestamp, id, usage) {
  return JSON.stringify({
    type: "assistant",
    timestamp,
    uuid: `${id}-${timestamp}`,
    message: { id, usage },
  });
}

function codexLine(timestamp, { last, total }) {
  return JSON.stringify({
    timestamp,
    type: "event_msg",
    payload: {
      type: "token_count",
      info: { last_token_usage: last, total_token_usage: total },
    },
  });
}

function codexTaskLine(timestamp, type, seconds) {
  return JSON.stringify({
    timestamp,
    type: "event_msg",
    payload: { type, started_at: seconds },
  });
}

function codexRollout() {
  const startedSecond = Math.floor(new Date(startedAt).getTime() / 1_000);
  return [
    // 이전 턴의 요청은 경계 밖이므로 제외한다.
    codexTaskLine("2026-09-02T13:58:00.000Z", "task_started", startedSecond - 200),
    codexLine("2026-09-02T13:59:00.000Z", {
      last: { input_tokens: 5_000, output_tokens: 500 },
      total: { input_tokens: 5_000, output_tokens: 500 },
    }),
    codexTaskLine("2026-09-02T13:59:01.000Z", "task_complete", startedSecond - 200),
    codexTaskLine("2026-09-02T14:00:00.100Z", "task_started", startedSecond),
    codexLine("2026-09-02T14:00:20.000Z", {
      last: {
        input_tokens: 1_200,
        output_tokens: 30,
        cached_input_tokens: 900,
        cache_write_input_tokens: 10,
        reasoning_output_tokens: 12,
      },
      // 압축으로 누적 총량이 줄어도 요청별 값만 쓰므로 영향이 없다.
      total: { input_tokens: 400, output_tokens: 10 },
    }),
    // 저장된 종료 시각(14:01:00.000)보다 늦게 기록된 마지막 요청이다.
    codexLine("2026-09-02T14:01:00.700Z", {
      last: {
        input_tokens: 800,
        output_tokens: 20,
        cached_input_tokens: 600,
        reasoning_output_tokens: 8,
      },
      total: { input_tokens: 1_200, output_tokens: 30 },
    }),
    codexTaskLine("2026-09-02T14:01:00.850Z", "task_complete", startedSecond),
    // 다음 턴의 요청은 경계 밖이므로 제외한다.
    codexTaskLine("2026-09-02T14:02:00.000Z", "task_started", startedSecond + 120),
    codexLine("2026-09-02T14:02:10.000Z", {
      last: { input_tokens: 9_000, output_tokens: 900 },
      total: { input_tokens: 10_200, output_tokens: 930 },
    }),
  ].join("\n");
}
