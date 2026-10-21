// 리스 프리셋의 프롬프트 템플릿·토글·정규식으로 롤플레이 요청을 리스와 같은 순서로 조립한다.
// 원본: RisuAI(GPL-3.0) src/ts/process/index.svelte.ts, scripts.ts, request/anthropic.ts
import { parseCBS } from "./roleplay-cbs.mjs";

export function parseToggleDefinitions(template) {
  if (!template) return [];
  const toggles = [];
  for (const line of String(template).split("\n")) {
    const [key, value, type, option] = line.split("=");
    if (type === "group" || type === "groupEnd" || type === "divider") {
      toggles.push({ key, value: value ?? "", type, options: [] });
    } else if (type === "caption" && value) {
      toggles.push({ key, value, type, options: [] });
    } else if (key && value) {
      toggles.push({
        key,
        value,
        type: ["select", "text", "textarea"].includes(type) ? type : "checkbox",
        options: option?.split(",") ?? [],
      });
    }
  }
  return toggles;
}

export function toggleVariables(values = {}) {
  const vars = {};
  for (const [key, value] of Object.entries(values)) {
    if (typeof value === "string") vars[`toggle_${key}`] = value;
  }
  return vars;
}

// 프리셋의 moduleIntergration(쉼표 구분 네임스페이스)에 든 모듈과 직접 켠 모듈을 쓴다.
export function activeModules(preset, modules, enabledModuleIds = []) {
  const namespaces = String(preset?.moduleIntergration ?? "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  return modules.filter((module) =>
    enabledModuleIds.includes(module.id) ||
    (module.data?.namespace && namespaces.includes(module.data.namespace))
  );
}

function regexScripts(preset, modules) {
  const scripts = [...(preset?.regex ?? [])];
  for (const module of modules) scripts.push(...(module.data?.regex ?? []));
  return scripts;
}

function parsedScripts(scripts) {
  let orderChanged = false;
  const result = scripts.map((script) => {
    if (!(script.ableFlag && script.flag?.includes("<"))) {
      return { script, order: 0, actions: [] };
    }
    let order = 0;
    const actions = [];
    const flag = script.flag.replace(/<(.+?)>/g, (_, meta) => {
      for (const item of meta.split(",").map((value) => value.trim())) {
        if (item.startsWith("order ")) {
          order = parseInt(item.substring(6), 10);
          orderChanged = true;
        } else {
          actions.push(item);
        }
      }
      return "";
    });
    return { script: { ...script, flag }, order, actions };
  });
  if (orderChanged) result.sort((a, b) => b.order - a.order);
  return result;
}

function replaceMatchGroups(template, matched) {
  return template
    .replace(/(?<!\$)\$[0-9]+/g, (value) => {
      const index = parseInt(value.substring(1), 10);
      return index < matched.length ? matched[index] : value;
    })
    .replace(/\$&/g, matched[0])
    .replace(/(?<!\$)\$<([^>]+)>/g, (value, name) => matched.groups?.[name] ?? value);
}

// processScriptFull의 정규식 단계. 감정·주입·되풀이 같은 리스 화면 전용 동작은 다루지 않는다.
export function applyRegexScripts(scripts, input, mode, context) {
  let data = parseCBS(input, context);
  for (const { script, actions } of parsedScripts(scripts)) {
    if (script.type !== mode || !script.in) continue;
    try {
      let out = String(script.out ?? "").replaceAll("$n", "\n").replace(/{{data}}/g, "$&");
      const moves = out.startsWith("@@move_top") || out.startsWith("@@move_bottom") ||
        actions.includes("move_top") || actions.includes("move_bottom");
      let flag = script.ableFlag ? (script.flag || "g") : "g";
      if (moves) flag = flag.replace("g", "");
      if (out.endsWith(">") && !actions.includes("no_end_nl")) out += "\n";
      flag = flag.trim().replace(/[^dgimsuvy]/g, "");
      flag = [...new Set(flag)].join("") || "u";
      const pattern = actions.includes("cbs") ? parseCBS(script.in, context) : script.in;
      const regex = new RegExp(pattern, flag);

      if (out.startsWith("@@") || actions.length > 0) {
        if (!regex.test(data)) continue;
        regex.lastIndex = 0;
        if (moves) {
          const matches = flag.includes("g") ? [...data.matchAll(regex)] : [data.match(regex)];
          data = data.replace(regex, "");
          for (const matched of matches) {
            if (!matched) continue;
            const moved = replaceMatchGroups(
              out.replace("@@move_top ", "").replace("@@move_bottom ", ""),
              matched,
            );
            data = out.startsWith("@@move_top") || actions.includes("move_top")
              ? `${moved}\n${data}`
              : `${data}\n${moved}`;
          }
        } else if (!out.startsWith("@@emo ") && !out.startsWith("@@inject") && !actions.includes("inject")) {
          data = parseCBS(data.replace(regex, out), context);
        }
      } else {
        data = parseCBS(data.replace(regex, out), context);
      }
    } catch {
      // 리스도 깨진 정규식 하나는 건너뛰고 나머지를 적용한다.
    }
  }
  return data;
}

const PROMPT_ROLE = { system: "system", user: "user", bot: "assistant" };
const removePositions = (text) => String(text ?? "").replace(/{{position::(.+?)}}/g, "");

function sliceChats(chats, card) {
  let start = card.rangeStart;
  let end = card.rangeEnd === "end" ? chats.length : card.rangeEnd;
  if (start === -1000) {
    start = 0;
    end = chats.length;
  }
  if (start < 0) start = Math.max(0, chats.length + start);
  if (end < 0) end = Math.max(0, chats.length + end);
  return start >= end ? [] : chats.slice(start, end);
}

// 프리셋 블록 순서대로 OpenAI식 메시지 목록을 만든다(리스 sendChat의 템플릿 단계).
export function assembleRoleplayMessages({
  preset,
  modules = [],
  toggles = {},
  characterName,
  characterDescription = "",
  firstMessage = "",
  userName,
  persona = "",
  messages = [],
}) {
  const scripts = regexScripts(preset, modules);
  const vars = toggleVariables(toggles);
  const baseContext = {
    userName,
    charName: characterName,
    vars,
    lastMessageIndex: messages.length - 1,
  };
  const parse = (text, extra = {}) => parseCBS(text, { ...baseContext, ...extra });

  const description = { role: "system", content: parse(characterDescription) };
  const personaPrompts = persona.trim() ? [{ role: "system", content: parse(persona) }] : [];

  const chats = [];
  if (!preset?.promptSettings?.trimStartNewChat) {
    chats.push({ role: "system", content: "[Start a new chat]", memo: "NewChat" });
  }
  if (firstMessage) {
    chats.push({
      role: "assistant",
      content: applyRegexScripts(scripts, parse(firstMessage), "editprocess", { ...baseContext, chatIndex: -1 }),
    });
  }
  messages.forEach((message, index) => {
    let content = applyRegexScripts(
      scripts,
      parse(message.content),
      "editprocess",
      { ...baseContext, chatIndex: index },
    );
    content = content.replace(/<Thoughts>(.+)<\/Thoughts>/gms, "");
    chats.push({ role: message.role === "user" ? "user" : "assistant", content, memo: message.id });
  });
  const history = chats.filter((chat) => chat.content.trim() !== "");

  const formatted = [];
  const push = (items) => {
    for (const item of items) {
      if (!item.content.trim()) continue;
      const last = formatted.at(-1);
      if (item.role === "system" && last?.role === "system" && last.memo === item.memo) {
        last.content += `\n\n${item.content}`;
      } else {
        formatted.push({ ...item });
      }
    }
  };
  const withRole = (items, role) => items.map((item) => (role ? { ...item, role: PROMPT_ROLE[role] } : { ...item }));
  const withInnerFormat = (items, innerFormat) => {
    if (!innerFormat || items.length === 0) return items;
    return items.map((item) => ({
      ...item,
      content: parse(removePositions(innerFormat)).replace("{{slot}}", item.content),
    }));
  };

  for (const card of preset?.promptTemplate ?? []) {
    switch (card.type) {
      case "persona":
        push(withInnerFormat(withRole(personaPrompts, card.role2), card.innerFormat));
        break;
      case "description":
        push(withInnerFormat(withRole([description], card.role2), card.innerFormat));
        break;
      case "plain":
      case "jailbreak":
      case "cot": {
        if (card.type !== "plain") break;
        const role = PROMPT_ROLE[card.role] ?? "system";
        push([{ role, content: parse(removePositions(card.text), { role: card.role }) }]);
        break;
      }
      case "chat":
        push(sliceChats(history, card));
        break;
      // 장기 기억·로어북·작가 노트는 1단계에서 비어 있다. cache는 문자열 요청에서 쓰이지 않는다.
      default:
        break;
    }
  }
  return formatted.map((item) => ({ ...item, content: item.content.trim() }));
}

// requestClaude의 Anthropic 변환. 앞쪽 system은 system으로, 이후 system은 "System: "을 붙인 user로 보낸다.
export function anthropicPayload(formatted) {
  let system = "";
  const messages = [];
  const add = (role, content) => {
    const last = messages.at(-1);
    if (last?.role === role) last.content += `\n\n${content}`;
    else messages.push({ role, content });
  };
  for (const item of formatted) {
    if (item.role === "user" || item.role === "assistant") {
      add(item.role, item.content);
    } else if (item.role === "system") {
      if (messages.length === 0) system += `\n\n${item.content}`;
      else add("user", `System: ${item.content}`);
    }
  }
  if (messages.length === 0) {
    if (!system) throw new Error("보낼 내용이 없습니다.");
    messages.push({ role: "user", content: "Start" });
    system = "";
  }
  if (messages[0].role !== "user") messages.unshift({ role: "user", content: "Start" });
  return { system: system.trim(), messages };
}

// 리스 LLMFlags 번호. 사용자 지정 플래그가 꺼져 있으면 Custom API 기본값을 쓴다.
const FLAG_FULL_SYSTEM = 6;
const FLAG_FIRST_SYSTEM = 7;
const FLAG_ALTERNATE_ROLE = 9;
const FLAG_START_WITH_USER = 10;
const CUSTOM_API_FLAGS = [FLAG_FULL_SYSTEM, 8];

// request.ts의 reformater. 앞쪽 system만 남기고 나머지 system은 "system: "을 붙여 다른 역할로 바꾼다.
export function reformatForModelFlags(formatted, preset) {
  const flags = preset?.enableCustomFlags && Array.isArray(preset.customFlags)
    ? preset.customFlags
    : CUSTOM_API_FLAGS;
  let items = formatted.map((item) => ({ ...item }));
  let systemPrompt = null;
  if (!flags.includes(FLAG_FULL_SYSTEM)) {
    if (flags.includes(FLAG_FIRST_SYSTEM)) {
      while (items[0]?.role === "system") {
        if (systemPrompt) systemPrompt.content += `\n\n${items[0].content}`;
        else systemPrompt = items[0];
        items = items.slice(1);
      }
    }
    for (const item of items) {
      if (item.role !== "system") continue;
      item.content = preset?.systemContentReplacement
        ? preset.systemContentReplacement.replace("{{slot}}", item.content)
        : `system: ${item.content}`;
      item.role = preset?.systemRoleReplacement || "user";
    }
  }
  if (flags.includes(FLAG_ALTERNATE_ROLE)) {
    const merged = [];
    for (const item of items) {
      const last = merged.at(-1);
      if (last?.role === item.role) last.content += `\n${item.content}`;
      else merged.push(item);
    }
    items = merged;
  }
  if (flags.includes(FLAG_START_WITH_USER) && items[0]?.role !== "user") {
    items.unshift({ role: "user", content: " " });
  }
  if (systemPrompt) items.unshift(systemPrompt);
  return items;
}

export function roleplayRequestBody({ preset, model, formatted }) {
  const { system, messages } = anthropicPayload(reformatForModelFlags(formatted, preset));
  const body = {
    model,
    messages,
    max_tokens: Number(preset?.maxResponse) > 0 ? Number(preset.maxResponse) : 4096,
    stream: false,
  };
  if (system) body.system = system;
  if (Number.isFinite(Number(preset?.temperature)) && Number(preset.temperature) >= 0) {
    body.temperature = Number(preset.temperature) / 100;
  }
  const budget = Number(preset?.thinkingTokens);
  if (preset?.thinkingType === "budget" && budget > 0 && budget < body.max_tokens) {
    body.thinking = { type: "enabled", budget_tokens: budget, display: "summarized" };
  }
  return body;
}
