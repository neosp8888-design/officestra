// 롤플레이 대화의 프리셋 가져오기, 직원별 설정, 대화 기록, 모델 호출을 맡는다.
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { normalizeLocalDefinition } from "./local-provider-service.mjs";
import { decodeRisuModule, decodeRisuPreset, RisuFormatError } from "./roleplay-risu-format.mjs";
import {
  activeModules,
  applyRegexScripts,
  assembleRoleplayMessages,
  parseToggleDefinitions,
  roleplayRequestBody,
} from "./roleplay-prompt.mjs";

export class RoleplayError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const MAX_IMPORT_BYTES = 20 * 1024 * 1024;
const MAX_TEXT = 200_000;
const MAX_MESSAGE = 20_000;
const REQUEST_TIMEOUT_MS = 300_000;
// 로컬은 4090에 모델을 올리고 앞선 요청을 기다리는 시간까지 포함한다.
const LOCAL_REQUEST_TIMEOUT_MS = 600_000;
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const PROFILE_DEFAULTS = {
  enabled: false,
  presetId: null,
  moduleIds: [],
  toggles: {},
  characterDescription: "",
  firstMessage: "",
  userName: "사용자",
  persona: "",
  model: "gemini-3.8-flash-high",
  localProfileId: null,
  currentChatId: null,
  updatedAt: null,
};

// 확장자로 먼저 고르고, 모르면 모듈·프리셋 순서로 풀어 본다.
export function decodeRoleplayAsset(fileName, buffer) {
  const lower = String(fileName ?? "").toLowerCase();
  const attempts = lower.endsWith(".risum")
    ? ["module"]
    : lower.endsWith(".risup")
      ? ["preset"]
      : ["module", "preset"];
  let lastError = null;
  for (const kind of attempts) {
    try {
      const data = kind === "module" ? decodeRisuModule(buffer) : decodeRisuPreset(buffer);
      const fallback = lower.replace(/\.(risum|risup)$/, "") || kind;
      return { kind, name: String(data.name ?? "").trim() || fallback, data };
    } catch (error) {
      if (!(error instanceof RisuFormatError)) throw error;
      lastError = error;
    }
  }
  throw new RoleplayError(400, lastError?.message ?? "리스 프리셋이나 모듈 파일이 아닙니다.");
}

function text(value, field, { max = MAX_TEXT, required = false } = {}) {
  if (typeof value !== "string") throw new RoleplayError(400, `${field}는 문자열이어야 합니다.`);
  if (value.length > max) throw new RoleplayError(400, `${field}가 너무 깁니다.`);
  if (required && !value.trim()) throw new RoleplayError(400, `${field}를 입력하세요.`);
  return value;
}

function uuid(value, field) {
  if (typeof value !== "string" || !UUID_PATTERN.test(value)) {
    throw new RoleplayError(400, `${field}는 UUID여야 합니다.`);
  }
  return value;
}

function toggleValues(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new RoleplayError(400, "토글 값은 객체여야 합니다.");
  }
  const result = {};
  for (const [key, item] of Object.entries(value)) {
    if (!key || key.length > 200) throw new RoleplayError(400, "토글 이름이 올바르지 않습니다.");
    result[key] = text(item, `토글 ${key}`, { max: 10_000 });
  }
  return result;
}

function profileFromRow(characterId, row) {
  if (!row) return { characterId, ...PROFILE_DEFAULTS };
  return {
    characterId,
    enabled: row.enabled,
    presetId: row.preset_id,
    moduleIds: row.module_ids ?? [],
    toggles: row.toggles ?? {},
    characterDescription: row.character_description,
    firstMessage: row.first_message,
    userName: row.user_name,
    persona: row.persona,
    model: row.model,
    localProfileId: row.local_profile_id ?? null,
    currentChatId: row.current_chat_id,
    updatedAt: row.updated_at,
  };
}

function assetSummary(row) {
  const data = row.data ?? {};
  const toggleTemplate = row.kind === "preset" ? data.customPromptTemplateToggle : data.customModuleToggle;
  return {
    id: row.id,
    kind: row.kind,
    name: row.name,
    importedAt: row.imported_at,
    promptCount: Array.isArray(data.promptTemplate) ? data.promptTemplate.length : 0,
    regexCount: Array.isArray(data.regex) ? data.regex.length : 0,
    toggleCount: parseToggleDefinitions(toggleTemplate)
      .filter((toggle) => !["group", "groupEnd", "divider", "caption"].includes(toggle.type)).length,
  };
}

function responseText(body) {
  if (!Array.isArray(body?.content)) return "";
  return body.content
    .filter((block) => block?.type === "text" && typeof block.text === "string")
    .map((block) => block.text)
    .join("");
}

export class RoleplayService {
  constructor({
    pool,
    broadcast = () => {},
    fetchImpl = globalThis.fetch,
    apiURL = process.env.OFFICE_ROLEPLAY_API_URL ?? "http://127.0.0.1:8080/v1/messages",
    apiKeyFile = process.env.OFFICE_ROLEPLAY_API_KEY_FILE ??
      join(homedir(), ".config", "antigravity-proxy", "local-api-key"),
    // 서버 시작 뒤에 만들어지므로 호출할 때 꺼내 쓴다.
    localProviders = () => null,
  }) {
    this.pool = pool;
    this.broadcast = broadcast;
    this.localProviders = localProviders;
    this.fetchImpl = fetchImpl;
    this.apiURL = apiURL;
    this.apiKeyFile = apiKeyFile;
    this.generating = new Map();
    this.lastErrors = new Map();
  }

  async listAssets() {
    const { rows } = await this.pool.query(
      "SELECT id, kind, name, data, imported_at FROM roleplay_assets ORDER BY kind, imported_at DESC, id",
    );
    return rows.map(assetSummary);
  }

  async importAsset(body) {
    const encoded = text(body?.dataBase64, "파일 내용", { max: Math.ceil(MAX_IMPORT_BYTES * 4 / 3) + 4, required: true });
    const buffer = Buffer.from(encoded, "base64");
    if (buffer.length === 0 || buffer.length > MAX_IMPORT_BYTES) {
      throw new RoleplayError(400, "파일 크기가 올바르지 않습니다.");
    }
    const asset = decodeRoleplayAsset(body?.fileName, buffer);
    const sha256 = createHash("sha256").update(buffer).digest("hex");
    const inserted = await this.pool.query(
      `INSERT INTO roleplay_assets (kind, name, source_sha256, data)
       VALUES ($1, $2, $3, $4::jsonb)
       ON CONFLICT (source_sha256) DO NOTHING
       RETURNING id, kind, name, data, imported_at`,
      [asset.kind, asset.name, sha256, JSON.stringify(asset.data)],
    );
    if (inserted.rows[0]) return { asset: assetSummary(inserted.rows[0]), created: true };
    const existing = await this.pool.query(
      "SELECT id, kind, name, data, imported_at FROM roleplay_assets WHERE source_sha256 = $1",
      [sha256],
    );
    return { asset: assetSummary(existing.rows[0]), created: false };
  }

  async character(db, characterId) {
    const { rows } = await db.query("SELECT id, name FROM characters WHERE id = $1", [characterId]);
    if (!rows[0]) throw new RoleplayError(404, "직원을 찾을 수 없습니다.");
    return rows[0];
  }

  async profileRow(db, characterId) {
    const { rows } = await db.query("SELECT * FROM roleplay_profiles WHERE character_id = $1", [characterId]);
    return profileFromRow(characterId, rows[0]);
  }

  async loadAssets(db, profile) {
    const { rows } = profile.presetId
      ? await db.query("SELECT data FROM roleplay_assets WHERE id = $1 AND kind = 'preset'", [profile.presetId])
      : { rows: [] };
    const preset = rows[0]?.data ?? null;
    const { rows: allModules } = await db.query(
      "SELECT id, data FROM roleplay_assets WHERE kind = 'module' ORDER BY imported_at, id",
    );
    return { preset, modules: activeModules(preset, allModules, profile.moduleIds) };
  }

  async readProfile(characterId) {
    await this.character(this.pool, characterId);
    const profile = await this.profileRow(this.pool, characterId);
    const { preset, modules } = await this.loadAssets(this.pool, profile);
    const toggleDefinitions = [
      ...parseToggleDefinitions(preset?.customPromptTemplateToggle),
      ...modules.flatMap((module) => parseToggleDefinitions(module.data?.customModuleToggle)),
    ];
    const messages = profile.currentChatId ? await this.messages(this.pool, profile.currentChatId) : [];
    return {
      profile,
      toggleDefinitions,
      activeModuleIds: modules.map((module) => module.id),
      messages,
      generating: this.generating.has(characterId),
      lastError: this.lastErrors.get(characterId) ?? null,
    };
  }

  async messages(db, chatId) {
    const { rows } = await db.query(
      "SELECT id, role, content, created_at FROM roleplay_messages WHERE chat_id = $1 ORDER BY position",
      [chatId],
    );
    return rows.map((row) => ({ id: row.id, role: row.role, content: row.content, createdAt: row.created_at }));
  }

  async updateProfile(characterId, body = {}) {
    await this.character(this.pool, characterId);
    const current = await this.profileRow(this.pool, characterId);
    const next = { ...current };
    if (body.enabled !== undefined) {
      if (typeof body.enabled !== "boolean") throw new RoleplayError(400, "enabled는 참/거짓이어야 합니다.");
      next.enabled = body.enabled;
    }
    if (body.presetId !== undefined) next.presetId = body.presetId === null ? null : uuid(body.presetId, "presetId");
    if (body.moduleIds !== undefined) {
      if (!Array.isArray(body.moduleIds)) throw new RoleplayError(400, "moduleIds는 배열이어야 합니다.");
      next.moduleIds = [...new Set(body.moduleIds.map((id) => uuid(id, "moduleIds")))];
    }
    if (body.toggles !== undefined) next.toggles = toggleValues(body.toggles);
    for (const [key, field] of [
      ["characterDescription", "캐릭터 설명"],
      ["firstMessage", "첫 메시지"],
      ["persona", "페르소나"],
    ]) {
      if (body[key] !== undefined) next[key] = text(body[key], field);
    }
    if (body.userName !== undefined) next.userName = text(body.userName, "사용자 이름", { max: 100, required: true }).trim();
    if (body.model !== undefined) next.model = text(body.model, "모델", { max: 200, required: true }).trim();
    if (body.localProfileId !== undefined) {
      // 빈 문자열과 null은 안티그래비티 프록시로 되돌린다.
      next.localProfileId = body.localProfileId === null || body.localProfileId === ""
        ? null
        : text(body.localProfileId, "로컬 프로필", { max: 200 });
      if (next.localProfileId) await this.localDefinition(next.localProfileId, characterId);
    }

    const assetIds = [next.presetId, ...next.moduleIds].filter(Boolean);
    if (assetIds.length > 0) {
      const { rows } = await this.pool.query(
        "SELECT id, kind FROM roleplay_assets WHERE id = ANY($1::uuid[])",
        [assetIds],
      );
      const kinds = new Map(rows.map((row) => [row.id, row.kind]));
      if (next.presetId && kinds.get(next.presetId) !== "preset") {
        throw new RoleplayError(400, "가져온 프리셋이 아닙니다.");
      }
      if (next.moduleIds.some((id) => kinds.get(id) !== "module")) {
        throw new RoleplayError(400, "가져온 모듈이 아닙니다.");
      }
    }

    await this.pool.query(
      `INSERT INTO roleplay_profiles (
         character_id, enabled, preset_id, module_ids, toggles, character_description,
         first_message, user_name, persona, model, current_chat_id, local_profile_id, updated_at
       ) VALUES ($1, $2, $3, $4::uuid[], $5::jsonb, $6, $7, $8, $9, $10, $11, $12, now())
       ON CONFLICT (character_id) DO UPDATE SET
         enabled = EXCLUDED.enabled,
         preset_id = EXCLUDED.preset_id,
         module_ids = EXCLUDED.module_ids,
         toggles = EXCLUDED.toggles,
         character_description = EXCLUDED.character_description,
         first_message = EXCLUDED.first_message,
         user_name = EXCLUDED.user_name,
         persona = EXCLUDED.persona,
         model = EXCLUDED.model,
         local_profile_id = EXCLUDED.local_profile_id,
         updated_at = now()`,
      [
        characterId,
        next.enabled,
        next.presetId,
        next.moduleIds,
        JSON.stringify(next.toggles),
        next.characterDescription,
        next.firstMessage,
        next.userName,
        next.persona,
        next.model,
        next.currentChatId,
        next.localProfileId,
      ],
    );
    this.broadcast({ type: "roleplay.changed", characterId });
    return await this.readProfile(characterId);
  }

  async createChat(characterId) {
    await this.pool.query(
      "INSERT INTO roleplay_profiles (character_id) VALUES ($1) ON CONFLICT (character_id) DO NOTHING",
      [characterId],
    );
    const { rows } = await this.pool.query(
      "INSERT INTO roleplay_chats (character_id) VALUES ($1) RETURNING id",
      [characterId],
    );
    await this.pool.query(
      "UPDATE roleplay_profiles SET current_chat_id = $2, updated_at = now() WHERE character_id = $1",
      [characterId, rows[0].id],
    );
    return rows[0].id;
  }

  async startNewChat(characterId) {
    await this.character(this.pool, characterId);
    if (this.generating.has(characterId)) throw new RoleplayError(409, "답을 만드는 중에는 새 대화를 시작할 수 없습니다.");
    await this.createChat(characterId);
    this.lastErrors.delete(characterId);
    this.broadcast({ type: "roleplay.changed", characterId });
    return await this.readProfile(characterId);
  }

  // 사용자 메시지를 저장하고 202로 돌려준 뒤, 답은 뒤에서 만들어 저장한다.
  async sendMessage(characterId, body = {}) {
    const content = text(body.content, "메시지", { max: MAX_MESSAGE, required: true });
    const character = await this.character(this.pool, characterId);
    if (this.generating.has(characterId)) throw new RoleplayError(409, "이전 답을 만드는 중입니다.");
    let profile = await this.profileRow(this.pool, characterId);
    if (!profile.presetId) throw new RoleplayError(409, "롤플레이 프리셋을 먼저 고르세요.");
    const { preset, modules } = await this.loadAssets(this.pool, profile);
    if (!preset) throw new RoleplayError(409, "고른 프리셋을 찾을 수 없습니다.");

    const job = { startedAt: new Date().toISOString() };
    this.generating.set(characterId, job);
    try {
      if (!profile.currentChatId) profile = { ...profile, currentChatId: await this.createChat(characterId) };
      const history = await this.messages(this.pool, profile.currentChatId);
      const context = this.parseContext(profile, character, history.length);
      const input = applyRegexScripts(this.scripts(preset, modules), content, "editinput", context);
      await this.pool.query(
        "INSERT INTO roleplay_messages (chat_id, role, content) VALUES ($1, 'user', $2)",
        [profile.currentChatId, input],
      );
    } catch (error) {
      this.generating.delete(characterId);
      throw error;
    }
    this.lastErrors.delete(characterId);
    this.broadcast({ type: "roleplay.changed", characterId });
    job.promise = this.generate(characterId, character, profile, preset, modules)
      .catch((error) => {
        this.lastErrors.set(characterId, error instanceof RoleplayError ? error.message : "답을 만들지 못했습니다.");
        if (!(error instanceof RoleplayError)) console.warn("롤플레이 답 생성 실패:", error?.message ?? error);
      })
      .finally(() => {
        this.generating.delete(characterId);
        this.broadcast({ type: "roleplay.changed", characterId });
      });
    return { accepted: true, chatId: profile.currentChatId };
  }

  scripts(preset, modules) {
    return [...(preset?.regex ?? []), ...modules.flatMap((module) => module.data?.regex ?? [])];
  }

  parseContext(profile, character, messageCount) {
    return {
      userName: profile.userName,
      charName: character.name,
      vars: Object.fromEntries(Object.entries(profile.toggles).map(([key, value]) => [`toggle_${key}`, value])),
      chatIndex: messageCount,
      lastMessageIndex: messageCount,
    };
  }

  async generate(characterId, character, profile, preset, modules) {
    const history = await this.messages(this.pool, profile.currentChatId);
    const formatted = assembleRoleplayMessages({
      preset,
      modules,
      toggles: profile.toggles,
      characterName: character.name,
      characterDescription: profile.characterDescription,
      firstMessage: profile.firstMessage,
      userName: profile.userName,
      persona: profile.persona,
      messages: history,
    });
    const requestBody = roleplayRequestBody({ preset, model: profile.model, formatted });
    const reply = profile.localProfileId
      ? await this.callLocalModel(requestBody, await this.localDefinition(profile.localProfileId, characterId))
      : await this.callModel(requestBody);
    const context = this.parseContext(profile, character, history.length);
    const output = applyRegexScripts(this.scripts(preset, modules), reply, "editoutput", context);
    await this.pool.query(
      "INSERT INTO roleplay_messages (chat_id, role, content) VALUES ($1, 'char', $2)",
      [profile.currentChatId, output],
    );
  }

  // 직원 업무와 같은 프로필을 쓰면 그 직원의 주소·추론 설정을 따라야 같은 로컬 연결을 함께 쓴다.
  async localDefinition(localProfileId, characterId) {
    const { rows } = await this.pool.query(
      `SELECT p.definition, c.config
         FROM local_agent_profiles p
         LEFT JOIN characters c ON c.id = $2
        WHERE p.id = $1 AND p.enabled = true`,
      [localProfileId, characterId],
    );
    if (!rows[0]) throw new RoleplayError(400, "로컬 프로필이 꺼져 있거나 없습니다.");
    let definition;
    try {
      definition = normalizeLocalDefinition(rows[0].definition);
      const config = rows[0].config ?? {};
      // 주소는 프로필이 아니라 직원의 PC 경로라 프로필이 달라도 따른다. 추론 설정은 같은 프로필일 때만 따른다.
      const reasoning = config.localProfileId === localProfileId ? config.localReasoning : null;
      definition = normalizeLocalDefinition({
        profile: reasoning ? { ...definition.profile, reasoning } : definition.profile,
        host: config.localHostAddress ? { ...definition.host, address: config.localHostAddress } : definition.host,
      });
    } catch {
      throw new RoleplayError(400, "로컬 프로필 설정을 읽지 못했습니다.");
    }
    if (definition.profile.backend !== "claude") {
      throw new RoleplayError(400, "Messages 형식을 쓰는 로컬 프로필만 롤플레이에 연결할 수 있습니다.");
    }
    return definition;
  }

  async callLocalModel(requestBody, definition) {
    const providers = this.localProviders();
    if (!providers) throw new RoleplayError(503, "로컬 모델 서비스가 준비되지 않았습니다.");
    let door;
    try {
      door = await providers.openMessagesDoor(definition);
    } catch (error) {
      throw new RoleplayError(503, error?.message || "로컬 모델 연결을 열지 못했습니다.");
    }
    try {
      // 로컬 브리지는 프로필 모델명과 출력 한도를 그대로 요구한다.
      const body = {
        ...requestBody,
        model: door.model,
        max_tokens: Math.min(requestBody.max_tokens ?? door.maxOutputTokens, door.maxOutputTokens),
      };
      return await this.callModel(body, { url: door.url, key: door.token, timeoutMs: LOCAL_REQUEST_TIMEOUT_MS });
    } finally {
      await door.release();
    }
  }

  async callModel(requestBody, { url = this.apiURL, key, timeoutMs = REQUEST_TIMEOUT_MS } = {}) {
    if (key === undefined) {
      try {
        key = (await readFile(this.apiKeyFile, "utf8")).trim();
      } catch {
        throw new RoleplayError(503, "롤플레이 모델 접근 키 파일을 읽지 못했습니다.");
      }
    }
    let response;
    try {
      response = await this.fetchImpl(url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "anthropic-version": "2023-06-01",
          "x-api-key": key,
        },
        body: JSON.stringify(requestBody),
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (error) {
      throw new RoleplayError(502, error?.name === "TimeoutError"
        ? "모델 응답이 너무 오래 걸려 멈췄습니다."
        : url === this.apiURL
          ? "롤플레이 모델 서버에 연결하지 못했습니다. 프록시가 켜져 있는지 확인하세요."
          : "로컬 모델에 연결하지 못했습니다.");
    }
    const raw = await response.text();
    let body = null;
    try {
      body = JSON.parse(raw);
    } catch {
      // 아래에서 상태 코드로 알린다.
    }
    if (!response.ok) {
      // 로컬 문은 error를 문자열로, 프록시는 error.message로 보낸다.
      const message = typeof body?.error === "string" ? body.error : body?.error?.message;
      const detail = typeof message === "string" ? message.slice(0, 300) : "";
      throw new RoleplayError(502, `모델 서버가 ${response.status}로 거절했습니다.${detail ? ` ${detail}` : ""}`);
    }
    const reply = responseText(body);
    if (!reply.trim()) throw new RoleplayError(502, "모델이 빈 답을 보냈습니다.");
    return reply;
  }
}
