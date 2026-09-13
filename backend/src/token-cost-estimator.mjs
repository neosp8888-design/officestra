// 이 파일은 CLI가 보고한 토큰 사용량을 공식 API 단가 기준의 USD 추정 비용으로 환산한다.
// Claude는 예외로, Claude Code가 보고한 금액만 쓰고 토큰으로 다시 계산하지 않는다.

import { pricingRateFor } from "./pricing-catalog.mjs";

// A turn contains multiple model requests. Never use its cumulative input as
// the context length of a single request. Claude remains CLI-reported only.
export function estimateTurnTokenCost(options) {
  const { backend, usage, model, fastMode, pricedAt = new Date() } = options;
  if (!usage || backend === "claude") return estimateTokenCost(options);
  const requests = usage.requestUsages;
  if (Array.isArray(requests) && requests.length > 0) {
    if (requests.some((request) => !request?.usage)) return null;
    for (const field of ["inputTokens", "outputTokens", "cachedInputTokens", "cacheWriteInputTokens"]) {
      if (nonnegativeNumber(usage[field] ?? 0) === null ||
          requests.some((request) => nonnegativeNumber(request.usage[field] ?? 0) === null)) return null;
      const total = requests.reduce((sum, request) => sum + (request.usage?.[field] ?? 0), 0);
      if (total !== (usage[field] ?? 0)) return null;
    }
    let total = 0;
    for (const request of requests) {
      const amount = estimateTokenCost({
        backend,
        model: request.model ?? model,
        fastMode: request.fastMode ?? fastMode,
        pricedAt: request.pricedAt ?? pricedAt,
        usage: request.usage,
      });
      if (amount === null) return null;
      total += amount;
    }
    return roundedCost(total);
  }
  // Missing request records: only a context-independent price is unambiguous.
  // Comparing all possible sizes prevents silently under/overpricing a turn.
  const max = (usage.inputTokens ?? 0) +
    (backend === "antigravity" ? (usage.cachedInputTokens ?? 0) : 0);
  const low = pricingRateFor({ backend, model, fastMode, pricedAt, promptTokens: 0 });
  const high = pricingRateFor({ backend, model, fastMode, pricedAt, promptTokens: max });
  if (JSON.stringify(low) !== JSON.stringify(high)) return null;
  return estimateTokenCost(options);
}

export function estimateTokenCost({
  backend,
  model,
  fastMode,
  usage,
  pricedAt = new Date(),
}) {
  if (!usage) {
    return null;
  }

  if (backend === "claude") {
    return reportedClaudeCost(usage);
  }
  if (backend === "antigravity") {
    return estimateAntigravityCost({ model, usage, pricedAt });
  }
  if (backend !== "codex") {
    return null;
  }

  const inputTokens = nonnegativeNumber(usage.inputTokens);
  const outputTokens = nonnegativeNumber(usage.outputTokens);
  const prices = pricingRateFor({
    backend: "codex",
    model,
    fastMode,
    promptTokens: inputTokens,
    pricedAt,
  });
  if (!prices || inputTokens === null || outputTokens === null) {
    return null;
  }

  const cachedInputTokens = Math.min(
    inputTokens,
    nonnegativeNumber(usage.cachedInputTokens) ?? 0,
  );
  const cacheWriteInputTokens = Math.min(
    Math.max(inputTokens - cachedInputTokens, 0),
    nonnegativeNumber(usage.cacheWriteInputTokens) ?? 0,
  );
  const uncachedInputTokens = Math.max(
    inputTokens - cachedInputTokens - cacheWriteInputTokens,
    0,
  );
  const cost = (
    uncachedInputTokens * prices.input +
    cachedInputTokens * (prices.cached ?? prices.input) +
    cacheWriteInputTokens * (prices.cacheWrite ?? prices.input) +
    outputTokens * prices.output
  ) / 1_000_000;
  return roundedCost(cost);
}

function estimateAntigravityCost({ model, usage, pricedAt }) {
  const inputTokens = nonnegativeNumber(usage.inputTokens);
  const outputTokens = nonnegativeNumber(usage.outputTokens);
  if (inputTokens === null || outputTokens === null) {
    return null;
  }
  const cachedInputTokens = nonnegativeNumber(usage.cachedInputTokens) ?? 0;
  const prices = pricingRateFor({
    backend: "antigravity",
    model,
    promptTokens: inputTokens + cachedInputTokens,
    pricedAt,
  });
  if (!prices) {
    return null;
  }

  // agy stream-json은 캐시 미스 입력과 cache_read_tokens를 별도로 보고한다.
  // output_tokens에는 thinking_tokens가 이미 포함되므로 추론 토큰을 다시
  // 더하지 않는다. 컨텍스트 캐시 보관 시간은 CLI 턴 데이터로 알 수 없어
  // 저장 요금은 이 API 환산 추정치에서 제외한다.
  const cost = (
    inputTokens * prices.input +
    cachedInputTokens * (prices.cached ?? prices.input) +
    outputTokens * prices.output
  ) / 1_000_000;
  return roundedCost(cost);
}

// Claude 비용은 Claude Code가 보고한 값만 쓴다. GUI 턴은 result의
// total_cost_usd, 터미널 턴은 상태줄이 알려준 세션 누적 비용의 차이가
// reportedCostUsd로 들어온다. 서브에이전트·압축·내부 호출까지 포함한
// 공급자 합계이므로 토큰으로 다시 계산하지 않고, 보고값이 없으면 비용을
// 만들어내지 않는다.
function reportedClaudeCost(usage) {
  const reportedCost = nonnegativeNumber(usage.reportedCostUsd);
  return reportedCost === null ? null : roundedCost(reportedCost);
}

function nonnegativeNumber(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : null;
}

function roundedCost(value) {
  return Math.round(value * 100_000_000) / 100_000_000;
}
