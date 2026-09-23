import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  MODEL_CATALOG_REFRESH_MILLISECONDS,
  ModelCatalogService,
  ModelCatalogValidationError,
  claudeModelTitle,
  mergeModelCatalog,
  parseAntigravityModelCatalog,
  parseClaudeModelCatalog,
  parseCodexModelCatalog,
} from "../src/model-catalog.mjs";

const codexPayload = JSON.stringify({
  models: [
    {
      slug: "internal-hidden",
      display_name: "Hidden",
      visibility: "hide",
      default_reasoning_level: "high",
      supported_reasoning_levels: [{ effort: "high" }],
    },
    {
      slug: "gpt-next",
      display_name: "GPT Next",
      visibility: "list",
      default_reasoning_level: "medium",
      supported_reasoning_levels: [
        { effort: "low" },
        { effort: "medium" },
        { effort: "high" },
      ],
      additional_speed_tiers: ["fast"],
      service_tiers: [{ id: "priority" }],
      context_window: 300_000,
      max_context_window: 1_000_000,
    },
  ],
});

const antigravityPayload = [
  "gemini-next-high\tGemini Next (High)",
  "gemini-next-medium\tGemini Next (Medium)",
  "gemini-next-low\tGemini Next (Low)",
  "claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)",
  "gpt-oss-120b-medium\tGPT-OSS 120B (Medium)",
].join("\n");

// Claude Code stream-json은 initialize control_response 앞뒤로 다른 줄을 낼 수 있다.
const claudePayload = [
  JSON.stringify({ type: "system", subtype: "init", session_id: "s" }),
  JSON.stringify({
    type: "control_response",
    response: {
      subtype: "success",
      request_id: "officestra-model-catalog",
      response: {
        models: [
          {
            value: "default",
            resolvedModel: "claude-opus-5[1m]",
            displayName: "Default (recommended)",
            supportsEffort: true,
            supportedEffortLevels: ["low", "medium", "high", "xhigh", "max"],
            supportsFastMode: true,
          },
          {
            value: "opus[1m]",
            resolvedModel: "claude-opus-5[1m]",
            displayName: "Opus (1M context)",
            supportsEffort: true,
            supportedEffortLevels: ["max", "low", "medium", "high", "xhigh"],
            supportsFastMode: true,
          },
          {
            value: "fable[1m]",
            resolvedModel: "claude-fable-5-1[1m]",
            displayName: "Fable",
            supportsEffort: true,
            supportedEffortLevels: ["low", "medium", "high", "xhigh", "max"],
          },
          { value: "haiku", displayName: "Haiku" },
          {
            value: "restricted",
            displayName: "Restricted",
            disabled: true,
            supportedEffortLevels: ["high"],
          },
        ],
      },
    },
  }),
].join("\n");

test("Claude initialize 응답은 선택기 값과 추론 단계를 읽고 별칭·비추론·비활성 모델은 뺀다", () => {
  const parsed = parseClaudeModelCatalog(claudePayload);
  assert.deepEqual(parsed.models.map((entry) => entry.id), [
    "opus[1m]",
    "fable[1m]",
  ]);
  assert.deepEqual(parsed.models[0], {
    id: "opus[1m]",
    title: "Opus 5 (1M)",
    efforts: ["low", "medium", "high", "xhigh", "max"],
    defaultEffort: "high",
    supportsFastMode: true,
    contextWindow: null,
    maxContextWindow: null,
    available: true,
    resolvedModel: "claude-opus-5[1m]",
    previousResolvedModel: null,
    resolvedModelChangedAt: null,
  });
  assert.equal(parsed.models[1].title, "Fable 5.1 (1M)");
  assert.equal(parsed.models[1].supportsFastMode, false);
  assert.throws(
    () =>
      parseClaudeModelCatalog(JSON.stringify({
        type: "control_response",
        response: { subtype: "error", error: "not logged in" },
      })),
    /not logged in/,
  );
  assert.throws(() => parseClaudeModelCatalog(""), /models 배열/);
});

test("Codex JSON은 화면 노출 모델과 실제 옵션만 추린다", () => {
  const parsed = parseCodexModelCatalog(codexPayload);
  assert.equal(parsed.models.length, 1);
  assert.deepEqual(parsed.models[0], {
    id: "gpt-next",
    title: "GPT Next",
    efforts: ["low", "medium", "high"],
    defaultEffort: "medium",
    supportsFastMode: true,
    contextWindow: 300_000,
    maxContextWindow: 1_000_000,
    available: true,
    resolvedModel: null,
    previousResolvedModel: null,
    resolvedModelChangedAt: null,
  });
});

test("Claude 제목은 별칭이 가리키는 실제 모델의 버전을 보여준다", () => {
  assert.equal(claudeModelTitle("Fable", "claude-fable-5-1[1m]"), "Fable 5.1 (1M)");
  assert.equal(claudeModelTitle("Sonnet", "claude-sonnet-5"), "Sonnet 5");
  assert.equal(claudeModelTitle("Haiku", "claude-haiku-4-5-20251001"), "Haiku 4.5");
  assert.equal(claudeModelTitle("Custom", "my-gateway-model"), "Custom · my-gateway-model");
  assert.equal(claudeModelTitle("Opus", null), "Opus");
});

// 같은 별칭(fable[1m])이 새 모델을 가리키게 되면 이전 값과 시각을 남기고,
// 다음 갱신에서 변화가 없으면 그 기록을 이어 간다.
test("같은 별칭의 실제 모델이 바뀌면 이전 모델과 시각을 남긴다", () => {
  const first = mergeModelCatalog(
    { version: 1, models: [] },
    parseClaudeModelCatalog(claudePayload),
    { now: Date.parse("2026-09-05T00:00:00Z") },
  );
  assert.equal(first.models[1].resolvedModelChangedAt, null);
  const revisedPayload = claudePayload.replace(
    "claude-fable-5-1[1m]",
    "claude-fable-5-2[1m]",
  );
  const second = mergeModelCatalog(
    first,
    parseClaudeModelCatalog(revisedPayload),
    { now: Date.parse("2026-10-01T00:00:00Z") },
  );
  const fable = second.models.find((entry) => entry.id === "fable[1m]");
  assert.equal(fable.title, "Fable 5.2 (1M)");
  assert.equal(fable.resolvedModel, "claude-fable-5-2[1m]");
  assert.equal(fable.previousResolvedModel, "claude-fable-5-1[1m]");
  assert.equal(fable.resolvedModelChangedAt, "2026-10-01T00:00:00.000Z");
  assert.equal(second.models[0].previousResolvedModel, null);
  const third = mergeModelCatalog(
    second,
    parseClaudeModelCatalog(revisedPayload),
    { now: Date.parse("2026-10-02T00:00:00Z") },
  );
  const carried = third.models.find((entry) => entry.id === "fable[1m]");
  assert.equal(carried.previousResolvedModel, "claude-fable-5-1[1m]");
  assert.equal(carried.resolvedModelChangedAt, "2026-10-01T00:00:00.000Z");
});

test("Antigravity 목록은 Gemini 노력 변형을 한 모델로 합친다", () => {
  const parsed = parseAntigravityModelCatalog(antigravityPayload);
  assert.deepEqual(parsed.models.map((entry) => entry.id), [
    "gemini-next",
    "claude-sonnet-4-6",
    "gpt-oss-120b-medium",
  ]);
  assert.deepEqual(parsed.models[0].efforts, ["low", "medium", "high"]);
  assert.equal(parsed.models[1].defaultEffort, "high");
  assert.deepEqual(parsed.models[2].efforts, ["medium"]);
});

test("사라진 모델은 선택 목록에서 내리되 기존 설정 검증용으로 보존한다", () => {
  const merged = mergeModelCatalog(
    parseCodexModelCatalog(codexPayload),
    parseCodexModelCatalog(JSON.stringify({
      models: [{
        slug: "gpt-newer",
        display_name: "GPT Newer",
        visibility: "list",
        default_reasoning_level: "high",
        supported_reasoning_levels: [{ effort: "high" }],
      }],
    })),
  );
  assert.deepEqual(merged.models.map((entry) => [entry.id, entry.available]), [
    ["gpt-newer", true],
    ["gpt-next", false],
  ]);
});

test("서비스는 12시간 게이트와 제외 모델을 영속 저장한다", async () => {
  let now = Date.parse("2026-09-05T00:00:00Z");
  const store = fakeStore();
  let commandCount = 0;
  const claudeInputs = [];
  const payloads = {
    codex: codexPayload,
    antigravity: antigravityPayload,
    claude: claudePayload,
  };
  const service = new ModelCatalogService({
    store,
    now: () => now,
    resolveExecutable: (provider) => provider,
    runCommand: async (executable, argumentsList, options) => {
      commandCount += 1;
      if (executable === "claude") {
        claudeInputs.push({ argumentsList, input: options.input });
      }
      return { stdout: payloads[executable] };
    },
  });
  await service.loadCached();
  const first = await service.refreshDue();
  assert.deepEqual(
    first.map((entry) => entry.status),
    ["updated", "updated", "updated"],
  );
  assert.equal(commandCount, 3);
  // Claude는 프롬프트 없이 initialize 요청만 stdin으로 보내야 비용이 없다.
  assert.equal(claudeInputs.length, 1);
  assert.ok(claudeInputs[0].argumentsList.includes("--input-format"));
  assert.match(claudeInputs[0].input, /"subtype":"initialize"/);
  assert.equal(
    service.modelCapabilities("claude", "opus[1m]")?.supportsFastMode,
    true,
  );
  // 내장 기본 목록의 기존 설정값은 선택 목록에서 내려가되 검증용으로 남는다.
  assert.equal(
    service.modelCapabilities("claude", "claude-opus-5")?.available,
    false,
  );

  now += MODEL_CATALOG_REFRESH_MILLISECONDS - 1;
  const fresh = await service.refreshDue();
  assert.deepEqual(
    fresh.map((entry) => entry.status),
    ["fresh", "fresh", "fresh"],
  );
  assert.equal(commandCount, 3);

  const snapshot = await service.setExclusions("codex", ["gpt-next"])
    .catch((error) => error);
  assert.ok(snapshot instanceof ModelCatalogValidationError);

  // 단일 모델을 모두 제외할 수 없으므로 모델을 하나 더 발견시킨다.
  const current = store.rows.get("codex");
  current.catalog.models.push({
    ...current.catalog.models[0],
    id: "gpt-second",
    title: "GPT Second",
  });
  service.rows.set("codex", current);
  const saved = await service.setExclusions("codex", ["gpt-next"]);
  assert.deepEqual(
    saved.providers.find((entry) => entry.backend === "codex").excludedModels,
    ["gpt-next"],
  );
});

test("갱신 실패 시 마지막 정상 모델과 제외 설정을 유지한다", async () => {
  const store = fakeStore();
  const service = new ModelCatalogService({
    store,
    resolveExecutable: (provider) => provider,
    runCommand: async () => ({ stdout: codexPayload }),
  });
  await service.loadCached();
  await service.refreshProvider("codex", { force: true });
  await service.setExclusions("codex", []);

  service.runCommand = async () => {
    throw new Error("offline");
  };
  const failed = await service.refreshProvider("codex", { force: true });
  assert.equal(failed.status, "failed");
  assert.equal(service.modelCapabilities("codex", "gpt-next")?.id, "gpt-next");
  assert.equal(service.snapshot().providers[0].lastError, "offline");
});

test("CLI가 마지막 수집 시도 뒤에 설치됐을 때만 12시간을 기다리지 않고 다시 받는다", async () => {
  let now = Date.parse("2026-09-23T10:53:00Z");
  const changedAt = { codex: 0, antigravity: 0, claude: 0 };
  const fetched = [];
  const service = new ModelCatalogService({
    store: fakeStore(),
    now: () => now,
    resolveExecutable: (provider) => provider,
    executableChangedAt: async (executable) => changedAt[executable],
    runCommand: async (executable) => {
      fetched.push(executable);
      return {
        stdout: {
          codex: codexPayload,
          antigravity: antigravityPayload,
          claude: claudePayload,
        }[executable],
      };
    },
  });
  await service.loadCached();
  await service.refreshDue();
  fetched.length = 0;

  now += 60_000;
  const unchanged = await service.refreshUpdatedExecutables();
  assert.deepEqual(unchanged.map((entry) => entry.status), ["fresh", "fresh", "fresh"]);
  assert.deepEqual(fetched, []);

  // 수집 4분 뒤 Claude CLI만 새로 설치됐다.
  changedAt.claude = Date.parse("2026-09-23T10:57:34Z");
  now = Date.parse("2026-09-23T11:03:00Z");
  const updated = await service.refreshUpdatedExecutables();
  assert.deepEqual(updated.map((entry) => entry.status), ["fresh", "fresh", "updated"]);
  assert.deepEqual(fetched, ["claude"]);

  // 한 번 다시 받은 뒤에는 같은 설치로 반복 수집하지 않는다.
  now += 60_000;
  await service.refreshUpdatedExecutables();
  assert.deepEqual(fetched, ["claude"]);
});

test("마이그레이션은 모델 카탈로그와 동적 effort 저장을 준비한다", () => {
  const source = readFileSync(
    new URL("../../database/migrations/036_agent_model_catalog.sql", import.meta.url),
    "utf8",
  );
  assert.match(source, /CREATE TABLE IF NOT EXISTS agent_model_catalogs/);
  assert.match(source, /excluded_models text\[\]/);
  assert.match(source, /effort ~ '\^\[a-z\]/);
  const claude = readFileSync(
    new URL("../../database/migrations/037_claude_model_catalog.sql", import.meta.url),
    "utf8",
  );
  assert.match(claude, /DROP CONSTRAINT IF EXISTS agent_model_catalogs_provider_check/);
  assert.match(claude, /CHECK \(provider IN \('codex', 'antigravity', 'claude'\)\)/);
});

function fakeStore() {
  const rows = new Map();
  return {
    rows,
    async load() {
      return [...rows.values()];
    },
    async claimRefresh({
      provider,
      attemptedAt,
      minimumIntervalMilliseconds,
      force,
    }) {
      const previous = rows.get(provider);
      if (
        !force && previous?.lastAttemptedAt &&
        attemptedAt.getTime() - new Date(previous.lastAttemptedAt).getTime() <
          minimumIntervalMilliseconds
      ) {
        return null;
      }
      const row = {
        provider,
        catalog: previous?.catalog ?? { version: 1, models: [] },
        excludedModels: previous?.excludedModels ?? [],
        fetchedAt: previous?.fetchedAt ?? null,
        lastAttemptedAt: attemptedAt,
        lastError: previous?.lastError ?? null,
      };
      rows.set(provider, row);
      return row;
    },
    async saveSuccess({ provider, catalog, fetchedAt }) {
      const row = {
        ...rows.get(provider),
        catalog,
        fetchedAt,
        lastError: null,
      };
      rows.set(provider, row);
      return row;
    },
    async saveFailure({ provider, error }) {
      const row = { ...rows.get(provider), lastError: String(error) };
      rows.set(provider, row);
      return row;
    },
    async saveExclusions({ provider, excludedModels, fallbackCatalog }) {
      const row = {
        provider,
        catalog: rows.get(provider)?.catalog ?? fallbackCatalog,
        excludedModels,
        fetchedAt: rows.get(provider)?.fetchedAt ?? null,
        lastAttemptedAt: rows.get(provider)?.lastAttemptedAt ?? null,
        lastError: rows.get(provider)?.lastError ?? null,
      };
      rows.set(provider, row);
      return row;
    },
  };
}
