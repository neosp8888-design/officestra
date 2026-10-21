// 롤플레이 대화: 리스 파일 해독, CBS, 프롬프트 조립, 서비스(DB 선택)를 합성 프리셋으로 검사한다.
import test from "node:test";
import assert from "node:assert/strict";
import { createCipheriv, createHash, randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import {
  decodeRisuModule,
  decodeRisuPreset,
  encodeRPack,
  RisuFormatError,
} from "../src/roleplay-risu-format.mjs";
import { parseCBS } from "../src/roleplay-cbs.mjs";
import {
  applyRegexScripts,
  assembleRoleplayMessages,
  parseToggleDefinitions,
  roleplayRequestBody,
} from "../src/roleplay-prompt.mjs";
import { RoleplayError, RoleplayService } from "../src/roleplay-service.mjs";

function encodeMsgpack(value) {
  const parts = [];
  const header = (bytes) => parts.push(Buffer.from(bytes));
  const write = (item) => {
    if (item === undefined) return header([0xd4, 0, 0]);
    if (item === null) return header([0xc0]);
    if (item === true || item === false) return header([item ? 0xc3 : 0xc2]);
    if (typeof item === "number") {
      if (Number.isInteger(item) && item >= 0 && item < 128) return header([item]);
      if (Number.isInteger(item) && item >= -(2 ** 31) && item < 2 ** 31) {
        const buffer = Buffer.alloc(5);
        buffer[0] = 0xd2;
        buffer.writeInt32BE(item, 1);
        return parts.push(buffer);
      }
      const buffer = Buffer.alloc(9);
      buffer[0] = 0xcb;
      buffer.writeDoubleBE(item, 1);
      return parts.push(buffer);
    }
    if (typeof item === "string") {
      const bytes = Buffer.from(item, "utf8");
      if (bytes.length < 32) header([0xa0 | bytes.length]);
      else {
        const length = Buffer.alloc(5);
        length[0] = 0xdb;
        length.writeUInt32BE(bytes.length, 1);
        parts.push(length);
      }
      return parts.push(bytes);
    }
    if (Buffer.isBuffer(item)) {
      const length = Buffer.alloc(5);
      length[0] = 0xc6;
      length.writeUInt32BE(item.length, 1);
      parts.push(length, item);
      return;
    }
    if (Array.isArray(item)) {
      const length = Buffer.alloc(5);
      length[0] = 0xdd;
      length.writeUInt32BE(item.length, 1);
      parts.push(length);
      item.forEach(write);
      return;
    }
    const entries = Object.entries(item);
    const length = Buffer.alloc(5);
    length[0] = 0xdf;
    length.writeUInt32BE(entries.length, 1);
    parts.push(length);
    for (const [key, entry] of entries) {
      write(key);
      write(entry);
    }
  };
  write(value);
  return Buffer.concat(parts);
}

function risuPresetFile(preset) {
  const key = createHash("sha256").update("risupreset").digest();
  const cipher = createCipheriv("aes-256-gcm", key, Buffer.alloc(12));
  const encrypted = Buffer.concat([cipher.update(encodeMsgpack(preset)), cipher.final(), cipher.getAuthTag()]);
  const outer = encodeMsgpack({ presetVersion: 2, type: "preset", preset: encrypted });
  return encodeRPack(gzipSync(outer));
}

function risuModuleFile(module) {
  const body = encodeRPack(Buffer.from(JSON.stringify({ type: "risuModule", module }), "utf8"));
  const header = Buffer.alloc(6);
  header[0] = 111;
  header[1] = 0;
  header.writeUInt32LE(body.length, 2);
  return Buffer.concat([header, body]);
}

// 블록 순서·토글 조건·정규식·플래그를 고루 쓰는 작은 프리셋.
function syntheticPreset(overrides = {}) {
  return {
    name: "합성 프리셋",
    promptTemplate: [
      {
        type: "plain",
        type2: "main",
        role: "system",
        text: "시작 {{#if_pure {{? {{getglobalvar::toggle_on}}=1}}}}켜짐{{/if}}{{position::x}}",
      },
      { type: "description", innerFormat: "<설명>{{slot}}</설명>" },
      { type: "chat", rangeStart: 0, rangeEnd: -1 },
      { type: "plain", type2: "normal", role: "system", text: "중간 지시 {{user}}" },
      { type: "chat", rangeStart: -1, rangeEnd: "end" },
      { type: "plain", type2: "normal", role: "bot", text: "알겠어요" },
    ],
    customPromptTemplateToggle: "on=켜기\nmode=방식=select=짧게,길게\n=묶음=group",
    moduleIntergration: "test-module",
    maxResponse: 1000,
    temperature: 80,
    thinkingType: "budget",
    thinkingTokens: 500,
    enableCustomFlags: true,
    customFlags: [7],
    systemContentReplacement: "",
    systemRoleReplacement: "user",
    promptSettings: { trimStartNewChat: false },
    regex: [{ comment: "", type: "editprocess", in: "비밀", out: "(가림)", flag: "g", ableFlag: false }],
    localStopStrings: undefined,
    ...overrides,
  };
}

function syntheticModule(overrides = {}) {
  return {
    name: "합성 모듈",
    namespace: "test-module",
    regex: [
      { comment: "", type: "editoutput", in: "(\\d+)", out: "[$1]", flag: "g", ableFlag: true },
      { comment: "", type: "editinput", in: "^/", out: "", flag: "g", ableFlag: true },
    ],
    ...overrides,
  };
}

test("리스 프리셋·모듈 파일을 원래 객체로 푼다", () => {
  const preset = syntheticPreset();
  const decoded = decodeRisuPreset(risuPresetFile(preset));
  assert.deepEqual(decoded.promptTemplate, preset.promptTemplate);
  assert.deepEqual(decoded.customFlags, [7]);
  assert.equal(decoded.temperature, 80);
  assert.equal("localStopStrings" in decoded, true);
  assert.equal(decoded.localStopStrings, undefined);

  const module = syntheticModule();
  assert.deepEqual(decodeRisuModule(risuModuleFile(module)), module);
});

test("리스 파일이 아니면 해독 오류를 낸다", () => {
  assert.throws(() => decodeRisuPreset(Buffer.from("not a preset")), RisuFormatError);
  assert.throws(() => decodeRisuModule(Buffer.from("not a module")), RisuFormatError);
  const tampered = risuPresetFile(syntheticPreset());
  assert.throws(() => decodeRisuPreset(tampered.subarray(0, tampered.length - 3)), RisuFormatError);
});

test("CBS 조건·계산·이름을 리스 규칙대로 푼다", () => {
  const context = { userName: "민지", charName: "코대리", vars: { toggle_on: "1" } };
  const condition = "{{#if_pure {{? {{getglobalvar::toggle_on}}=1}}}}켜짐{{/if}}";
  assert.equal(parseCBS(condition, context), "켜짐");
  assert.equal(parseCBS(condition, { vars: {} }), "");
  assert.equal(parseCBS("{{? {{? 1>0}} || {{? 0>1}}=1}}", {}), "1");
  assert.equal(parseCBS("{{? {{getglobalvar::toggle_none}}>0}}", { vars: {} }), "0");
  assert.equal(parseCBS("{{? 1+2*3}}", {}), "7");
  assert.equal(parseCBS("{{user}}와 <char>", context), "민지와 코대리");
  assert.equal(parseCBS("{{#if 1}}\n   첫줄\n   둘째{{/if}}", {}), "첫줄\n둘째");
  assert.equal(parseCBS("{{#if_pure 1}}\n  그대로{{/if}}", {}), "\n  그대로");
  assert.equal(parseCBS("{{#if 0}}{{#if 1}}안{{/if}}밖{{/if}}뒤", {}), "뒤");
  assert.equal(parseCBS("{{unknownfn::x}} {{abc", {}), "{{unknownfn::x}} {{abc");
});

test("토글 정의를 종류별로 읽는다", () => {
  assert.deepEqual(parseToggleDefinitions("on=켜기\nmode=방식=select=짧게,길게\n=묶음=group\nmemo=메모=text"), [
    { key: "on", value: "켜기", type: "checkbox", options: [] },
    { key: "mode", value: "방식", type: "select", options: ["짧게", "길게"] },
    { key: "", value: "묶음", type: "group", options: [] },
    { key: "memo", value: "메모", type: "text", options: [] },
  ]);
});

test("정규식 스크립트는 그룹 치환과 순서를 지킨다", () => {
  const scripts = [
    { type: "editoutput", in: "a", out: "b", flag: "g", ableFlag: false },
    { type: "editoutput", in: "(\\d+)", out: "[$1]", flag: "g<order 5>", ableFlag: true },
    { type: "editinput", in: "x", out: "y", flag: "g", ableFlag: false },
  ];
  assert.equal(applyRegexScripts(scripts, "a1 a22", "editoutput", {}), "b[1] b[22]");
  assert.equal(applyRegexScripts(scripts, "x", "editinput", {}), "y");
});

test("프리셋 블록 순서와 모델 플래그대로 요청을 조립한다", () => {
  const preset = syntheticPreset();
  const formatted = assembleRoleplayMessages({
    preset,
    toggles: { on: "1" },
    characterName: "코대리",
    characterDescription: "명랑한 개발자",
    userName: "민지",
    messages: [
      { id: "1", role: "user", content: "안녕" },
      { id: "2", role: "char", content: "반가워 비밀" },
      { id: "3", role: "user", content: "다음" },
    ],
  });
  const body = roleplayRequestBody({ preset, model: "test-model", formatted });
  assert.deepEqual(body, {
    model: "test-model",
    max_tokens: 1000,
    stream: false,
    temperature: 0.8,
    thinking: { type: "enabled", budget_tokens: 500, display: "summarized" },
    system: "시작 켜짐\n\n<설명>명랑한 개발자</설명>\n\n[Start a new chat]",
    messages: [
      { role: "user", content: "안녕" },
      { role: "assistant", content: "반가워 (가림)" },
      { role: "user", content: "system: 중간 지시 민지\n\n다음" },
      { role: "assistant", content: "알겠어요" },
    ],
  });
});

test("사용자 지정 플래그가 없으면 뒤쪽 system을 System: 사용자 메시지로 보낸다", () => {
  const preset = syntheticPreset({ enableCustomFlags: false, thinkingType: "off" });
  const formatted = assembleRoleplayMessages({
    preset,
    characterName: "코대리",
    userName: "민지",
    messages: [
      { id: "1", role: "user", content: "안녕" },
      { id: "2", role: "char", content: "반가워" },
      { id: "3", role: "user", content: "다음" },
    ],
  });
  const body = roleplayRequestBody({ preset, model: "m", formatted });
  assert.equal(body.thinking, undefined);
  // 리스도 설명이 비어 있으면 innerFormat만 남은 블록을 그대로 보낸다.
  assert.equal(body.system, "시작 \n\n<설명></설명>\n\n[Start a new chat]");
  assert.deepEqual(body.messages, [
    { role: "user", content: "안녕" },
    { role: "assistant", content: "반가워" },
    { role: "user", content: "System: 중간 지시 민지\n\n다음" },
    { role: "assistant", content: "알겠어요" },
  ]);
});

// 실제 PostgreSQL에서 격리 스키마를 만들고 끝나면 되돌린다. 실제 직원·대화 기록은 건드리지 않는다.
test("롤플레이 서비스 DB 통합", { skip: process.env.OFFICESTRA_TEST_ROLEPLAY_POSTGRES !== "1" }, async (t) => {
  const { pool } = await import("../src/db.mjs");
  const client = await pool.connect();
  const keyDirectory = await mkdtemp(join(tmpdir(), "roleplay-key-"));
  try {
    await client.query("BEGIN");
    const schema = `test_roleplay_${randomUUID().replaceAll("-", "")}`;
    await client.query(`CREATE SCHEMA ${schema}`);
    await client.query(`SET LOCAL search_path TO ${schema}, public`);
    await client.query("CREATE TABLE characters (id text PRIMARY KEY, name text NOT NULL, config jsonb NOT NULL DEFAULT '{}'::jsonb)");
    await client.query("INSERT INTO characters VALUES ('right-woman', '코대리')");
    await client.query("CREATE TABLE local_agent_profiles (id text PRIMARY KEY, definition jsonb NOT NULL, enabled boolean NOT NULL DEFAULT true)");
    const localDefinition = { profile: { id: "local-test", providerKind: "local", backend: "claude", model: "test-model", endpoint: "http://127.0.0.1:41235", credentialEnv: "OFFICESTRA_LOCAL_TEST_TOKEN", credentialVersion: "v1", contextWindow: 32768, maxOutputTokens: 4096, usageProtocol: "anthropic-normalized-v1" }, host: { address: "127.0.0.1", user: "test", sshPort: 2222, keyPath: "/tmp/test-key", hostKeyAlias: "test-host", modelKey: "test/model", comfyPort: 8188 } };
    await client.query(
      "INSERT INTO local_agent_profiles VALUES ('local-test', $1::jsonb, true), ('local-codex', $2::jsonb, true), ('local-off', $1::jsonb, false)",
      [JSON.stringify(localDefinition), JSON.stringify({ ...localDefinition, profile: { ...localDefinition.profile, id: "local-codex", backend: "codex", usageProtocol: "openai-responses-v1" } })],
    );
    await client.query(await readFile(new URL("../../database/migrations/066_roleplay_chat.sql", import.meta.url), "utf8"));
    await client.query(await readFile(new URL("../../database/migrations/067_roleplay_local_profile.sql", import.meta.url), "utf8"));
    const keyFile = join(keyDirectory, "key");
    await writeFile(keyFile, "test-key\n", { mode: 0o600 });

    const calls = [];
    let reply = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({
      content: [{ type: "thinking", thinking: "숨김" }, { type: "text", text: "답 123" }],
    }) });
    const events = [];
    const doors = [];
    let localProviders = null;
    const service = new RoleplayService({
      localProviders: () => localProviders,
      pool: { query: (sql, args) => client.query(sql, args) },
      broadcast: (event) => events.push(event),
      fetchImpl: async (url, options) => {
        calls.push({ url, options });
        return await reply();
      },
      apiURL: "http://127.0.0.1:1/v1/messages",
      apiKeyFile: keyFile,
    });
    const id = "right-woman";
    const finish = async () => await service.generating.get(id)?.promise;

    const presetFile = risuPresetFile(syntheticPreset()).toString("base64");
    const first = await service.importAsset({ fileName: "합성.risup", dataBase64: presetFile });
    const again = await service.importAsset({ fileName: "다른이름.risup", dataBase64: presetFile });
    const module = await service.importAsset({ fileName: "합성.risum", dataBase64: risuModuleFile(syntheticModule()).toString("base64") });
    assert.equal(first.created, true);
    assert.equal(again.created, false);
    assert.equal(again.asset.id, first.asset.id);
    assert.deepEqual(
      { kind: first.asset.kind, name: first.asset.name, promptCount: first.asset.promptCount, toggleCount: first.asset.toggleCount },
      { kind: "preset", name: "합성 프리셋", promptCount: 6, toggleCount: 2 },
    );
    assert.equal(module.asset.kind, "module");
    await assert.rejects(service.importAsset({ fileName: "x.risup", dataBase64: "AAAA" }), RoleplayError);

    await t.test("프리셋 없이 보내면 거절하고 잘못된 자산 종류도 거절한다", async () => {
      await assert.rejects(service.sendMessage(id, { content: "안녕" }), (error) => error.status === 409);
      await assert.rejects(service.updateProfile(id, { presetId: module.asset.id }), (error) => error.status === 400);
      await assert.rejects(service.updateProfile("missing", {}), (error) => error.status === 404);
    });

    await t.test("설정을 저장하고 통합 모듈의 토글까지 보여 준다", async () => {
      const saved = await service.updateProfile(id, {
        enabled: true,
        presetId: first.asset.id,
        toggles: { on: "1" },
        characterDescription: "명랑한 개발자",
        userName: "민지",
      });
      assert.equal(saved.profile.enabled, true);
      assert.deepEqual(saved.activeModuleIds, [module.asset.id]);
      assert.deepEqual(saved.toggleDefinitions.map((toggle) => toggle.key), ["on", "mode", ""]);
      assert.equal(events.at(-1).type, "roleplay.changed");
    });

    await t.test("메시지를 보내면 입력·출력 정규식을 거쳐 대화에 저장한다", async () => {
      assert.deepEqual(Object.keys(await service.sendMessage(id, { content: "/안녕" })).sort(), ["accepted", "chatId"]);
      await finish();
      const state = await service.readProfile(id);
      assert.deepEqual(state.messages.map((message) => [message.role, message.content]), [
        ["user", "안녕"],
        ["char", "답 [123]"],
      ]);
      assert.equal(state.generating, false);
      assert.equal(state.lastError, null);
      const sent = calls.at(-1);
      assert.equal(sent.options.headers["x-api-key"], "test-key");
      const body = JSON.parse(sent.options.body);
      assert.equal(body.model, "gemini-3.8-flash-high");
      assert.match(body.system, /^시작 켜짐/);
      assert.equal(body.messages[0].content, "안녕");
    });

    await t.test("답을 만드는 동안 다음 메시지와 새 대화는 막는다", async () => {
      let release;
      reply = () => new Promise((resolve) => {
        release = () => resolve({ ok: true, status: 200, text: async () => JSON.stringify({ content: [{ type: "text", text: "늦은 답" }] }) });
      });
      await service.sendMessage(id, { content: "기다려" });
      await assert.rejects(service.sendMessage(id, { content: "또" }), (error) => error.status === 409);
      await assert.rejects(service.startNewChat(id), (error) => error.status === 409);
      assert.equal((await service.readProfile(id)).generating, true);
      while (!release) await new Promise((resolve) => setImmediate(resolve));
      release();
      await finish();
      assert.equal((await service.readProfile(id)).messages.at(-1).content, "늦은 답");
    });

    await t.test("모델 오류는 마지막 오류로 남고 키는 노출하지 않는다", async () => {
      reply = async () => ({ ok: false, status: 401, text: async () => JSON.stringify({ error: { message: "invalid key" } }) });
      await service.sendMessage(id, { content: "실패" });
      await finish();
      const state = await service.readProfile(id);
      assert.match(state.lastError, /401/);
      assert.doesNotMatch(state.lastError, /test-key/);
      assert.equal(state.messages.at(-1).role, "user");
    });

    await t.test("로컬 프로필은 켜진 Messages 형식만 고를 수 있다", async () => {
      await assert.rejects(service.updateProfile(id, { localProfileId: "local-codex" }), (error) => error.status === 400);
      await assert.rejects(service.updateProfile(id, { localProfileId: "local-off" }), (error) => error.status === 400);
      await assert.rejects(service.updateProfile(id, { localProfileId: "missing" }), (error) => error.status === 400);
      assert.equal((await service.updateProfile(id, { localProfileId: "local-test" })).profile.localProfileId, "local-test");
    });

    await t.test("로컬 프로필이면 로컬 문으로 보내고 모델명과 출력 한도를 맞춘다", async () => {
      reply = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({ content: [{ type: "text", text: "로컬 답" }] }) });
      await service.sendMessage(id, { content: "서비스 없음" });
      await finish();
      assert.match((await service.readProfile(id)).lastError, /준비되지 않았습니다/);

      localProviders = {
        openMessagesDoor: async (definition) => {
          const door = { definition, released: false };
          doors.push(door);
          return {
            url: "http://127.0.0.1:2/v1/messages",
            token: "door-token",
            model: definition.profile.model,
            maxOutputTokens: 1000,
            release: async () => { door.released = true; },
          };
        },
      };
      await service.sendMessage(id, { content: "로컬" });
      await finish();
      const state = await service.readProfile(id);
      assert.equal(state.lastError, null);
      assert.equal(state.messages.at(-1).content, "로컬 답");
      const sent = calls.at(-1);
      assert.equal(sent.url, "http://127.0.0.1:2/v1/messages");
      assert.equal(sent.options.headers["x-api-key"], "door-token");
      const body = JSON.parse(sent.options.body);
      assert.equal(body.model, "test-model");
      assert.equal(body.max_tokens, 1000);
      assert.equal(doors.length, 1);
      assert.equal(doors[0].definition.profile.id, "local-test");
      assert.equal(doors[0].definition.host.address, "127.0.0.1");
      assert.equal(doors[0].released, true);

      // 직원에게 다른 프로필이 배정돼 있어도 저장한 PC 주소는 롤플레이도 따른다.
      await client.query(
        "UPDATE characters SET config = $2::jsonb WHERE id = $1",
        [id, JSON.stringify({ localProfileId: "local-codex", localHostAddress: "127.0.0.2", localReasoning: "high" })],
      );
      reply = async () => ({ ok: false, status: 503, text: async () => JSON.stringify({ error: "로컬 연결 실패: 시간 초과" }) });
      await service.sendMessage(id, { content: "로컬 실패" });
      await finish();
      assert.match((await service.readProfile(id)).lastError, /503로 거절했습니다\. 로컬 연결 실패: 시간 초과/);
      assert.equal(doors.at(-1).definition.host.address, "127.0.0.2");
      assert.notEqual(doors.at(-1).definition.profile.reasoning, "high");
      assert.equal(doors.at(-1).released, true);

      assert.equal((await service.updateProfile(id, { localProfileId: "" })).profile.localProfileId, null);
      reply = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({ content: [{ type: "text", text: "프록시 답" }] }) });
      await service.sendMessage(id, { content: "프록시" });
      await finish();
      assert.equal(calls.at(-1).url, "http://127.0.0.1:1/v1/messages");
      assert.equal(doors.length, 2);
    });

    await t.test("새 대화를 시작하면 빈 대화로 바뀐다", async () => {
      const state = await service.startNewChat(id);
      assert.deepEqual(state.messages, []);
      assert.equal(state.lastError, null);
    });
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
    await rm(keyDirectory, { recursive: true, force: true });
  }
});
