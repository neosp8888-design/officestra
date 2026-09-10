// 이 파일은 화이트보드 상세 사용 현황 집계의 입력 검증과 SQL 계약을 검증한다.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  CharacterTurnCostSummaryCache,
  USAGE_REPORT_DAY_SPAN,
  USAGE_REPORT_MONTH_SPAN,
  UsageReportError,
  normalizeUsageReportOptions,
  readUsageReport,
  usageReportQuery,
} from "../src/usage-report.mjs";

test("직원 전체 비용은 페이지·모델과 무관하며 미확인 비용과 진행 중 턴을 제외한다", async () => {
  const cache = new CharacterTurnCostSummaryCache({ async query(sql) {
    assert.match(sql, /GROUP BY character.id/);
    assert.match(sql, /COUNT\(usage.cost_usd\)/);
    assert.match(sql, /SUM\(usage.cost_usd\)/);
    assert.match(sql, /EXTRACT\(EPOCH FROM/);
    assert.match(sql, /turn\.ended_at > turn\.started_at/);
    assert.match(sql, /feedback\.feedback = 'liked'/);
    assert.match(sql, /feedback\.feedback = 'disliked'/);
    assert.match(sql, /LEFT JOIN turn_response_feedback AS feedback/);
    assert.match(sql, /'completed', 'failed', 'interrupted'/);
    assert.doesNotMatch(sql, /LIMIT|external_id|active_cli_sessions|turn.model/);
    return { rows: [{
      characterId: "boss",
      pricedTurnCount: 4,
      totalCostUsd: 12,
      timedCostUsd: 10,
      totalDurationSeconds: 120,
      likedCount: 3,
      dislikedCount: 1,
    }] };
  } });
  const snapshot = await cache.read();
  assert.deepEqual(snapshot.characters, [{
    characterId: "boss",
    pricedTurnCount: 4,
    totalCostUsd: 12,
    timedCostUsd: 10,
    totalDurationSeconds: 120,
    likedCount: 3,
    dislikedCount: 1,
  }]);
});

test("평균 비용·평가 캐시는 동시 조회·스트리밍 이벤트에서 재집계하지 않고 관련 변경만 반영한다", async () => {
  let calls = 0;
  const cache = new CharacterTurnCostSummaryCache({ async query() {
    calls += 1;
    return { rows: [{ characterId: "boss", pricedTurnCount: calls, totalCostUsd: calls * 2 }] };
  } });
  const snapshots = await Promise.all(Array.from({ length: 50 }, () => cache.read()));
  assert.equal(calls, 1);
  for (const snapshot of snapshots) assert.equal(snapshot, snapshots[0]);
  for (let i = 0; i < 100; i += 1) {
    cache.observe({ type: "feed.changed", turnId: "streaming" });
    await cache.read();
  }
  assert.equal(calls, 1);
  cache.observe({ type: "feed.changed", costChanged: true });
  const updated = await cache.read();
  assert.equal(calls, 2);
  assert.ok(updated.version > snapshots[0].version);
  assert.equal(updated.characters[0].pricedTurnCount, 2);
  cache.observe({ type: "feed.changed", feedbackChanged: true });
  const feedbackUpdated = await cache.read();
  assert.equal(calls, 3);
  assert.ok(feedbackUpdated.version > updated.version);
});

test("집계 도중 비용이 변경되면 오래된 결과를 보내지 않고 한번 더 조회한다", async () => {
  let release;
  let calls = 0;
  const cache = new CharacterTurnCostSummaryCache({ async query() {
    calls += 1;
    if (calls === 1) await new Promise(resolve => { release = resolve; });
    return { rows: [{ characterId: "boss", pricedTurnCount: calls, totalCostUsd: calls }] };
  } });
  const pending = cache.read();
  await Promise.resolve();
  cache.observe({ type: "feed.changed", costChanged: true });
  release();
  const updated = await pending;
  assert.equal(calls, 2);
  assert.equal(updated.characters[0].totalCostUsd, 2);
});

test("백엔드·집계 단위·시간대는 허용 값만 받는다", () => {
  assert.deepEqual(
    normalizeUsageReportOptions({
      backend: "claude",
      granularity: "month",
      timeZone: " Asia/Seoul ",
    }),
    { backend: "claude", granularity: "month", timeZone: "Asia/Seoul" },
  );
  assert.deepEqual(
    normalizeUsageReportOptions({ backend: "codex" }),
    { backend: "codex", granularity: "day", timeZone: "UTC" },
  );
  for (const options of [
    { backend: "gemini" },
    { backend: null },
    { backend: "codex", granularity: "week" },
    { backend: "codex", timeZone: "Asia/Seoul; DROP TABLE turns" },
    { backend: "codex", timeZone: "" },
  ]) {
    assert.throws(
      () => normalizeUsageReportOptions(options),
      (error) => error instanceof UsageReportError && error.statusCode === 400,
    );
  }
});

test("집계 SQL은 단위별 기간 라벨과 조회 범위를 쓰고 값은 파라미터로만 넣는다", () => {
  const day = usageReportQuery("day");
  assert.match(day, /date_trunc\('day', turn\.started_at AT TIME ZONE \$2\)/);
  assert.match(day, /'YYYY-MM-DD'/);
  assert.match(day, new RegExp(`interval '${USAGE_REPORT_DAY_SPAN - 1} days'`));
  assert.match(day, /turn\.backend = \$1/);
  // 직원별 평가율이 핵심이므로 세션의 직원 ID를 함께 집계한다.
  assert.match(day, /LEFT JOIN cli_sessions AS session/);
  assert.match(day, /session\.character_id/);
  assert.match(day, /FILTER \(WHERE feedback\.feedback = 'liked'\)/);
  assert.match(day, /FILTER \(WHERE feedback\.feedback = 'disliked'\)/);

  const month = usageReportQuery("month");
  assert.match(month, /date_trunc\('month'/);
  assert.match(month, /'YYYY-MM'/);
  assert.match(
    month,
    new RegExp(`interval '${USAGE_REPORT_MONTH_SPAN - 1} months'`),
  );
});

test("응답 행은 숫자로 정규화되고 빈 모델·추론은 빈 문자열이다", async () => {
  const calls = [];
  const client = {
    async query(text, params) {
      calls.push({ text, params });
      return {
        rows: [
          {
            period: "2026-09",
            characterId: "boss",
            model: "gpt-5.6-sol",
            effort: "max",
            turns: "3",
            costUSD: "1.25",
            inputTokens: "1000",
            cachedInputTokens: "200",
            outputTokens: "50",
            liked: "2",
            disliked: "0",
          },
        ],
      };
    },
  };
  const report = await readUsageReport(client, {
    backend: "codex",
    granularity: "month",
    timeZone: "Asia/Seoul",
  });
  assert.deepEqual(calls[0].params, ["codex", "Asia/Seoul"]);
  assert.equal(report.backend, "codex");
  assert.equal(report.granularity, "month");
  assert.deepEqual(report.rows, [
    {
      period: "2026-09",
      characterId: "boss",
      model: "gpt-5.6-sol",
      effort: "max",
      turns: 3,
      costUSD: 1.25,
      inputTokens: 1000,
      cachedInputTokens: 200,
      outputTokens: 50,
      liked: 2,
      disliked: 0,
    },
  ]);
});

test("서버는 /api/usage-report GET을 집계로 연결하고 검증 오류를 상태 코드로 돌려준다", () => {
  const serverSource = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  assert.match(
    serverSource,
    /request\.method === "GET" &&\s*url\.pathname === "\/api\/usage-report"/,
  );
  assert.match(serverSource, /readUsageReport\(pool, \{/);
  assert.match(serverSource, /error instanceof UsageReportError/);
});
