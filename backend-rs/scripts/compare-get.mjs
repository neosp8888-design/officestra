// 4317(Node)과 4318(Rust)에 같은 읽기 요청을 보내 상태·형식·JSON 본문과 웹소켓 첫 메시지 형태를 비교한다(R22).
// 쓰기 요청과 바깥 모델 호출이 생기는 경로(/api/roleplay/models, force=1)는 보내지 않는다. POST는 읽기 전용인 /api/rag/search만 쓴다.
// 사용: node scripts/compare-get.mjs [--node URL] [--rust URL] [--out 폴더] [--only 경로접두어]

import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const options = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, value, index, all) => {
    if (value.startsWith("--")) pairs.push([value.slice(2), all[index + 1]]);
    return pairs;
  }, []),
);
const NODE = options.node ?? "http://127.0.0.1:4317";
const RUST = options.rust ?? "http://127.0.0.1:4318";
const OUT = options.out ?? join(tmpdir(), `office-rs-compare-get-${Date.now()}`);
const ONLY = options.only ?? null;
const ZERO = "00000000-0000-4000-8000-000000000000";
const SAMPLE_LIMIT = 200;

// 실행 중인 백엔드마다 달라지는 값. 두 쪽 모두에서 지우고 비교한다.
const RUNTIME_FIELDS = {
  "/health": ["pid", "releaseID"],
};

async function call(base, entry) {
  const { method = "GET", path, body } = entry;
  const headers = body === undefined ? {} : { Origin: new URL(base).origin, "Content-Type": "application/json" };
  const started = performance.now();
  try {
    const response = await fetch(base + path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(120_000),
    });
    const text = await response.text();
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      json = undefined;
    }
    return {
      status: response.status,
      type: (response.headers.get("content-type") ?? "").split(";")[0].trim(),
      json,
      text: json === undefined ? text : undefined,
      bytes: text.length,
      ms: Math.round(performance.now() - started),
    };
  } catch (error) {
    return { status: 0, error: String(error?.message ?? error), ms: Math.round(performance.now() - started) };
  }
}

function normalize(path, json) {
  const fields = RUNTIME_FIELDS[path.split("?")[0]];
  if (!fields || !json || typeof json !== "object") return json;
  const copy = { ...json };
  for (const field of fields) delete copy[field];
  return copy;
}

function kind(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}

function preview(value) {
  if (value === undefined) return "(없음)";
  const text = JSON.stringify(value);
  return text.length > 80 ? `${text.slice(0, 77)}...` : text;
}

// 값 차이를 경로별로 모은다. 배열 번호는 []로 묶어 같은 모양의 차이를 한 패턴으로 센다.
function diff(node, rust, path, result) {
  if (Object.is(node, rust)) return;
  const record = (where, left, right) => {
    result.count += 1;
    const pattern = where.replace(/\[\d+\]/g, "[]");
    result.patterns[pattern] = (result.patterns[pattern] ?? 0) + 1;
    if (result.items.length < SAMPLE_LIMIT) result.items.push({ path: where, node: preview(left), rust: preview(right) });
  };
  const left = kind(node);
  const right = kind(rust);
  if (left !== right) return record(path, node, rust);
  if (left === "array") {
    if (node.length !== rust.length) record(`${path}.length`, node.length, rust.length);
    for (let index = 0; index < Math.min(node.length, rust.length); index += 1) {
      diff(node[index], rust[index], `${path}[${index}]`, result);
    }
    return;
  }
  if (left === "object") {
    for (const key of new Set([...Object.keys(node), ...Object.keys(rust)])) {
      if (!(key in rust)) record(`${path}.${key}`, node[key], undefined);
      else if (!(key in node)) record(`${path}.${key}`, undefined, rust[key]);
      else diff(node[key], rust[key], `${path}.${key}`, result);
    }
    return;
  }
  record(path, node, rust);
}

function compare(entry, node, rust) {
  const result = { count: 0, patterns: {}, items: [] };
  if (node.status !== rust.status) result.status = [node.status, rust.status];
  if (node.type !== rust.type) result.type = [node.type, rust.type];
  if (node.error || rust.error) result.error = [node.error ?? null, rust.error ?? null];
  if (node.json !== undefined && rust.json !== undefined) {
    const left = normalize(entry.path, node.json);
    const right = normalize(entry.path, rust.json);
    diff(left, right, "$", result);
    if (result.count === 0 && JSON.stringify(left) !== JSON.stringify(right)) result.keyOrderOnly = true;
  } else if (node.text !== rust.text) {
    result.count += 1;
    result.items.push({ path: "$text", node: preview(node.text), rust: preview(rust.text) });
  }
  result.same = !result.status && !result.type && !result.error && result.count === 0 && !result.keyOrderOnly;
  return result;
}

// 개별 경로에 넣을 ID는 Node 응답에서 고르게 뽑는다. 복사 뒤 새로 생긴 기록을 피하려고 최근 20개 턴은 건너뛴다.
async function seeds() {
  const get = async (path) => (await call(NODE, { path })).json ?? {};
  const spread = (list, count) =>
    list.length <= count
      ? list
      : Array.from({ length: count }, (_, index) => list[Math.floor((index * (list.length - 1)) / (count - 1))]);
  const characters = (await get("/api/characters")).characters?.map((character) => character.id) ?? [];
  const history = ((await get("/api/history")).turns ?? []).slice(20);
  const byBackend = new Map();
  for (const turn of history) if (!byBackend.has(turn.backend)) byBackend.set(turn.backend, turn.id);
  const board = await get("/api/work-board");
  const pages = (await get("/api/wiki/pages")).pages ?? [];
  return {
    characters,
    turns: [...new Set([...spread(history, 6).map((turn) => turn.id), ...byBackend.values()])],
    tickets: spread(board.tickets ?? [], 5).map((ticket) => ticket.id),
    records: spread(board.workRecords ?? [], 4).map((record) => record.id),
    pages: spread(pages, 3).map((page) => page.id),
  };
}

function requests(seed) {
  const since = new Date(Date.now() - 24 * 3600 * 1000).toISOString();
  const paths = [
    "/health",
    "/api/maintenance/drain",
    "/api/characters",
    "/api/active-sessions",
    "/api/model-catalog",
    "/api/local-profiles",
    "/api/usage-summary",
    "/api/cli-updates",
    "/api/reply-routes",
    "/api/voice-follow",
    "/api/terminal-sessions",
    "/api/agent-jobs",
    "/api/roleplay/assets",
    "/api/wiki/pages",
    "/api/wiki/pages?limit=3",
    "/api/wiki/pages?query=rust",
    "/api/wiki/proposals",
    "/api/wiki/proposals?state=pending",
    "/api/work-board",
    "/api/work-records",
    "/api/work-records?limit=5&offset=5",
    "/api/work-records?state=active",
    "/api/work-records?kind=decision",
    "/api/work-records?q=rust",
    "/api/work-records?kind=bogus",
    "/api/history",
    "/api/history?characterId=right-man",
    "/api/history?from=2026-10-01T00:00:00Z&to=2026-10-02T00:00:00Z",
    "/api/live-feed",
    "/api/live-feed?limit=10",
    "/api/archive-feed",
    "/api/archive-feed?limit=5&offset=5",
    "/api/archive-feed?query=rust",
    `/api/voice-feed?characterId=right-man&since=${encodeURIComponent(since)}`,
    "/api/voice-feed",
  ];
  for (const backend of ["claude", "codex", "antigravity", "local"]) {
    for (const granularity of ["day", "week", "month"]) {
      paths.push(`/api/usage-report?backend=${backend}&granularity=${granularity}&tz=Asia%2FSeoul`);
    }
  }
  paths.push("/api/usage-report?backend=bogus", "/api/usage-report?granularity=hour");
  for (const id of seed.characters) {
    paths.push(`/api/characters/${id}/history`, `/api/agent-jobs?characterId=${id}`, `/api/roleplay/profiles/${id}`);
  }
  for (const id of seed.turns) paths.push(`/api/live-feed/${id}`, `/api/turns/${id}/sources`);
  for (const id of seed.tickets) paths.push(`/api/work-board/tickets/${id}/activity`);
  for (const id of seed.records) paths.push(`/api/work-board/records/${id}`);
  for (const id of seed.pages) paths.push(`/api/wiki/pages/${id}`);
  paths.push(
    `/api/live-feed/${ZERO}`,
    "/api/live-feed/not-a-uuid",
    "/api/characters/nobody/history",
    `/api/turns/${ZERO}/sources`,
    `/api/wiki/pages/${ZERO}`,
    "/api/roleplay/profiles/nobody",
    "/api/agent-jobs?characterId=nobody",
    `/api/workspace-reviews/${ZERO}`,
    `/api/work-board/records/${ZERO}`,
    `/api/work-board/tickets/${ZERO}/activity`,
    "/api/does-not-exist",
  );
  const entries = paths.map((path) => ({ path }));
  for (const body of [{ query: "Rust 전환 계획", limit: 5 }, { query: "텔레그램 연결", limit: 3 }, { query: "" }, {}]) {
    entries.push({ method: "POST", path: "/api/rag/search", body });
  }
  return ONLY ? entries.filter((entry) => entry.path.startsWith(ONLY)) : entries;
}

// 접속 직후 3초 동안 받은 메시지의 type과 키 목록을 모은다.
async function socketMessages(base) {
  return await new Promise((resolve) => {
    const messages = [];
    let socket;
    try {
      socket = new WebSocket(`${base.replace(/^http/, "ws")}/ws`);
    } catch (error) {
      resolve({ messages, error: String(error?.message ?? error) });
      return;
    }
    let error = null;
    socket.onmessage = (event) => {
      try {
        messages.push(JSON.parse(String(event.data)));
      } catch {
        messages.push({ raw: String(event.data).slice(0, 80) });
      }
    };
    socket.onerror = (event) => {
      error = String(event?.message ?? "websocket error");
    };
    setTimeout(() => {
      try {
        socket.close();
      } catch {}
      resolve({ messages, error });
    }, 3000);
  });
}

const shape = (message) => ({ type: message?.type ?? null, keys: Object.keys(message ?? {}).sort() });

mkdirSync(OUT, { recursive: true });
const began = Date.now();
const entries = requests(await seeds());
const results = [];
for (const entry of entries) {
  const [node, rust] = await Promise.all([call(NODE, entry), call(RUST, entry)]);
  const result = compare(entry, node, rust);
  const strip = ({ json, text, ...meta }) => meta;
  results.push({ ...entry, node: strip(node), rust: strip(rust), ...result });
  const label = `${entry.method ?? "GET"} ${entry.path}${entry.body ? ` ${JSON.stringify(entry.body)}` : ""}`;
  if (result.same) {
    console.log(`같음 ${label}`);
  } else {
    const top = Object.entries(result.patterns)
      .sort((left, right) => right[1] - left[1])
      .slice(0, 3)
      .map(([pattern, count]) => `${pattern}×${count}`)
      .join(", ");
    const parts = [`상태 ${node.status}/${rust.status}`];
    if (result.type) parts.push(`형식 ${result.type.join("/")}`);
    if (result.keyOrderOnly) parts.push("키 순서만");
    if (result.error) parts.push(`오류 ${result.error.map((value) => value ?? "-").join(" / ")}`);
    if (result.count) parts.push(`차이 ${result.count} ${top}`);
    console.log(`다름 ${label} ${parts.join(" ")}`);
  }
}
if (!ONLY || "/ws".startsWith(ONLY)) {
  const [node, rust] = await Promise.all([socketMessages(NODE), socketMessages(RUST)]);
  const result = { count: 0, patterns: {}, items: [] };
  diff(node.messages.map(shape), rust.messages.map(shape), "$", result);
  result.same = result.count === 0 && !node.error && !rust.error;
  results.push({ path: "/ws", node: { messages: node.messages.length, error: node.error }, rust: { messages: rust.messages.length, error: rust.error }, ...result });
  console.log(`${result.same ? "같음" : "다름"} WS /ws 메시지 ${node.messages.length}/${rust.messages.length} 형태 차이 ${result.count}`);
}
const same = results.filter((result) => result.same).length;
writeFileSync(join(OUT, "report.json"), `${JSON.stringify({ node: NODE, rust: RUST, began: new Date(began).toISOString(), results }, null, 2)}\n`);
console.log(`같음 ${same} / ${results.length}, ${Math.round((Date.now() - began) / 1000)}초, 보고서 ${join(OUT, "report.json")}`);
process.exitCode = same === results.length ? 0 : 1;
