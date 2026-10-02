// 이 파일은 여러 직원 설정 변경의 원자성, 잠금 순서와 드레인 복구를 검증한다.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { AgentBusyError } from "../src/agent-runtime.mjs";
import {
  CharacterSettingsDrainConflictError,
  CharacterSettingsTargetsNotFoundError,
  CharacterSettingsValidationError,
  normalizeBulkCharacterSettings,
  updateCharacterSettingsAtomically,
  withCharacterSessionLocks,
} from "../src/character-settings.mjs";

test("Antigravity 고정 Thinking 설정은 새 default와 이전 high를 기본값으로 저장한다", () => {
  const modelCatalog = { modelCapabilities: () => ({ efforts: ["default"], supportsFastMode: false }) };
  for (const effort of ["default", "high"]) {
    const [normalized] = normalizeBulkCharacterSettings({ updates: [settings("right-man", {
      backend: "antigravity", model: "claude-opus-4-6-thinking", effort,
      permission: "plan", fastMode: false,
    })] }, { modelCatalog });
    assert.equal(normalized.effort, "default");
  }
  assert.throws(() => normalizeBulkCharacterSettings({ updates: [settings("right-man", {
    backend: "antigravity", model: "claude-opus-4-6-thinking", effort: "low",
    permission: "plan", fastMode: false,
  })] }, { modelCatalog }), /지원하지 않는 추론 레벨/);
});

function profile(id, overrides = {}) {
  return {
    id,
    name: id === "boss" ? "백부장" : "코과장",
    backend: "codex",
    model: "gpt-5.6-sol",
    effort: "high",
    fastMode: false,
    permission: "workspace-write",
    identityPrompt: `${id} 역할`,
    ...overrides,
  };
}

function settings(characterId, overrides = {}) {
  return {
    characterId,
    backend: "codex",
    model: "gpt-5.6-sol",
    effort: "high",
    fastMode: false,
    permission: "workspace-write",
    ...overrides,
  };
}

function fakeDatabase(initialProfiles, {
  failUpdateCharacterID = null,
  timeline = [],
} = {}) {
  let committed = new Map(
    initialProfiles.map((character) => [character.id, { ...character }]),
  );
  let staged = null;
  let connectCount = 0;
  let releaseCount = 0;
  const queries = [];

  const client = {
    async query(source, parameters = []) {
      const sql = String(source).replace(/\s+/g, " ").trim();
      queries.push({ sql, parameters });
      if (sql.startsWith("SELECT pg_advisory_lock")) {
        timeline.push(`lock:${parameters[0].split(":").at(-1)}`);
        return { rowCount: 1, rows: [{}] };
      }
      if (sql.startsWith("SELECT pg_advisory_unlock")) {
        timeline.push(`unlock:${parameters[0].split(":").at(-1)}`);
        return { rowCount: 1, rows: [{}] };
      }
      if (sql === "BEGIN") {
        timeline.push("begin");
        staged = new Map(
          [...committed].map(([id, character]) => [id, { ...character }]),
        );
        return { rowCount: null, rows: [] };
      }
      if (sql === "COMMIT") {
        timeline.push("commit");
        committed = staged;
        staged = null;
        return { rowCount: null, rows: [] };
      }
      if (sql === "ROLLBACK") {
        timeline.push("rollback");
        staged = null;
        return { rowCount: null, rows: [] };
      }
      if (sql.includes("FROM characters") && sql.includes("FOR UPDATE")) {
        const ids = parameters[0];
        const rows = ids
          .filter((id) => staged.has(id))
          .map((id) => ({ ...staged.get(id) }))
          .sort((left, right) => left.id.localeCompare(right.id));
        timeline.push(`select:${rows.map((row) => row.id).join(",")}`);
        return { rowCount: rows.length, rows };
      }
      if (sql.startsWith("UPDATE characters")) {
        const characterID = parameters[0];
        timeline.push(`update:${characterID}`);
        if (characterID === failUpdateCharacterID) {
          throw new Error(`forced update failure: ${characterID}`);
        }
        const previous = staged.get(characterID);
        if (!previous) {
          return { rowCount: 0, rows: [] };
        }
        const updated = {
          ...previous,
          backend: parameters[1],
          model: parameters[2],
          effort: parameters[3],
          fastMode: parameters[4],
          permission: parameters[5],
        };
        staged.set(characterID, updated);
        return { rowCount: 1, rows: [{ ...updated }] };
      }
      throw new Error(`unexpected query: ${sql}`);
    },
    release() {
      releaseCount += 1;
    },
  };

  return {
    pool: {
      async connect() {
        connectCount += 1;
        return client;
      },
    },
    client,
    queries,
    timeline,
    profile(id) {
      return committed.get(id);
    },
    get connectCount() {
      return connectCount;
    },
    get releaseCount() {
      return releaseCount;
    },
  };
}

function fakeRuntime({
  idle = true,
  draining = false,
  inspectionErrorByID = new Map(),
  finalizeWarningByID = new Map(),
  timeline = [],
} = {}) {
  let beginCount = 0;
  let cancelCount = 0;
  return {
    draining,
    timeline,
    beginDrain() {
      beginCount += 1;
      this.draining = true;
      timeline.push("drain:begin");
      return this.maintenanceStatus();
    },
    cancelDrain() {
      cancelCount += 1;
      this.draining = false;
      timeline.push("drain:cancel");
      return this.maintenanceStatus();
    },
    maintenanceStatus() {
      return {
        acceptingJobs: !this.draining,
        draining: this.draining,
        activeTurnCount: idle ? 0 : 1,
        idle,
      };
    },
    async inspectWorkspaceForSessionEnd(characterID, client) {
      assert.ok(client);
      timeline.push(`inspect:${characterID}`);
      const inspectionError = inspectionErrorByID.get(characterID);
      if (inspectionError) {
        throw inspectionError;
      }
      return {
        characterID,
        workspace: null,
        reviewTurnID: null,
        review: null,
      };
    },
    async applyWorkspaceSessionEndPlan(client, plan) {
      assert.ok(client);
      timeline.push(`apply:${plan.characterID}`);
      return { ended: true };
    },
    async finalizeWorkspaceSessionEndPlan(plan) {
      timeline.push(`finalize:${plan.characterID}`);
      return finalizeWarningByID.get(plan.characterID) ?? null;
    },
    get beginCount() {
      return beginCount;
    },
    get cancelCount() {
      return cancelCount;
    },
  };
}

test("bulk 요청은 모든 항목을 먼저 검증하고 중복 직원을 거절한다", () => {
  assert.throws(
    () => normalizeBulkCharacterSettings({ updates: [] }),
    CharacterSettingsValidationError,
  );
  assert.throws(
    () =>
      normalizeBulkCharacterSettings({
        updates: [settings("boss"), settings("boss")],
      }),
    /같은 직원 설정/,
  );
  assert.throws(
    () =>
      normalizeBulkCharacterSettings({
        updates: [
          settings("boss"),
          settings("right-man", { backend: "unknown" }),
        ],
      }),
    /지원하지 않는 CLI/,
  );
});

test("Antigravity 3.8 Flash 선택이 검증을 통과해 저장된다", () => {
  assert.deepEqual(
    normalizeBulkCharacterSettings({
      updates: [settings("right-man", {
        backend: "antigravity",
        model: "gemini-3.8-flash",
        effort: "medium",
        fastMode: false,
        permission: "accept-edits",
      })],
    }),
    [{
      characterID: "right-man",
      backend: "antigravity",
      model: "gemini-3.8-flash",
      effort: "medium",
      fastMode: false,
      permission: "accept-edits",
    }],
  );
});

// 목록에서 내렸어도 이미 저장된 3.7-flash 값을 다시 저장할 수 있어야 한다.
test("Antigravity 3.7 Flash 저장값은 재저장 시 막히지 않는다", () => {
  assert.deepEqual(
    normalizeBulkCharacterSettings({
      updates: [settings("right-man", {
        backend: "antigravity",
        model: "gemini-3.7-flash",
        effort: "high",
        fastMode: false,
        permission: "accept-edits",
      })],
    })[0].model,
    "gemini-3.7-flash",
  );
});

test("Antigravity 설정은 실제 모델별 추론·권한·Fast 계약을 검증한다", () => {
  assert.deepEqual(
    normalizeBulkCharacterSettings({
      updates: [settings("boss", {
        backend: "antigravity",
        model: "gemini-3.7-flash",
        effort: "medium",
        fastMode: false,
        permission: "accept-edits",
      })],
    }),
    [{
      characterID: "boss",
      backend: "antigravity",
      model: "gemini-3.7-flash",
      effort: "medium",
      fastMode: false,
      permission: "accept-edits",
    }],
  );

  for (const overrides of [
    { model: "gemini-3.1-pro", effort: "medium" },
    { fastMode: true },
    { permission: "auto" },
  ]) {
    assert.throws(
      () => normalizeBulkCharacterSettings({
        updates: [settings("boss", {
          backend: "antigravity",
          model: "gemini-3.1-pro",
          effort: "high",
          fastMode: false,
          permission: "plan",
          ...overrides,
        })],
      }),
      CharacterSettingsValidationError,
    );
  }
});

test("동적 카탈로그의 새 Codex·Antigravity 모델과 옵션을 검증한다", () => {
  const capabilities = new Map([
    ["codex:gpt-future-mini", {
      efforts: ["low", "medium"],
      supportsFastMode: false,
    }],
    ["antigravity:gemini-future-pro", {
      efforts: ["low", "high"],
      supportsFastMode: false,
    }],
  ]);
  const modelCatalog = {
    modelCapabilities(backend, model) {
      return capabilities.get(`${backend}:${model}`) ?? null;
    },
  };

  const normalized = normalizeBulkCharacterSettings({
    updates: [
      settings("boss", {
        backend: "codex",
        model: "gpt-future-mini",
        effort: "medium",
        fastMode: false,
      }),
      settings("right-man", {
        backend: "antigravity",
        model: "gemini-future-pro",
        effort: "low",
        fastMode: false,
        permission: "accept-edits",
      }),
    ],
  }, { modelCatalog });

  assert.deepEqual(
    normalized.map(({ backend, model, effort }) => ({ backend, model, effort })),
    [
      { backend: "codex", model: "gpt-future-mini", effort: "medium" },
      {
        backend: "antigravity",
        model: "gemini-future-pro",
        effort: "low",
      },
    ],
  );

  assert.throws(
    () => normalizeBulkCharacterSettings({
      updates: [settings("boss", {
        model: "gpt-future-mini",
        effort: "ultra",
      })],
    }, { modelCatalog }),
    /지원하지 않는 추론 레벨/,
  );
  assert.throws(
    () => normalizeBulkCharacterSettings({
      updates: [settings("boss", {
        model: "gpt-future-mini",
        effort: "medium",
        fastMode: true,
      })],
    }, { modelCatalog }),
    /Fast 모드/,
  );
});

test("bulk 전체 요청 검증은 drain과 DB 잠금보다 먼저 끝난다", async () => {
  const database = fakeDatabase([profile("boss"), profile("right-man")]);
  const runtime = fakeRuntime();
  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: {
        updates: [
          settings("boss"),
          settings("right-man", { permission: "invalid" }),
        ],
      },
    }),
    CharacterSettingsValidationError,
  );
  assert.equal(runtime.beginCount, 0);
  assert.equal(runtime.cancelCount, 0);
  assert.equal(database.connectCount, 0);
});

test("여러 직원 잠금은 정렬 순서로 획득하고 역순으로 해제한다", async () => {
  const database = fakeDatabase([]);
  await assert.rejects(
    withCharacterSessionLocks(
      database.pool,
      ["right-man", "boss", "right-man"],
      async () => {
        database.timeline.push("operation");
        throw new Error("stop");
      },
    ),
    /stop/,
  );
  assert.deepEqual(database.timeline, [
    "lock:boss",
    "lock:right-man",
    "operation",
    "unlock:right-man",
    "unlock:boss",
  ]);
  assert.equal(database.releaseCount, 1);
});

test("bulk provider 전환은 모든 사전 검사를 마친 뒤 한 트랜잭션으로 반영한다", async () => {
  const timeline = [];
  const database = fakeDatabase(
    [profile("boss"), profile("right-man")],
    { timeline },
  );
  const runtime = fakeRuntime({
    timeline,
    finalizeWarningByID: new Map([
      ["right-man", "right-man: 대화 기록은 유지했습니다."],
    ]),
  });
  const result = await updateCharacterSettingsAtomically({
    pool: database.pool,
    runtime,
    body: {
      updates: [
        settings("right-man", {
          backend: "claude",
          model: "claude-sonnet-5",
          effort: "high",
          permission: "auto",
        }),
        settings("boss", {
          backend: "claude",
          model: "claude-opus-5",
          effort: "max",
          fastMode: true,
          permission: "bypassPermissions",
        }),
      ],
    },
  });

  assert.deepEqual(result.characters.map((character) => character.id), [
    "right-man",
    "boss",
  ]);
  assert.deepEqual(result.characters[0], profile("right-man", {
    backend: "claude",
    model: "claude-sonnet-5",
    permission: "auto",
  }));
  assert.deepEqual(result.characters[1], profile("boss", {
    backend: "claude",
    model: "claude-opus-5",
    effort: "max",
    fastMode: true,
    permission: "bypassPermissions",
  }));
  assert.deepEqual(result.warnings, [
    "right-man: 대화 기록은 유지했습니다.",
  ]);
  assert.ok(result.warnings.every((warning) => typeof warning === "string"));
  assert.ok(
    timeline.indexOf("inspect:boss") < timeline.indexOf("apply:right-man"),
  );
  assert.ok(
    timeline.indexOf("inspect:right-man") < timeline.indexOf("apply:right-man"),
  );
  assert.ok(timeline.indexOf("apply:boss") < timeline.indexOf("update:right-man"));
  assert.deepEqual(
    timeline.filter((event) => event.startsWith("lock:")),
    ["lock:boss", "lock:right-man"],
  );
  assert.equal(timeline.filter((event) => event === "begin").length, 1);
  assert.equal(timeline.filter((event) => event === "commit").length, 1);
  assert.equal(timeline.includes("rollback"), false);
  assert.equal(runtime.draining, false);
  assert.equal(runtime.beginCount, 1);
  assert.equal(runtime.cancelCount, 1);
});

test("같은 provider 설정 변경은 현재 작업 공간 종료 검사를 호출하지 않는다", async () => {
  const database = fakeDatabase([profile("boss")]);
  const runtime = fakeRuntime({
    inspectionErrorByID: new Map([
      ["boss", new AgentBusyError("호출되면 안 됩니다")],
    ]),
  });
  const result = await updateCharacterSettingsAtomically({
    pool: database.pool,
    runtime,
    body: {
      updates: [settings("boss", { effort: "xhigh" })],
    },
  });

  assert.equal(result.characters[0].effort, "xhigh");
  assert.equal(runtime.timeline.some((event) => event.startsWith("inspect:")), false);
});

test("대상 하나라도 없으면 사전 검사와 설정 변경 없이 전부 롤백한다", async () => {
  const database = fakeDatabase([profile("boss")]);
  const runtime = fakeRuntime();
  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: {
        updates: [settings("boss"), settings("right-man")],
      },
    }),
    CharacterSettingsTargetsNotFoundError,
  );
  assert.equal(
    database.queries.some(({ sql }) => sql.startsWith("UPDATE characters")),
    false,
  );
  assert.equal(runtime.timeline.some((event) => event.startsWith("inspect:")), false);
  assert.equal(runtime.draining, false);
  assert.equal(runtime.cancelCount, 1);
});

test("provider 사전 검사 하나라도 실패하면 세션과 설정을 하나도 변경하지 않는다", async () => {
  const timeline = [];
  const database = fakeDatabase(
    [profile("boss"), profile("right-man")],
    { timeline },
  );
  const runtime = fakeRuntime({
    timeline,
    inspectionErrorByID: new Map([
      ["boss", new AgentBusyError("업무 중")],
    ]),
  });
  const providerUpdates = ["right-man", "boss"].map((characterId) =>
    settings(characterId, {
      backend: "claude",
      model: "claude-sonnet-5",
      permission: "auto",
    })
  );

  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: { updates: providerUpdates },
    }),
    /업무 중/,
  );
  assert.equal(timeline.some((event) => event.startsWith("apply:")), false);
  assert.equal(timeline.some((event) => event.startsWith("update:")), false);
  assert.equal(timeline.includes("rollback"), true);
  assert.equal(runtime.draining, false);
});

test("두 번째 설정 UPDATE가 실패해도 첫 번째 설정은 커밋되지 않는다", async () => {
  const boss = profile("boss");
  const rightMan = profile("right-man");
  const database = fakeDatabase([boss, rightMan], {
    failUpdateCharacterID: "right-man",
  });
  const runtime = fakeRuntime();

  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: {
        updates: [
          settings("boss", { effort: "xhigh" }),
          settings("right-man", { effort: "max" }),
        ],
      },
    }),
    /forced update failure/,
  );
  assert.deepEqual(database.profile("boss"), boss);
  assert.deepEqual(database.profile("right-man"), rightMan);
  assert.equal(database.timeline.includes("commit"), false);
  assert.equal(database.timeline.includes("rollback"), true);
  assert.equal(runtime.draining, false);
});

test("실행 중 업무가 있으면 DB 잠금을 잡기 전에 실패하고 접수를 복구한다", async () => {
  const database = fakeDatabase([profile("boss")]);
  const runtime = fakeRuntime({ idle: false });
  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: { updates: [settings("boss")] },
    }),
    AgentBusyError,
  );
  assert.equal(database.connectCount, 0);
  assert.equal(runtime.draining, false);
  assert.equal(runtime.beginCount, 1);
  assert.equal(runtime.cancelCount, 1);
});

test("이미 드레인 중이면 소유하지 않은 드레인을 해제하지 않는다", async () => {
  const database = fakeDatabase([profile("boss")]);
  const runtime = fakeRuntime({ draining: true });
  await assert.rejects(
    updateCharacterSettingsAtomically({
      pool: database.pool,
      runtime,
      body: { updates: [settings("boss")] },
    }),
    CharacterSettingsDrainConflictError,
  );
  assert.equal(database.connectCount, 0);
  assert.equal(runtime.draining, true);
  assert.equal(runtime.beginCount, 0);
  assert.equal(runtime.cancelCount, 0);
});

test("server는 bulk route와 상태별 오류 매핑 및 개별 설정의 공용 잠금을 제공한다", () => {
  const source = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  const bulkSource = readFileSync(
    new URL("../src/character-settings.mjs", import.meta.url),
    "utf8",
  );
  assert.match(source, /url\.pathname === "\/api\/characters\/settings\/bulk"/);
  assert.match(
    source,
    /return await withCharacterSessionLocks\(pool, \[characterID\], body\)/,
  );
  assert.match(source, /CharacterSettingsValidationError[\s\S]*?response, 400/);
  assert.match(source, /CharacterSettingsTargetsNotFoundError[\s\S]*?response, 404/);
  assert.match(source, /CharacterSettingsRuntimeUnavailableError[\s\S]*?response, 503/);
  assert.match(source, /CharacterSettingsDrainConflictError[\s\S]*?response, 409/);
  assert.match(
    source,
    /COALESCE\(characters\.config, '\{\}'::jsonb\)[\s\S]*?- 'executablePath'/,
  );
  assert.match(
    bulkSource,
    /COALESCE\(characters\.config, '\{\}'::jsonb\)[\s\S]*?- 'executablePath'/,
  );
});
