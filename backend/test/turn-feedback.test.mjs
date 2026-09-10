// 이 파일은 대화 평가의 유효값과 저장·해제 계약을 검증한다.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  TurnFeedbackValidationError,
  normalizeTurnFeedback,
  replaceTurnFeedback,
} from "../src/turn-feedback.mjs";

const migrationSource = readFileSync(
  new URL(
    "../../database/migrations/019_turn_response_feedback.sql",
    import.meta.url,
  ),
  "utf8",
);
const serverSource = readFileSync(
  new URL("../src/server.mjs", import.meta.url),
  "utf8",
);

test("대화 평가는 좋아요·싫어요·미선택만 허용한다", () => {
  assert.equal(normalizeTurnFeedback("liked"), "liked");
  assert.equal(normalizeTurnFeedback("disliked"), "disliked");
  assert.equal(normalizeTurnFeedback(null), null);
  assert.throws(
    () => normalizeTurnFeedback("neutral"),
    TurnFeedbackValidationError,
  );
  assert.throws(
    () => normalizeTurnFeedback(undefined),
    TurnFeedbackValidationError,
  );
});

test("미선택 평가는 행을 만들지 않고 기존 평가를 삭제한다", async () => {
  const queries = [];
  const client = {
    async query(statement, parameters) {
      queries.push({ statement, parameters });
      if (/SELECT id, status/.test(statement)) {
        return {
          rowCount: 1,
          rows: [{ id: "turn-1", status: "completed" }],
        };
      }
      return { rowCount: 0, rows: [] };
    },
  };

  const result = await replaceTurnFeedback(client, "turn-1", null);

  assert.deepEqual(result, { outcome: "stored", feedback: null });
  assert.match(queries[1].statement, /DELETE FROM turn_response_feedback/);
  assert.deepEqual(queries[1].parameters, ["turn-1"]);
});

test("완료 응답 평가는 한 행에 덮어써서 저장한다", async () => {
  const queries = [];
  const client = {
    async query(statement, parameters) {
      queries.push({ statement, parameters });
      if (/SELECT id, status/.test(statement)) {
        return {
          rowCount: 1,
          rows: [{ id: "turn-1", status: "completed" }],
        };
      }
      return { rowCount: 1, rows: [{ feedback: parameters[1] }] };
    },
  };

  const result = await replaceTurnFeedback(client, "turn-1", "disliked");

  assert.deepEqual(result, {
    outcome: "stored",
    feedback: "disliked",
  });
  assert.match(queries[1].statement, /ON CONFLICT \(turn_id\) DO UPDATE/);
  assert.deepEqual(queries[1].parameters, ["turn-1", "disliked"]);
});

test("진행 중 대화에는 평가를 저장하지 않는다", async () => {
  const client = {
    async query() {
      return {
        rowCount: 1,
        rows: [{ id: "turn-1", status: "running" }],
      };
    },
  };

  assert.deepEqual(
    await replaceTurnFeedback(client, "turn-1", "liked"),
    { outcome: "unavailable" },
  );
});

test("DB는 평가가 없는 보통 상태를 행 부재로 표현한다", () => {
  assert.match(
    migrationSource,
    /turn_id uuid PRIMARY KEY REFERENCES turns\(id\) ON DELETE CASCADE/,
  );
  assert.match(
    migrationSource,
    /feedback IN \('liked', 'disliked'\)/,
  );
  assert.doesNotMatch(migrationSource, /DEFAULT\s+'(?:liked|disliked)'/);
});

test("피드와 평가 API는 같은 turn 평가를 조회하고 갱신한다", () => {
  assert.match(
    serverSource,
    /request\.method === "PUT" && turnFeedbackID/,
  );
  assert.ok(
    serverSource.includes("^\\/api\\/turns\\/([^/]+)\\/feedback$"),
  );
  assert.match(
    serverSource,
    /LEFT JOIN turn_response_feedback AS turn_feedback/,
  );
  assert.match(serverSource, /turn_feedback\.feedback/);
  assert.match(
    serverSource,
    /feedbackChanged: true/,
  );
});
