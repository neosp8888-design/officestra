// 리스 CBS({{…}}) 문법 중 누렁이 프리셋·모듈이 쓰는 부분을 리스와 같은 규칙으로 해석한다.
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
]);
FUNCTIONS.set("greater_equal", FUNCTIONS.get("greaterequal"));
FUNCTIONS.set("chat_index", FUNCTIONS.get("chatindex"));
FUNCTIONS.set("lastmessageindex", FUNCTIONS.get("lastmessageid"));

function callFunction(body, context) {
  if (body.startsWith("? ")) return calcString(body.substring(2), context).toString();
  const colonIndex = body.indexOf(":");
  const parts = colonIndex !== -1 && body[colonIndex + 1] === ":"
    ? body.split("::")
    : body.split(":");
  const name = parts[0].toLocaleLowerCase().replace(/[\s_-]/g, "");
  const handler = FUNCTIONS.get(name);
  return handler ? handler(parts.slice(1), context) : null;
}

function blockStart(body) {
  if (body.startsWith("#if") || body.startsWith("#if_pure ")) {
    const state = body.split(" ", 2)[1];
    if (state === "true" || state === "1") {
      return body.startsWith("#if_pure") ? "ifpure" : "parse";
    }
    return "ignore";
  }
  return "nothing";
}

const trimLines = (text) => text.split("\n").map((line) => line.trimStart()).join("\n").trim();

function blockEnd(text, type) {
  if (type === "parse") return trimLines(text.trim());
  if (type === "ifpure") return text;
  return "";
}

const STACK_FUNCTION = 1;
const STACK_BLOCK = 5;
const STACK_PURE_CHILD = 6;

export function parseCBS(source, context = {}) {
  const text = String(source ?? "").replace(/<(user|char|bot)>/gi, "{{$1}}");
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
      const type = blockStart(body);
      if (type === "nothing") {
        nested[0] += `{{${body}}}`;
        return;
      }
      nested.unshift("");
      stackType[nested.length] = STACK_BLOCK;
      blockTypes.set(nested.length, type);
      if (type === "ignore") pureBlocks.add(nested.length);
      return;
    }
    if (body.startsWith("/") && !body.startsWith("//")) {
      if (stackType[nested.length] === STACK_BLOCK) {
        const type = blockTypes.get(nested.length);
        pureBlocks.delete(nested.length);
        blockTypes.delete(nested.length);
        const inner = nested.shift();
        const result = blockEnd(inner, type);
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
