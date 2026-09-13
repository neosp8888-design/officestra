// Antigravity 로컬 대화 DB의 컨텍스트와 누적 사용량 해석을 검증한다.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";

import {
  antigravityContextUsage,
  antigravitySessionUsage,
  antigravityTurnRequestUsages,
  parseAntigravityGeneratorMetadata,
  parseAntigravityStepMetadata,
} from "../src/antigravity-local-state.mjs";

const sessionID = "11111111-2222-4333-8444-555555555555";

test("Antigravity 턴 요청 원장을 SQLite 인덱스 또는 시각으로 분리한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-agy-requests-"));
  const database = new DatabaseSync(join(root, `${sessionID}.db`));
  try {
    database.exec("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB)");
    for (const idx of [1, 2, 3]) {
      database.prepare("INSERT INTO steps VALUES (?, 15, ?)").run(idx, stepMetadata({
        executionID: `request-${idx}`, seconds: idx * 1000,
        inputTokens: 100000, outputTokens: 100, cachedInputTokens: 50000,
        reasoningOutputTokens: 10,
      }));
    }
    const byIndex = antigravityTurnRequestUsages(sessionID, { root, afterIndex: 1, endedAt: new Date(3000000) });
    const byTime = antigravityTurnRequestUsages(sessionID, { root,
      startedAt: new Date(2000000), endedAt: new Date(3000000),
    });
    assert.deepEqual(byIndex, byTime);
    assert.equal(byIndex.length, 2);
    assert.equal(byIndex[0].usage.inputTokens, 100000);
    assert.equal(antigravityTurnRequestUsages(sessionID, { root }), null);
  } finally { database.close(); rmSync(root, { recursive: true, force: true }); }
});

test("Antigravity protobuf 메타데이터에서 실제 컨텍스트 값을 읽는다", () => {
  assert.deepEqual(
    parseAntigravityGeneratorMetadata(generatorMetadata({
      executionID: "execution-1",
      usedTokens: 58_559,
      limitTokens: 256_000,
    })),
    {
      executionID: "execution-1",
      usedTokens: 58_559,
      limitTokens: 256_000,
    },
  );
});

test("Antigravity 응답 단계에서 토큰과 시각을 읽는다", () => {
  assert.deepEqual(
    parseAntigravityStepMetadata(stepMetadata({
      executionID: "execution-1",
      seconds: 1_800_000_000,
      inputTokens: 1_200,
      outputTokens: 30,
      cachedInputTokens: 900,
      reasoningOutputTokens: 12,
    })),
    {
      executionID: "execution-1",
      at: 1_800_000_000_000,
      usage: {
        inputTokens: 1_200,
        outputTokens: 30,
        cachedInputTokens: 900,
        reasoningOutputTokens: 12,
      },
    },
  );
});

test("대화 DB는 시점별 컨텍스트와 중단 복구용 누적 사용량을 제공한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-agy-state-"));
  const path = join(root, `${sessionID}.db`);
  const database = new DatabaseSync(path);
  try {
    database.exec(`
      CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
      CREATE TABLE steps (
        idx INTEGER PRIMARY KEY,
        step_type INTEGER,
        metadata BLOB
      );
    `);
    const insertGenerator = database.prepare(
      "INSERT INTO gen_metadata (idx, data) VALUES (?, ?)",
    );
    const insertStep = database.prepare(
      "INSERT INTO steps (idx, step_type, metadata) VALUES (?, 15, ?)",
    );
    insertGenerator.run(1, generatorMetadata({
      executionID: "execution-1",
      usedTokens: 50,
      limitTokens: 200,
    }));
    insertStep.run(1, stepMetadata({
      executionID: "execution-1",
      seconds: 1_000,
      inputTokens: 10,
      outputTokens: 2,
      cachedInputTokens: 30,
      reasoningOutputTokens: 1,
    }));
    insertGenerator.run(2, generatorMetadata({
      executionID: "execution-2",
      usedTokens: 100,
      limitTokens: 200,
    }));
    insertStep.run(2, stepMetadata({
      executionID: "execution-2",
      seconds: 2_000,
      inputTokens: 20,
      outputTokens: 3,
      cachedInputTokens: 40,
      reasoningOutputTokens: 2,
    }));
  } finally {
    database.close();
  }

  try {
    assert.deepEqual(
      antigravityContextUsage({
        sessionID,
        at: 1_500_000,
        root,
      }),
      { usedTokens: 50, limitTokens: 200 },
    );
    assert.deepEqual(antigravitySessionUsage(sessionID, { root }), {
      inputTokens: 30,
      outputTokens: 5,
      cachedInputTokens: 70,
      reasoningOutputTokens: 3,
      cacheWriteInputTokens: null,
      cacheWrite5mInputTokens: null,
      cacheWrite1hInputTokens: null,
      serviceTier: null,
      speed: null,
      inferenceGeo: null,
      reportedCostUsd: null,
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

function generatorMetadata({ executionID, usedTokens, limitTokens }) {
  return message(
    bytesField(1, message(
      bytesField(9, message(
        bytesField(10, message(
          numberField(1, usedTokens),
          numberField(4, limitTokens),
        )),
      )),
    )),
    bytesField(4, Buffer.from(executionID)),
  );
}

function stepMetadata({
  executionID,
  seconds,
  inputTokens,
  outputTokens,
  cachedInputTokens,
  reasoningOutputTokens,
}) {
  return message(
    bytesField(1, message(numberField(1, seconds))),
    bytesField(9, message(
      numberField(2, inputTokens),
      numberField(3, outputTokens),
      numberField(5, cachedInputTokens),
      numberField(9, reasoningOutputTokens),
    )),
    bytesField(12, Buffer.from(executionID)),
  );
}

function message(...fields) {
  return Buffer.concat(fields);
}

function numberField(number, value) {
  return Buffer.concat([varint(BigInt(number << 3)), varint(BigInt(value))]);
}

function bytesField(number, value) {
  const buffer = Buffer.from(value);
  return Buffer.concat([
    varint(BigInt((number << 3) | 2)),
    varint(BigInt(buffer.length)),
    buffer,
  ]);
}

function varint(value) {
  const bytes = [];
  let current = value;
  do {
    let byte = Number(current & 0x7fn);
    current >>= 7n;
    if (current > 0n) byte |= 0x80;
    bytes.push(byte);
  } while (current > 0n);
  return Buffer.from(bytes);
}
