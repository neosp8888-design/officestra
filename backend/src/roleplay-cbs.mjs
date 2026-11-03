// 리스 CBS({{…}}) 문법 중 누렁이·소악마 프리셋과 모듈이 쓰는 부분을 리스와 같은 규칙으로 해석한다.
// 원본: RisuAI(GPL-3.0) src/ts/parser/parser.svelte.ts, src/ts/cbs.ts, src/ts/process/infunctions.ts

const OPERATORS = {
  "+": { precedence: 2, associativity: "Left" },
  "-": { precedence: 2, associativity: "Left" },
  "*": { precedence: 3, associativity: "Left" },
  "/": { precedence: 3, associativity: "Left" },
  "^": { precedence: 4, associativity: "Left" },
  "%": { precedence: 3, associativity: "Left" },
  "<": { precedence: 1, associativity: "Left" },
  ">": { precedence: 1, associativity: "Left" },
  "|": { precedence: 1, associativity: "Left" },
  "&": { precedence: 1, associativity: "Left" },
  "≤": { precedence: 1, associativity: "Left" },
  "≥": { precedence: 1, associativity: "Left" },
  "=": { precedence: 1, associativity: "Left" },
  "≠": { precedence: 1, associativity: "Left" },
  "!": { precedence: 5, associativity: "Right" },
};
const OPERATOR_KEYS = Object.keys(OPERATORS);
const isNumberToken = (token) => Boolean(parseFloat(token)) || token === "0";

function toRPN(source) {
  const expression = source.replace(/\s+/g, "");
  const tokens = [];
  let lastToken = "";
  for (let i = 0; i < expression.length; i++) {
    const char = expression[i];
    if (char === "-" && (i === 0 || OPERATOR_KEYS.includes(expression[i - 1]) || expression[i - 1] === "(")) {
      lastToken += char;
    } else if (OPERATOR_KEYS.includes(char)) {
      tokens.push(lastToken !== "" ? lastToken : "0");
      lastToken = "";
      tokens.push(char);
    } else {
      lastToken += char;
    }
  }
  tokens.push(lastToken !== "" ? lastToken : "0");

  let output = "";
  const stack = [];
  for (const token of tokens) {
    if (isNumberToken(token)) {
      output += `${token} `;
    } else if (OPERATOR_KEYS.includes(token)) {
      while (stack.length > 0 && (
        (OPERATORS[token].associativity === "Left" &&
          OPERATORS[token].precedence <= OPERATORS[stack.at(-1)].precedence) ||
        (OPERATORS[token].associativity === "Right" &&
          OPERATORS[token].precedence < OPERATORS[stack.at(-1)].precedence)
      )) {
        output += `${stack.pop()} `;
      }
      stack.push(token);
    }
  }
  while (stack.length > 0) output += `${stack.pop()} `;
  return output.trim();
}

function calculateRPN(expression) {
  const stack = [];
  for (const token of expression.split(" ")) {
    if (isNumberToken(token)) {
      stack.push(parseFloat(token));
      continue;
    }
    const b = stack.pop();
    const a = stack.pop();
    switch (token) {
      case "+": stack.push(a + b); break;
      case "-": stack.push(a - b); break;
      case "*": stack.push(a * b); break;
      case "/": stack.push(a / b); break;
      case "^": stack.push(a ** b); break;
      case "%": stack.push(a % b); break;
      case "<": stack.push(a < b ? 1 : 0); break;
      case ">": stack.push(a > b ? 1 : 0); break;
      case "|": stack.push(a || b); break;
      case "&": stack.push(a && b); break;
      case "≤": stack.push(a <= b ? 1 : 0); break;
      case "≥": stack.push(a >= b ? 1 : 0); break;
      case "=": stack.push(a === b ? 1 : 0); break;
      case "≠": stack.push(a !== b ? 1 : 0); break;
      case "!": stack.push(b ? 0 : 1); break;
    }
  }
  return stack.length === 0 ? 0 : stack.pop();
}

function executeCalculation(text, context) {
  const numeric = (value) => {
    const parsed = parseFloat(value);
    return Number.isNaN(parsed) ? "0" : parsed.toString();
  };
  const prepared = text
    .replace(/\$([a-zA-Z0-9_]+)/g, () => "0")
    .replace(/@([a-zA-Z0-9_]+)/g, (_, key) => numeric(globalVar(context, key)))
    .replace(/&&/g, "&")
    .replace(/\|\|/g, "|")
    .replace(/<=/g, "≤")
    .replace(/>=/g, "≥")
    .replace(/==/g, "=")
    .replace(/!=/g, "≠")
    .replace(/null/gi, "0");
  return calculateRPN(toRPN(prepared));
}

export function calcString(text, context = {}) {
  const depth = [""];
  for (const char of text) {
    if (char === "(") {
      depth.push("");
    } else if (char === ")" && depth.length > 1) {
      const result = executeCalculation(depth.pop(), context);
      depth[depth.length - 1] += result;
    } else {
      depth[depth.length - 1] += char;
    }
  }
  return executeCalculation(depth.join(""), context);
}

function globalVar(context, key) {
  return context.vars?.[key] ?? "null";
}

function randomPick(args, random) {
  if (args.length === 0) return random.toString();
  const items = args.length === 1
    ? args[0].replace(/\\,/g, "§X").split(/:|,/g)
    : args;
  const item = items[Math.floor(random * items.length)];
  return String(item).replace(/§X/g, ",");
}

function parseArray(text) {
  try {
    const array = JSON.parse(text);
    if (Array.isArray(array)) return array;
  } catch {
    // 리스처럼 § 구분 문자열로 본다.
  }
  return String(text).split("§");
}

function makeArray(items) {
  return JSON.stringify(items.map((item) => (typeof item === "string" ? item.replace(/::/g, "\\u003A\\u003A") : item)));
}

function rollDice(args, random) {
  if (args.length === 0) return "1";
  const notation = args[0].split("d");
  let count = 1;
  let sides = 6;
  if (notation.length === 2) {
    count = Number(notation[0] || 1);
    sides = Number(notation[1] || 6);
  } else if (notation.length === 1) {
    sides = Number(notation[0]);
  }
  if (Number.isNaN(count) || Number.isNaN(sides) || count < 1 || sides < 1) return "NaN";
  let total = 0;
  for (let i = 0; i < count; i++) total += Math.floor(random() * sides) + 1;
  return total.toString();
}

function dateTimeFormat(format, time = 0) {
  const date = time === 0 ? new Date() : new Date(time);
  if (!format) return "";
  if (format.startsWith(":")) format = format.substring(1);
  if (format.length > 300) return "";
  return format
    .replace(/YYYY/g, date.getFullYear().toString())
    .replace(/YY/g, date.getFullYear().toString().substring(2))
    .replace(/MMMM/g, Intl.DateTimeFormat("en", { month: "long" }).format(date))
    .replace(/MMM/g, Intl.DateTimeFormat("en", { month: "short" }).format(date))
    .replace(/MM/g, (date.getMonth() + 1).toString().padStart(2, "0"))
    .replace(/DDDD/g, Math.floor((date.getTime() - new Date(date.getFullYear(), 0, 0).getTime()) / (1000 * 60 * 60 * 24)).toString())
    .replace(/DD/g, date.getDate().toString().padStart(2, "0"))
    .replace(/dddd/g, Intl.DateTimeFormat("en", { weekday: "long" }).format(date))
    .replace(/ddd/g, Intl.DateTimeFormat("en", { weekday: "short" }).format(date))
    .replace(/HH/g, date.getHours().toString().padStart(2, "0"))
    .replace(/hh/g, (date.getHours() % 12 || 12).toString().padStart(2, "0"))
    .replace(/mm/g, date.getMinutes().toString().padStart(2, "0"))
    .replace(/ss/g, date.getSeconds().toString().padStart(2, "0"))
    .replace(/X/g, Math.floor(date.getTime() / 1000).toString())
    .replace(/x/g, date.getTime().toString())
    .replace(/A/g, date.getHours() >= 12 ? "PM" : "AM");
}

function currentTime(args, context) {
  const now = context.now ? context.now() : new Date();
  if (args.length === 0) return `${now.getHours()}:${now.getMinutes()}:${now.getSeconds()}`;
  let time = 0;
  if (args[1]) {
    time = Number(args[1]) / 1000;
    if (Number.isNaN(time)) time = 0;
  }
  return dateTimeFormat(args[0], time || (context.now ? now.getTime() : 0));
}

// 앱 메시지의 createdAt을 리스 메시지의 time처럼 쓴다.
function idleDuration(context) {
  const messages = context.messages ?? [];
  if (messages.length === 0) return "00:00:00";
  const sentAt = new Date(messages.at(-1).createdAt ?? NaN).getTime();
  if (Number.isNaN(sentAt)) return "[Cannot get time, message was sent in older version]";
  const now = context.now ? context.now().getTime() : Date.now();
  let seconds = Math.floor((now - sentAt) / 1000);
  let minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  seconds %= 60;
  minutes %= 60;
  return `${hours}:${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}

const ESCAPES = { "{": "", "}": "", "(": "", ")": "" };
const UNESCAPES = ["{", "}", "(", ")", "&lt;", "&gt;", ":", ";"];
const risuEscape = (text) => text.replace(/[{}()]/g, (char) => ESCAPES[char]);

// 리스는 요청을 보내기 직전에 #escape로 숨긴 글자를 되돌린다.
export function risuUnescape(text) {
  return String(text).replace(/[-]/g, (char) => UNESCAPES[char.charCodeAt(0) - 0xE9B8]);
}

const FUNCTIONS = new Map([
  ["user", (args, context) => context.userName ?? "User"],
  ["char", (args, context) => context.charName ?? "Char"],
  ["bot", (args, context) => context.charName ?? "Char"],
  ["getglobalvar", (args, context) => globalVar(context, args[0])],
  ["length", (args) => (args[0] ?? "").length.toString()],
  ["greaterequal", (args) => (Number(args[0]) >= Number(args[1]) ? "1" : "0")],
  ["chatindex", (args, context) => String(context.chatIndex ?? -1)],
  ["lastmessageid", (args, context) => String(context.lastMessageIndex ?? -1)],
  ["random", (args, context) => randomPick(args, (context.random ?? Math.random)())],
  ["equal", (args) => (args[0] === args[1] ? "1" : "0")],
  ["notequal", (args) => (args[0] !== args[1] ? "1" : "0")],
  ["greater", (args) => (Number(args[0]) > Number(args[1]) ? "1" : "0")],
  ["less", (args) => (Number(args[0]) < Number(args[1]) ? "1" : "0")],
  ["and", (args) => (args[0] === "1" && args[1] === "1" ? "1" : "0")],
  ["or", (args) => (args[0] === "1" || args[1] === "1" ? "1" : "0")],
  ["not", (args) => (args[0] === "1" ? "0" : "1")],
  ["any", (args) => ((args.length > 1 ? args : parseArray(args[0])).some((item) => item === "1") ? "1" : "0")],
  ["contains", (args) => (args[0].includes(args[1]) ? "1" : "0")],
  ["replace", (args) => args[0].replaceAll(args[1], args[2])],
  ["trim", (args) => args[0].trim()],
  ["blank", () => ""],
  ["u", (args) => String.fromCharCode(parseInt(args[0], 16))],
  ["ruby", (args) => `<ruby>${args[0]}<rp> (</rp><rt>${args[1]}</rt><rp>) </rp></ruby>`],
  ["button", (args) => `<button class="button-default" risu-trigger="${args[1]}">${args[0]}</button>`],
  ["makearray", (args) => makeArray(args)],
  ["tempvar", (args, context) => context.tempVars?.[args[0]] ?? ""],
  ["settempvar", (args, context) => {
    if (context.tempVars) context.tempVars[args[0]] = args[1];
    return "";
  }],
  ["roll", (args, context) => rollDice(args, context.random ?? Math.random)],
  ["time", (args, context) => currentTime(args, context)],
  ["idleduration", (args, context) => idleDuration(context)],
  ["previouschatlog", (args, context) => context.messages?.[Number(args[0])]?.content ?? "Out of range"],
]);
FUNCTIONS.set("greater_equal", FUNCTIONS.get("greaterequal"));
FUNCTIONS.set("chat_index", FUNCTIONS.get("chatindex"));
FUNCTIONS.set("lastmessageindex", FUNCTIONS.get("lastmessageid"));
FUNCTIONS.set("none", FUNCTIONS.get("blank"));
FUNCTIONS.set("unicodedecodefromhex", FUNCTIONS.get("u"));
FUNCTIONS.set("furigana", FUNCTIONS.get("ruby"));
FUNCTIONS.set("array", FUNCTIONS.get("makearray"));
FUNCTIONS.set("a", FUNCTIONS.get("makearray"));
FUNCTIONS.set("gettempvar", FUNCTIONS.get("tempvar"));

// 리스처럼 함수 하나가 실패하면 그 매크로만 글자 그대로 남긴다.
function callFunction(body, context) {
  try {
    if (body.startsWith("? ")) return calcString(body.substring(2), context).toString();
    const colonIndex = body.indexOf(":");
    const parts = colonIndex !== -1 && body[colonIndex + 1] === ":"
      ? body.split("::")
      : body.split(":");
    const name = parts[0].toLocaleLowerCase().replace(/[\s_-]/g, "");
    const handler = FUNCTIONS.get(name);
    if (handler) return handler(parts.slice(1), context);
  } catch {
    // 아래에서 null을 돌려 원문을 남긴다.
  }
  return null;
}

function blockStart(body) {
  if (body.startsWith("#if") || body.startsWith("#if_pure ")) {
    const state = body.split(" ", 2)[1];
    if (state === "true" || state === "1") {
      return { type: body.startsWith("#if_pure") ? "ifpure" : "parse" };
    }
    return { type: "ignore" };
  }
  if (body.startsWith("#escape")) {
    return { type: "escape", mode: body.substring(7).trim() === "::keep" ? "keep" : undefined };
  }
  if (body.startsWith("#each")) {
    let rest = body.substring(5).trim();
    let mode;
    if (rest.startsWith("::keep ")) {
      mode = "keep";
      rest = rest.substring(7).trim();
    }
    if (rest.startsWith("as ")) rest = rest.substring(3).trim();
    return { type: "each", type2: rest, mode };
  }
  return { type: "nothing" };
}

// 안쪽을 해석하지 않고 그대로 모아 두는 블록.
const PURE_BLOCKS = new Set(["ignore", "each", "escape"]);

const trimLines = (text) => text.split("\n").map((line) => line.trimStart()).join("\n").trim();

function blockEnd(text, block) {
  switch (block.type) {
    case "parse": return trimLines(text.trim());
    case "ifpure": return text;
    case "each": return block.mode === "keep" ? text : trimLines(text.trim());
    case "escape": return risuEscape(block.mode === "keep" ? text : text.trim());
    default: return "";
  }
}

// #each 본문을 배열 항목마다 펼친다. 펼친 글은 원문에 다시 넣어 이어서 해석한다.
function expandEach(body, block) {
  const asIndex = block.type2.lastIndexOf(" as ");
  let name = block.type2.substring(asIndex + 4).trim();
  let array = parseArray(block.type2.substring(0, asIndex));
  if (asIndex === -1) {
    const spaceIndex = block.type2.lastIndexOf(" ");
    if (spaceIndex === -1) return null;
    name = block.type2.substring(spaceIndex + 1);
    array = parseArray(block.type2.substring(0, spaceIndex));
  }
  let added = "";
  for (const item of array) {
    added += body.replaceAll(`{{slot::${name}}}`, typeof item === "string" ? item : JSON.stringify(item));
  }
  return block.mode === "keep" ? added : added.trim();
}

const STACK_FUNCTION = 1;
const STACK_BLOCK = 5;
const STACK_PURE_CHILD = 6;

export function parseCBS(source, baseContext = {}) {
  let text = String(source ?? "").replace(/<(user|char|bot)>/gi, "{{$1}}");
  // 리스처럼 임시 변수는 해석 한 번 안에서만 산다.
  const context = { ...baseContext, tempVars: {} };
  const nested = [""];
  const stackType = [];
  const blockTypes = new Map();
  const pureBlocks = new Set();
  const isPure = () => pureBlocks.size > 0;
  let pointer = 0;

  while (pointer < text.length) {
    const char = text[pointer];
    if (char === "{" && text[pointer + 1] === "{") {
      pointer++;
      nested.unshift("");
      stackType[nested.length] = STACK_FUNCTION;
    } else if (char === "}" && text[pointer + 1] === "}" && nested.length > 1 && stackType[nested.length] === STACK_FUNCTION) {
      pointer++;
      const body = nested.shift();
      handleClose(body);
    } else {
      nested[0] += char;
    }
    pointer++;
  }

  function handleClose(body) {
    if (body.startsWith("#") || body.startsWith(":")) {
      if (isPure()) {
        nested[0] += `{{${body}}}`;
        if (body !== ":else") {
          nested.unshift("");
          stackType[nested.length] = STACK_PURE_CHILD;
        }
        return;
      }
      const block = blockStart(body);
      if (block.type === "nothing") {
        nested[0] += `{{${body}}}`;
        return;
      }
      nested.unshift("");
      stackType[nested.length] = STACK_BLOCK;
      blockTypes.set(nested.length, block);
      if (PURE_BLOCKS.has(block.type)) pureBlocks.add(nested.length);
      return;
    }
    if (body.startsWith("/") && !body.startsWith("//")) {
      if (stackType[nested.length] === STACK_BLOCK) {
        const block = blockTypes.get(nested.length);
        pureBlocks.delete(nested.length);
        blockTypes.delete(nested.length);
        const inner = nested.shift();
        const result = blockEnd(inner, block);
        if (block.type === "each") {
          const added = expandEach(result, block);
          if (added !== null) text = text.substring(0, pointer + 1) + added + text.substring(pointer + 1);
          return;
        }
        if (result !== "") nested[0] += result;
        return;
      }
      if (stackType[nested.length] === STACK_PURE_CHILD) {
        const inner = nested.shift();
        nested[0] += `${inner}{{${body}}}`;
        return;
      }
    }
    const result = isPure() ? null : callFunction(body, context);
    nested[0] += result === null ? `{{${body}}}` : result;
  }

  if (nested.length === 1) return nested[0];
  let rest = "";
  while (nested.length > 1) {
    const opener = stackType[nested.length] === STACK_FUNCTION ? "{{" : "<";
    rest = opener + nested.shift() + rest;
  }
  return nested[0] + rest;
}
