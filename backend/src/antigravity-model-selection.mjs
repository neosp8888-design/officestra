// agy models의 고정 모델과 노력 수준이 포함된 모델 ID를 CLI 인자로 변환한다.
// Thinking 모델에는 --effort를 붙이면 agy 1.2.14가 실행 전에 거부한다.
export function isAntigravityFixedThinkingModel(model) {
  return ["claude-sonnet-4-6", "claude-opus-4-6-thinking"].includes(model);
}

export function antigravityModelArguments(model, effort) {
  if (isAntigravityFixedThinkingModel(model)) {
    // 이전 설정의 high도 실행 가능하게 하되 추론 강도를 적용했다고 주장하지 않는다.
    return ["--model", model];
  }
  const variant = String(model).match(/^(.*)-(low|medium|high)$/);
  if (String(model).startsWith("gemini-") && ["low", "medium", "high"].includes(effort)) {
    return ["--model", `${variant?.[1] ?? model}-${effort}`];
  }
  if (variant || effort === "default") return ["--model", model];
  // 사용자 정의 모델은 기존 CLI의 effort 처리에 맡긴다.
  return ["--model", model, "--effort", effort];
}
