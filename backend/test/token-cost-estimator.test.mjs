// 이 파일은 공식 토큰 단가에 따른 완료 턴의 USD 추정 비용 계산을 검증한다.

import assert from "node:assert/strict";
import test from "node:test";

import { estimateTokenCost, estimateTurnTokenCost } from "../src/token-cost-estimator.mjs";

function requestTurn(requests) {
  const result = { requestUsages: requests };
  for (const field of ["inputTokens", "outputTokens", "cachedInputTokens", "cacheWriteInputTokens"]) {
    result[field] = requests.reduce((sum, request) => sum + (request.usage[field] ?? 0), 0);
  }
  return result;
}

test("Astra 턴 누적 30만이어도 각 요청 15만이면 장문 할증하지 않는다", () => {
  const request = { usage: { inputTokens: 150_000, outputTokens: 100 } };
  const options = { backend: "codex", model: "gpt-6-astra", fastMode: false };
  const usage = requestTurn([request, request]);
  assert.equal(estimateTokenCost({ ...options, usage }), 6.015);
  assert.equal(estimateTurnTokenCost({ ...options, usage }), 3.01);
});

test("실제 장문 요청은 캐시 포함 입력 272001부터 개별 할증한다", () => {
  const options = { backend: "codex", model: "gpt-6-astra", fastMode: false };
  const requests = [272_000, 272_001].map(inputTokens => ({
    usage: { inputTokens, cachedInputTokens: 200_000, outputTokens: 100 },
  }));
  assert.equal(estimateTokenCost({ ...options, usage: requests[0].usage }), 0.925);
  assert.equal(estimateTokenCost({ ...options, usage: requests[1].usage }), 1.84752);
  assert.equal(estimateTurnTokenCost({ ...options, usage: requestTurn(requests) }), 2.77252);
});

test("요청별 모델과 Fast 설정을 적용하고 Claude는 보고 금액만 보존한다", () => {
  const options = { backend: "codex", model: "gpt-6-astra", fastMode: false };
  const requests = [
    { model: "gpt-6-astra", fastMode: true, usage: { inputTokens: 1000, outputTokens: 100 } },
    { model: "gpt-5.6-sol", fastMode: false, usage: { inputTokens: 1000, outputTokens: 100 } },
  ];
  const expected = requests.reduce((sum, request) => sum + estimateTokenCost({ ...options, ...request }), 0);
  assert.equal(estimateTurnTokenCost({ ...options, usage: requestTurn(requests) }), expected);
  assert.equal(estimateTurnTokenCost({ backend: "claude", usage: {
    ...requestTurn(requests), reportedCostUsd: 12.345678,
  } }), 12.345678);
  assert.equal(estimateTurnTokenCost({ backend: "claude", usage: requestTurn(requests) }), null);
});

test("누락되거나 합계와 불일치하는 요청 원본으로 장문 비용을 추측하지 않는다", () => {
  const options = { backend: "codex", model: "gpt-6-astra", fastMode: false };
  const usage = { inputTokens: 300_000, outputTokens: 200 };
  assert.equal(estimateTurnTokenCost({ ...options, usage }), null);
  assert.equal(estimateTurnTokenCost({ ...options, usage: { ...usage,
    requestUsages: [{ usage: { inputTokens: 150_000, outputTokens: 100 } }],
  } }), null);
  assert.equal(estimateTurnTokenCost({ ...options, usage: { ...usage, requestUsages: [null] } }), null);
  assert.equal(estimateTurnTokenCost({ ...options, usage: { inputTokens: 150_000, outputTokens: 100 } }), 1.505);
});

test("Antigravity Pro도 요청별 캐시 포함 길이를 사용하고 Flash 단일 단가는 유지한다", () => {
  const pricedAt = new Date("2026-09-13T00:00:00Z");
  const request = { usage: { inputTokens: 100_000, cachedInputTokens: 50_000, outputTokens: 100 } };
  for (const model of ["gemini-3.1-pro", "gemini-3.8-flash"]) {
    const options = { backend: "antigravity", model, pricedAt };
    const expected = estimateTokenCost({ ...options, usage: request.usage }) * 2;
    assert.equal(estimateTurnTokenCost({ ...options, usage: requestTurn([request, request]) }), expected);
  }
});

const usage = {
  inputTokens: 1_000,
  cachedInputTokens: 200,
  cacheWriteInputTokens: 100,
  outputTokens: 50,
};

test("GPT-5.6 Sol Standard 비용을 캐시 종류별로 계산한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: false,
      usage,
    }),
    0.00438,
  );
});

test("GPT-5.6 Sol Fast 비용은 Fast 공식 단가를 사용한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "codex",
      model: "gpt-5.6-sol",
      fastMode: true,
      usage,
    }),
    0.00876,
  );
});

test("Gemini 3.7 Flash는 agy의 분리된 입력·캐시와 합산 출력을 계산한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.7-flash",
      fastMode: false,
      pricedAt: new Date("2026-08-28T00:00:00Z"),
      usage: {
        inputTokens: 100_000,
        cachedInputTokens: 20_000,
        outputTokens: 5_000,
        reasoningOutputTokens: 4_000,
      },
    }),
    0.09525,
  );
});

// 3.8-flash 단가는 3.7-flash와 같다.
test("Gemini 3.8 Flash는 3.7 Flash와 같은 단가로 비용을 채운다", () => {
  const usage = {
    inputTokens: 100_000,
    cachedInputTokens: 20_000,
    outputTokens: 5_000,
    reasoningOutputTokens: 4_000,
  };
  const promotion = new Date("2026-08-28T00:00:00Z");
  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.8-flash",
      fastMode: false,
      pricedAt: promotion,
      usage,
    }),
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.7-flash",
      fastMode: false,
      pricedAt: promotion,
      usage,
    }),
  );
  assert.notEqual(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.8-flash",
      fastMode: false,
      pricedAt: promotion,
      usage,
    }),
    null,
  );
});

test("Gemini 3.7 Flash는 프로모션 종료 뒤 정가를 사용한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.7-flash",
      fastMode: false,
      pricedAt: new Date("2027-01-01T00:00:00Z"),
      usage: {
        inputTokens: 100_000,
        cachedInputTokens: 20_000,
        outputTokens: 5_000,
      },
    }),
    0.1905,
  );
});

test("Gemini 3.1 Pro는 전체 프롬프트 20만 초과 단가를 적용한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.1-pro",
      fastMode: false,
      usage: {
        inputTokens: 2_000,
        cachedInputTokens: 200_000,
        outputTokens: 1_000,
      },
    }),
    0.106,
  );
});

test("Gemini 3.5 Flash는 모델별 출력 단가를 사용한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "gemini-3.5-flash",
      fastMode: false,
      usage: {
        inputTokens: 1_000,
        cachedInputTokens: 1_000,
        outputTokens: 1_000,
      },
    }),
    0.01065,
  );
});

test("Claude CLI가 보고한 누적 비용을 그대로 보존한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "claude",
      model: "claude-opus-5",
      fastMode: true,
      usage: { reportedCostUsd: 0.13599125 },
    }),
    0.13599125,
  );
});

test("Claude 전체 pipeline 비용은 본체 토큰 재계산보다 우선한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "claude",
      model: "claude-opus-5",
      fastMode: false,
      usage: {
        inputTokens: 2,
        outputTokens: 4,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 10,
        reportedCostUsd: 0.42,
      },
    }),
    0.42,
  );
});

test("Claude Sonnet 5 공급자 비용은 시점과 무관하게 받은 값을 보존한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "claude",
      model: "claude-sonnet-5",
      fastMode: false,
      pricedAt: new Date("2026-08-19T00:00:00Z"),
      usage: {
        reportedCostUsd: 1.4559903,
        reportedSonnet5CostUsd: 1.1559903,
      },
    }),
    1.4559903,
  );
});

test("Claude Sonnet 5 공급자 비용은 정가 전환일부터 그대로 보존한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "claude",
      model: "claude-sonnet-5",
      fastMode: false,
      pricedAt: new Date("2026-09-01T00:00:00Z"),
      usage: {
        reportedCostUsd: 1.1559903,
        reportedSonnet5CostUsd: 1.1559903,
      },
    }),
    1.1559903,
  );
});

// Claude 비용은 Claude Code가 보고한 값만 저장한다. 보고값이 없으면 토큰과
// 단가로 다시 계산하지 않고 비운다.
test("Claude 보고 비용이 없으면 토큰으로 추정하지 않는다", () => {
  for (const [model, speed] of [
    ["claude-opus-5", "standard"],
    ["claude-opus-5", "fast"],
    ["claude-sonnet-5", "standard"],
    ["opus[1m]", "standard"],
  ]) {
    assert.equal(
      estimateTokenCost({
        backend: "claude",
        model,
        fastMode: speed === "fast",
        usage: {
          inputTokens: 10,
          outputTokens: 5,
          cachedInputTokens: 100,
          cacheWriteInputTokens: 50,
          cacheWrite5mInputTokens: 20,
          cacheWrite1hInputTokens: 30,
          speed,
          inferenceGeo: "global",
          reportedCostUsd: null,
        },
      }),
      null,
    );
  }
});

test("Claude 상태줄 차액 0도 보고값으로 보존한다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "claude",
      model: "opus[1m]",
      fastMode: false,
      usage: { inputTokens: 10, outputTokens: 5, reportedCostUsd: 0 },
    }),
    0,
  );
});

test("단가를 모르는 모델은 비용을 꾸며내지 않는다", () => {
  assert.equal(
    estimateTokenCost({
      backend: "codex",
      model: "unknown",
      fastMode: false,
      usage,
    }),
    null,
  );

  assert.equal(
    estimateTokenCost({
      backend: "antigravity",
      model: "unknown",
      fastMode: false,
      usage,
    }),
    null,
  );
});
