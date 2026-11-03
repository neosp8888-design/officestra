// Node 백엔드 시험이 부르는 함수의 인자와 JSON.stringify 결과를 녹화해 Rust 시험의 고정 자료로 남긴다.
//
// office 저장소 backend 폴더에서 실행한다. 시험 파일이 가져가는 대상 모듈만 기록용 모듈로 바뀐다.
//   OFFICE_RS_RECORD="agent-event-parser.mjs:parseAgentEvent,decodeAgentResponse" \
//   OFFICE_RS_RECORD_OUT=/Users/neo/office-rs/tests/fixtures/agent-event-parser-cases.json \
//   node --import /Users/neo/office-rs/tests/fixtures/record-node-calls.mjs \
//     --test --test-isolation=none test/agent-event-parser.test.mjs

import { writeFileSync } from "node:fs";
import { registerHooks } from "node:module";

const [moduleName, names] = String(process.env.OFFICE_RS_RECORD ?? "").split(":");
const functions = String(names ?? "").split(",").filter(Boolean);
const output = process.env.OFFICE_RS_RECORD_OUT;
if (!moduleName || functions.length === 0 || !output) {
  throw new Error("OFFICE_RS_RECORD와 OFFICE_RS_RECORD_OUT을 지정하세요.");
}

const PREFIX = "office-rs-record:";
const RECORD = Symbol.for("office-rs.record");
const cases = [];

globalThis[RECORD] = (fn, args, run) => {
  const entry = { fn, args };
  cases.push(entry);
  try {
    const result = run();
    entry.result = JSON.stringify(result) ?? null;
    return result;
  } catch (error) {
    entry.error = String(error?.message);
    throw error;
  }
};

registerHooks({
  resolve(specifier, context, nextResolve) {
    const resolved = nextResolve(specifier, context);
    const fromTest = String(context.parentURL ?? "").includes("/backend/test/");
    return fromTest && resolved.url.endsWith(`/backend/src/${moduleName}`)
      ? { url: `${PREFIX}${resolved.url}`, shortCircuit: true }
      : resolved;
  },
  load(url, context, nextLoad) {
    if (!url.startsWith(PREFIX)) {
      return nextLoad(url, context);
    }
    const real = JSON.stringify(url.slice(PREFIX.length));
    const source = [
      `import * as real from ${real};`,
      `export * from ${real};`,
      ...functions.map((name) =>
        `export function ${name}(...args) { return globalThis[Symbol.for("office-rs.record")](` +
        `${JSON.stringify(name)}, args, () => real.${name}(...args)); }`
      ),
    ].join("\n");
    return { format: "module", source, shortCircuit: true };
  },
});

process.on("exit", () => {
  const lines = cases.map((entry) => JSON.stringify(entry)).join(",\n");
  writeFileSync(output, `{"node":${JSON.stringify(process.version)},"module":${JSON.stringify(moduleName)},"cases":[\n${lines}\n]}\n`);
});
