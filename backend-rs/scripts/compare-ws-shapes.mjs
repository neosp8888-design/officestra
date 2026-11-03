// Node 백엔드와 Rust 백엔드 소스에서 웹소켓 방송 객체를 찾아 종류별 최상위 키 집합을 대조한다(R22).
// 운영 백엔드에 이벤트를 일으키는 쓰기 없이 형태를 비교하려는 정적 대조다. 변수로 조립한 객체는 잡지 못한다.
// 사용: node scripts/compare-ws-shapes.mjs [--node Node소스폴더] [--rust Rust소스폴더]
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const option = (name, fallback) => {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
};
const NODE_ROOT = option("--node", "/Users/neo/office/backend/src");
const RUST_ROOT = option("--rust", new URL("../src", import.meta.url).pathname);
const BROADCAST_TYPES = /^(feed|local|roleplay|session|terminal|workspace|reply-routes|model-catalog)\./;

const walk = (root, suffix) => readdirSync(root).flatMap((name) => {
  const path = join(root, name);
  if (statSync(path).isDirectory()) return name === "node_modules" || name === "tests" ? [] : walk(path, suffix);
  return path.endsWith(suffix) ? [path] : [];
});

// index를 품은 가장 안쪽 { ... } 안쪽 글을 돌려준다. 문자열 안 괄호는 따로 다루지 않는다.
function enclosing(text, index) {
  let depth = 0;
  let start = -1;
  for (let cursor = index; cursor >= 0; cursor -= 1) {
    if (text[cursor] === "}") depth += 1;
    if (text[cursor] === "{") {
      if (depth === 0) {
        start = cursor;
        break;
      }
      depth -= 1;
    }
  }
  if (start < 0) return null;
  depth = 0;
  for (let cursor = start; cursor < text.length; cursor += 1) {
    if (text[cursor] === "{") depth += 1;
    if (text[cursor] === "}") {
      depth -= 1;
      if (depth === 0) return text.slice(start + 1, cursor);
    }
  }
  return null;
}

// 깊이 0의 키만 모은다. Node 쪽 전개(...)는 따로 표시해 형태를 다 모른다는 것을 드러낸다.
function topKeys(body, rust) {
  const keys = new Set();
  let depth = 0;
  let segment = "";
  const flush = () => {
    const trimmed = segment.trim();
    segment = "";
    if (!trimmed) return;
    if (!rust && trimmed.startsWith("...")) {
      keys.add("...전개");
      return;
    }
    const match = rust ? trimmed.match(/^"([A-Za-z_]+)"\s*:/) : trimmed.match(/^["']?([A-Za-z_$][\w$]*)["']?\s*(?::|$)/);
    if (match) keys.add(match[1]);
  };
  for (const char of body) {
    if ("{[(".includes(char)) depth += 1;
    if ("}])".includes(char)) depth -= 1;
    if (char === "," && depth === 0) {
      flush();
      continue;
    }
    segment += char;
  }
  flush();
  return [...keys].sort();
}

function collect(files, pattern, rust) {
  const shapes = new Map();
  for (const file of files) {
    let text = readFileSync(file, "utf8");
    // 파일 안에 중괄호로 연 단위 시험 모듈은 방송이 아니라 시험 입력이다(`mod tests;` 선언은 자르지 않는다).
    const inlineTests = rust ? text.search(/#\[cfg\(test\)\]\s*mod\s+\w+\s*\{/) : -1;
    if (inlineTests >= 0) text = text.slice(0, inlineTests);
    for (const match of text.matchAll(pattern)) {
      if (!BROADCAST_TYPES.test(match[1])) continue;
      const body = enclosing(text, match.index);
      if (!body) continue;
      const shape = `${match[1]} {${topKeys(body, rust).join(",")}}`;
      if (!shapes.has(shape)) shapes.set(shape, []);
      shapes.get(shape).push(`${file.slice(file.lastIndexOf("/src/") + 5)}:${text.slice(0, match.index).split("\n").length}`);
    }
  }
  return shapes;
}

const node = collect(walk(NODE_ROOT, ".mjs"), /type\s*:\s*["']([a-z-]+\.[a-z.-]+)["']/g, false);
const rust = collect(walk(RUST_ROOT, ".rs"), /"type"\s*:\s*"([a-z-]+\.[a-z.-]+)"/g, true);
const shapes = [...new Set([...node.keys(), ...rust.keys()])].sort();
let differing = 0;
for (const shape of shapes) {
  const mark = node.has(shape) && rust.has(shape) ? "같음" : node.has(shape) ? "Node만" : "Rust만";
  console.log(`${mark} ${shape} 지점 Node ${node.get(shape)?.length ?? 0}·Rust ${rust.get(shape)?.length ?? 0}`);
  if (mark !== "같음") {
    differing += 1;
    console.log(`  ${(node.get(shape) ?? rust.get(shape)).slice(0, 4).join(" ")}`);
  }
}
console.log(`형태 ${shapes.length}종 중 차이 ${differing}`);
process.exitCode = differing === 0 ? 0 : 1;
