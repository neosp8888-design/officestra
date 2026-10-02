import assert from "node:assert/strict";
import test from "node:test";
import { antigravityModelArguments } from "../src/antigravity-model-selection.mjs";

test("고정 Thinking 모델은 이전 high 설정에서도 effort 인자를 보내지 않는다", () => {
  for (const model of ["claude-opus-4-6-thinking", "claude-sonnet-4-6"]) {
    for (const effort of ["high", "default"]) {
      assert.deepEqual(antigravityModelArguments(model, effort), ["--model", model]);
    }
  }
});

test("Gemini 추론 수준은 실제 모델 slug로 전달하고 기존 접미사는 교체한다", () => {
  for (const effort of ["low", "medium", "high"]) {
    assert.deepEqual(antigravityModelArguments("gemini-3.8-flash", effort),
      ["--model", `gemini-3.8-flash-${effort}`]);
  }
  assert.deepEqual(antigravityModelArguments("gemini-3.1-pro-high", "low"),
    ["--model", "gemini-3.1-pro-low"]);
});

test("단일 노력 변형과 기본 설정은 CLI에 중복 effort를 보내지 않는다", () => {
  assert.deepEqual(antigravityModelArguments("gpt-oss-120b-medium", "medium"),
    ["--model", "gpt-oss-120b-medium"]);
  assert.deepEqual(antigravityModelArguments("future-fixed", "default"),
    ["--model", "future-fixed"]);
});

test("사용자 정의 모델의 명시적 effort는 보존한다", () => {
  assert.deepEqual(antigravityModelArguments("custom-model", "high"),
    ["--model", "custom-model", "--effort", "high"]);
});
