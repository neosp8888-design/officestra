// 이 파일은 보관함 대화 삭제가 파생 기록과 세션 재개 상태를 함께 정리하는지 검증한다.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  TurnDeletionError,
  deleteArchivedTurn,
} from "../src/turn-deletion.mjs";

const serverSource = readFileSync(
  new URL("../src/server.mjs", import.meta.url),
  "utf8",
);

function fakePool(respond) {
  const queries = [];
  let released = false;
  const client = {
    async query(statement, parameters = []) {
      queries.push({ statement, parameters });
      return respond(statement, parameters, queries.length);
    },
    release() {
      released = true;
    },
  };
  return {
    queries,
    get released() {
      return released;
    },
    async connect() {
      return client;
    },
  };
}

const empty = { rowCount: 0, rows: [] };

test("없는 대화 삭제는 변경 없이 null을 반환한다", async () => {
  const pool = fakePool((statement) => {
    if (/FROM turns AS turn/.test(statement)) return empty;
    return empty;
  });

  assert.equal(await deleteArchivedTurn(pool, "missing-turn"), null);
  assert.equal(pool.released, true);
  assert.equal(pool.queries[0].statement, "BEGIN");
  assert.equal(pool.queries.at(-1).statement, "COMMIT");
});

test("진행 중인 대화는 롤백하고 삭제하지 않는다", async () => {
  const pool = fakePool((statement) => {
    if (/FROM turns AS turn/.test(statement)) {
      return {
        rowCount: 1,
        rows: [{
          id: "turn-1",
          status: "running",
          sessionId: "session-1",
          characterId: "right-woman",
        }],
      };
    }
    return empty;
  });

  await assert.rejects(
    deleteArchivedTurn(pool, "turn-1"),
    (error) => error instanceof TurnDeletionError
      && error.statusCode === 409,
  );
  assert.ok(pool.queries.some(({ statement }) => statement === "ROLLBACK"));
  assert.ok(!pool.queries.some(({ statement }) => /DELETE FROM turns/.test(statement)));
});

test("종료 대화는 파생 기록을 지우고 활성 세션 재개를 끊는다", async () => {
  const pool = fakePool((statement) => {
    if (/FROM turns AS turn/.test(statement)) {
      return {
        rowCount: 1,
        rows: [{
          id: "11111111-1111-4111-8111-111111111111",
          status: "completed",
          sessionId: "22222222-2222-4222-8222-222222222222",
          characterId: "right-woman",
        }],
      };
    }
    if (/status IN \('pending', 'running'\)/.test(statement)) return empty;
    if (/SELECT id\s+FROM work_records/.test(statement)) {
      return {
        rowCount: 2,
        rows: [
          { id: "33333333-3333-4333-8333-333333333333" },
          { id: "44444444-4444-4444-8444-444444444444" },
        ],
      };
    }
    if (/DELETE FROM active_cli_sessions/.test(statement)) {
      return { rowCount: 1, rows: [] };
    }
    if (/DELETE FROM work_records/.test(statement)) {
      return { rowCount: 2, rows: [] };
    }
    if (/DELETE FROM turns/.test(statement)) {
      return { rowCount: 1, rows: [] };
    }
    return empty;
  });

  assert.deepEqual(
    await deleteArchivedTurn(
      pool,
      "11111111-1111-4111-8111-111111111111",
    ),
    {
      id: "11111111-1111-4111-8111-111111111111",
      characterId: "right-woman",
      deletedWorkRecordCount: 2,
      sessionReset: true,
    },
  );
  const sql = pool.queries.map(({ statement }) => statement).join("\n");
  assert.match(sql, /DELETE FROM turn_response_sources/);
  assert.match(sql, /DELETE FROM wiki_proposals/);
  assert.match(sql, /DELETE FROM work_record_events/);
  assert.match(sql, /DELETE FROM work_record_links/);
  assert.match(sql, /DELETE FROM work_records/);
  assert.match(sql, /DELETE FROM turns/);
  assert.match(sql, /UPDATE cli_sessions/);
  assert.equal(pool.queries.at(-1).statement, "COMMIT");
  assert.equal(pool.released, true);
});

test("DELETE turns API는 JSON 요청만 받고 피드 전체 갱신을 알린다", () => {
  assert.match(serverSource, /request\.method === "DELETE" && turnID/);
  assert.match(serverSource, /await deleteTurn\(response, turnID\)/);
  assert.match(serverSource, /costChanged: true/);
});
