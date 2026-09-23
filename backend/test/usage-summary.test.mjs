// 이 파일은 공급자 직접 한도와 PostgreSQL 사용량 집계 계약을 검증한다.

import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { PassThrough } from "node:stream";
import test from "node:test";

import {
  createUsageSummaryReader,
  parseAntigravityRateLimits,
  parseClaudeRateLimits,
  parseCodexRateLimits,
  parseCodexSubscriptionExpiration,
  probeAntigravityQuota,
  readClaudeRateLimits,
  readCodexRateLimits,
  readAntigravityRateLimits,
  readUsageActivity,
} from "../src/usage-summary.mjs";

function emptyActivityFixture(costEstimateSupported = true) {
  return {
    todayCostUSD: 0,
    recentTokens: 0,
    last30DaysCostUSD: 0,
    last30DaysTokens: 0,
    costEstimateSupported,
  };
}

test("Codex app-server 한도는 창 길이로 5시간과 7일을 구분한다", () => {
  assert.deepEqual(
    parseCodexRateLimits({
      rateLimits: {
        planType: "pro",
        primary: {
          usedPercent: 24.6,
          windowDurationMins: 300,
          resetsAt: 1_800_000_000,
        },
        secondary: {
          usedPercent: 41,
          windowDurationMins: 10_080,
          resetsAt: 1_800_600_000,
        },
      },
    }),
    {
      fiveHour: {
        remaining: 75,
        resetAt: "2027-01-15T08:00:00.000Z",
      },
      weekly: {
        remaining: 59,
        resetAt: "2027-01-22T06:40:00.000Z",
      },
      plan: "Pro 20x",
    },
  );
});

test("Codex 인증 claim에서 구독 만료 시각만 읽는다", () => {
  const payload = Buffer.from(JSON.stringify({
    "https://api.openai.com/auth": {
      chatgpt_subscription_active_until: "2026-08-31T14:54:17+00:00",
    },
    exp: 1_900_000_000,
  })).toString("base64url");
  const raw = JSON.stringify({
    tokens: { id_token: `header.${payload}.signature` },
  });

  assert.equal(
    parseCodexSubscriptionExpiration(raw),
    "2026-08-31T14:54:17.000Z",
  );
  assert.equal(parseCodexSubscriptionExpiration("{}"), null);
  assert.equal(parseCodexSubscriptionExpiration("not-json"), null);
});

test("Codex 직접 조회는 백엔드 작업 폴더와 무관한 홈에서 실행한다", async () => {
  let invocation;
  const child = new EventEmitter();
  child.stdin = new PassThrough();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.kill = () => {};
  child.stdin.setEncoding("utf8");
  child.stdin.on("data", (message) => {
    if (!message.includes('"id":2')) return;
    child.stdout.write(`${JSON.stringify({
      id: 2,
      result: {
        rateLimits: {
          planType: "pro",
          primary: {
            usedPercent: 34,
            windowDurationMins: 10_080,
            resetsAt: 1_800_000_000,
          },
        },
      },
    })}\n`);
  });
  const pool = {
    query: async () => ({
      rows: [{ path: "/opt/test/bin/codex" }],
    }),
  };

  const result = await readCodexRateLimits({
    pool,
    subscriptionExpirationReader: async () =>
      "2026-08-31T14:54:17.000Z",
    spawnProcess: (executable, arguments_, options) => {
      invocation = { executable, arguments_, options };
      return child;
    },
  });

  assert.equal(invocation.executable, "/opt/test/bin/codex");
  assert.deepEqual(invocation.arguments_, ["app-server", "--stdio"]);
  assert.equal(invocation.options.cwd, homedir());
  assert.equal(result.weekly.remaining, 66);
  assert.equal(result.plan, "Pro 20x");
  assert.equal(
    result.subscriptionExpiresAt,
    "2026-08-31T14:54:17.000Z",
  );
});

test("Claude OAuth 사용률은 남은 비율로 변환한다", () => {
  assert.deepEqual(
    parseClaudeRateLimits(
      {
        five_hour: {
          utilization: 63,
          resets_at: "2026-08-12T10:00:00Z",
        },
        seven_day: {
          utilization: 11,
          resets_at: "2026-08-13T00:00:00Z",
        },
      },
      null,
      "max",
    ),
    {
      fiveHour: {
        remaining: 37,
        resetAt: "2026-08-12T10:00:00.000Z",
      },
      weekly: {
        remaining: 89,
        resetAt: "2026-08-13T00:00:00.000Z",
      },
      plan: "Max",
    },
  );
});

test("Claude의 구체 한도 등급은 Max 배수를 보존한다", () => {
  const parsed = parseClaudeRateLimits(
    {
      five_hour: null,
      seven_day: null,
    },
    "default_claude_max_5x",
  );

  assert.equal(parsed.plan, "Max 5x");
});

test("Claude 한도 등급에 배수가 없으면 구독 종류로 표시한다", () => {
  const parsed = parseClaudeRateLimits(
    { five_hour: null, seven_day: null },
    "default_claude_ai",
    "pro",
  );

  // 티어 문자열을 그대로 쓰면 "Default Claude Ai"가 배지에 뜬다.
  assert.equal(parsed.plan, "Pro");
});

test("Claude 한도 등급도 구독 종류도 없으면 배지를 비운다", () => {
  const parsed = parseClaudeRateLimits(
    { five_hour: null, seven_day: null },
    "default_claude_ai",
    null,
  );

  assert.equal(parsed.plan, null);
});

test("Claude 직접 조회는 기존 OAuth 토큰을 읽기 전용으로 전달한다", async () => {
  let request;
  const result = await readClaudeRateLimits({
    credentialReader: async () => ({
      accessToken: "test-token",
      tier: null,
      subscriptionType: "max",
    }),
    fetchImplementation: async (url, options) => {
      request = { url, options };
      return {
        ok: true,
        status: 200,
        json: async () => ({
          five_hour: { utilization: 10, resets_at: null },
          seven_day: { utilization: 20, resets_at: null },
        }),
      };
    },
  });

  assert.equal(request.url, "https://api.anthropic.com/api/oauth/usage");
  assert.equal(request.options.method, "GET");
  assert.equal(request.options.headers.authorization, "Bearer test-token");
  assert.equal(request.options.headers["anthropic-beta"], "oauth-2025-04-20");
  assert.equal(result.fiveHour.remaining, 90);
  assert.equal(result.weekly.remaining, 80);
});

test("Claude 요금제는 로그인 정보의 옛 등급보다 계정 프로필을 따른다", async () => {
  const response = {
    ok: true,
    status: 200,
    json: async () => ({ five_hour: null, seven_day: null }),
  };
  const read = (accountPlan) => readClaudeRateLimits({
    credentialReader: async () => ({
      accessToken: "test-token",
      tier: "default_claude_max_5x",
      subscriptionType: "max",
    }),
    accountPlanReader: async () => accountPlan,
    fetchImplementation: async () => response,
  });

  assert.equal(
    (await read({ tier: "default_claude_ai", subscriptionType: "pro" })).plan,
    "Pro",
  );
  // 프로필을 못 읽으면 로그인 정보로 되돌아간다.
  assert.equal((await read(null)).plan, "Max 5x");
});

test("Antigravity Gemini 주간 잔량을 실제 quota bucket에서 읽는다", () => {
  assert.deepEqual(
    parseAntigravityRateLimits({
      response: {
        groups: [
          {
            displayName: "Gemini Models",
            buckets: [
              {
                bucketId: "gemini-weekly",
                window: "weekly",
                remainingFraction: 0.8529488,
                resetTime: "2026-09-03T13:44:43Z",
              },
            ],
          },
        ],
      },
    }),
    {
      fiveHour: null,
      weekly: {
        remaining: 85,
        resetAt: "2026-09-03T13:44:43.000Z",
      },
      plan: "Free",
    },
  );
});

test("Antigravity 5시간 한도가 있으면 Google AI Pro로 표시한다", () => {
  const result = parseAntigravityRateLimits({
    response: {
      groups: [{
        displayName: "Gemini Models",
        buckets: [
          {
            bucketId: "gemini-weekly",
            window: "weekly",
            remainingFraction: 0.91,
          },
          {
            bucketId: "gemini-5h",
            window: "5h",
            remainingFraction: 0.73,
          },
        ],
      }],
    },
  });

  assert.equal(result.fiveHour.remaining, 73);
  assert.equal(result.plan, "Google AI Pro");
});

test("Antigravity 제3자 모델 그룹도 5시간 한도 계정은 Google AI Pro다", () => {
  const result = parseAntigravityRateLimits({
    response: {
      groups: [
        {
          displayName: "Gemini Models",
          buckets: [{
            bucketId: "gemini-5h",
            window: "5h",
            remainingFraction: 0.96,
          }],
        },
        {
          displayName: "Claude and GPT models",
          buckets: [{
            bucketId: "3p-5h",
            window: "5h",
            remainingFraction: 1,
          }],
        },
      ],
    },
  });

  assert.equal(result.plan, "Google AI Pro");
});

test("Antigravity 한도 조회는 설정된 agy 실행 파일을 사용한다", async () => {
  let invocation;
  const result = await readAntigravityRateLimits({
    pool: {
      query: async () => ({ rows: [{ path: "/opt/test/bin/agy" }] }),
    },
    quotaProbe: async (options) => {
      invocation = options;
      return {
        response: {
          groups: [{
            displayName: "Gemini Models",
            buckets: [{
              bucketId: "gemini-weekly",
              remainingFraction: 0.42,
              resetTime: null,
            }],
          }],
        },
      };
    },
  });

  assert.equal(invocation.executable, "/opt/test/bin/agy");
  assert.equal(result.weekly.remaining, 42);
});

test("Antigravity 이미지 생성 쿨다운이 있으면 리셋 시각을 반영한다", async () => {
  const result = await readAntigravityRateLimits({
    pool: {
      query: async () => ({ rows: [{ path: "/opt/test/bin/agy" }] }),
    },
    quotaProbe: async () => ({
      response: {
        groups: [{
          displayName: "Gemini Models",
          buckets: [{
            bucketId: "gemini-5h",
            window: "5h",
            remainingFraction: 0.95,
            resetTime: "2026-08-30T16:55:00Z",
          }],
        }],
      },
    }),
    imageCooldownReader: async () => "2026-08-30T16:54:02.000Z",
  });

  assert.equal(result.fiveHour.remaining, 95);
  assert.equal(result.imageResetAt, "2026-08-30T16:54:02.000Z");
});

test("Antigravity 직접 한도 조회는 CSRF 토큰을 agy 플래그와 HTTP 헤더에 일치시켜 전달한다", async () => {
  let spawnArgs = null;
  let fetchOptions = null;
  const child = new EventEmitter();
  child.kill = () => {};

  const payload = {
    response: {
      groups: [{
        displayName: "Gemini Models",
        buckets: [{
          bucketId: "gemini-weekly",
          remainingFraction: 0.8,
          resetTime: "2026-09-18T02:00:00.000Z",
        }],
      }],
    },
  };

  const result = await probeAntigravityQuota({
    executable: "agy",
    timeoutMs: 1_000,
    spawnProcess: (executable, args) => {
      spawnArgs = args;
      const logFileIndex = args.indexOf("--log-file");
      const logPath = args[logFileIndex + 1];
      writeFileSync(logPath, "Language server listening on random port at 49152 for HTTP\n");
      return child;
    },
    fetchImplementation: async (url, options) => {
      fetchOptions = { url, ...options };
      return {
        ok: true,
        json: async () => payload,
      };
    },
  });

  assert.deepEqual(result, payload);
  const tokenIndex = spawnArgs.indexOf("--csrf_token");
  assert.ok(tokenIndex >= 0, "--csrf_token 플래그가 있어야 합니다");
  const token = spawnArgs[tokenIndex + 1];
  assert.ok(token, "CSRF 토큰 값이 있어야 합니다");
  assert.equal(fetchOptions.headers?.["x-codeium-csrf-token"], token);
  assert.equal(
    fetchOptions.url,
    "http://127.0.0.1:49152/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary",
  );
});

test("DB 통계는 공급자별 오늘과 30일 값을 숫자로 정규화한다", async () => {
  let query;
  const pool = {
    query: async (text, values) => {
      query = { text, values };
      return {
        rows: [
          {
            provider: "codex",
            todayCostUSD: "1.25",
            recentTokens: "1234",
            last30DaysCostUSD: "9.5",
            last30DaysTokens: "9876",
          },
        ],
      };
    },
  };
  const now = new Date("2026-08-12T12:00:00+09:00");

  const activity = await readUsageActivity(pool, now);

  assert.deepEqual(activity.codex, {
    todayCostUSD: 1.25,
    recentTokens: 1234,
    last30DaysCostUSD: 9.5,
    last30DaysTokens: 9876,
    costEstimateSupported: true,
  });
  assert.deepEqual(activity.claude, {
    todayCostUSD: 0,
    recentTokens: 0,
    last30DaysCostUSD: 0,
    last30DaysTokens: 0,
    costEstimateSupported: true,
  });
  assert.deepEqual(activity.antigravity, emptyActivityFixture());
  assert.match(query.text, /usage_records AS usage/);
  assert.match(query.text, /IN \('claude', 'antigravity'\)/);
  assert.equal(query.values.length, 2);
});

test("한도는 짧게 캐시하고 DB 통계는 매 요청 최신값을 읽는다", async () => {
  let codexReads = 0;
  let claudeReads = 0;
  let antigravityReads = 0;
  let activityReads = 0;
  const fixedNow = new Date("2026-08-12T00:00:00Z");
  const reader = createUsageSummaryReader({
    pool: {},
    codexReader: async () => {
      codexReads += 1;
      return {
        fiveHour: { remaining: 70, resetAt: null },
        weekly: { remaining: 60, resetAt: null },
        plan: "Pro",
        subscriptionExpiresAt: "2026-08-31T14:54:17.000Z",
      };
    },
    claudeReader: async () => {
      claudeReads += 1;
      return {
        fiveHour: { remaining: 50, resetAt: null },
        weekly: { remaining: 40, resetAt: null },
        plan: "Max",
      };
    },
    antigravityReader: async () => {
      antigravityReads += 1;
      return {
        fiveHour: null,
        weekly: { remaining: 85, resetAt: null },
        plan: null,
      };
    },
    activityReader: async () => {
      activityReads += 1;
      return {
        codex: {
          todayCostUSD: activityReads,
          recentTokens: 0,
          last30DaysCostUSD: 0,
          last30DaysTokens: 0,
        },
        claude: {
          todayCostUSD: 0,
          recentTokens: 0,
          last30DaysCostUSD: 0,
          last30DaysTokens: 0,
          costEstimateSupported: true,
        },
        antigravity: emptyActivityFixture(),
      };
    },
    now: () => fixedNow,
  });

  const first = await reader();
  const second = await reader();
  const forced = await reader({ force: true });

  assert.equal(first.codexFiveHour, 70);
  assert.equal(
    first.codexSubscriptionExpiresAt,
    "2026-08-31T14:54:17.000Z",
  );
  assert.equal(second.codexActivity.todayCostUSD, 2);
  assert.equal(forced.codexActivity.todayCostUSD, 3);
  assert.equal(codexReads, 2);
  // Claude 한도는 토큰당 호출 간격이 좁아 force여도 캐시 수명 안에서는
  // 업스트림을 다시 부르지 않고 직전 값을 그대로 쓴다.
  assert.equal(claudeReads, 1);
  assert.equal(antigravityReads, 1);
  assert.equal(forced.claudeFiveHour, 50);
  assert.equal(forced.antigravityWeekly, 85);
  assert.equal(activityReads, 3);
});

test("Claude 한도 조회가 실패해도 직전 값을 유지하고 오류만 표시한다", async () => {
  let claudeReads = 0;
  let current = new Date("2026-08-23T00:00:00Z");
  const reader = createUsageSummaryReader({
    pool: {},
    codexReader: async () => ({
      fiveHour: { remaining: 70, resetAt: null },
      weekly: { remaining: 60, resetAt: null },
      plan: "Pro",
    }),
    claudeReader: async () => {
      claudeReads += 1;
      if (claudeReads === 1) {
        return {
          fiveHour: { remaining: 48, resetAt: null },
          weekly: { remaining: 13, resetAt: null },
          plan: "Max",
        };
      }
      throw new Error("Claude 계정 한도 조회가 실패했습니다 (HTTP 429).");
    },
    antigravityReader: async () => ({
      fiveHour: null,
      weekly: { remaining: 85, resetAt: null },
      plan: null,
    }),
    activityReader: async () => ({
      codex: emptyActivityFixture(),
      claude: emptyActivityFixture(),
      antigravity: emptyActivityFixture(),
    }),
    now: () => current,
  });

  const healthy = await reader();
  assert.equal(healthy.claudeFiveHour, 48);
  assert.equal(healthy.claudeLimitStale, false);
  assert.equal(healthy.claudeLimitError, null);

  // 캐시 수명이 지난 뒤 429가 나도 값은 남고 오류만 붙는다.
  current = new Date("2026-08-23T00:06:00Z");
  const failed = await reader({ force: true });
  assert.equal(claudeReads, 2);
  assert.equal(failed.claudeFiveHour, 48, "직전 정상값을 유지한다");
  assert.equal(failed.claudeWeekly, 13);
  assert.equal(failed.claudeLimitStale, true);
  assert.match(failed.claudeLimitError, /429/);
  assert.equal(failed.codexFiveHour, 70, "코덱스 한도는 영향받지 않는다");
});

test("Claude 한도 조회가 실패하면 백오프 동안 다시 부르지 않는다", async () => {
  let claudeReads = 0;
  let current = new Date("2026-08-23T00:00:00Z");
  const reader = createUsageSummaryReader({
    pool: {},
    codexReader: async () => null,
    claudeReader: async () => {
      claudeReads += 1;
      throw new Error("Claude 계정 한도 조회가 실패했습니다 (HTTP 429).");
    },
    antigravityReader: async () => null,
    activityReader: async () => ({
      codex: emptyActivityFixture(),
      claude: emptyActivityFixture(),
      antigravity: emptyActivityFixture(),
    }),
    now: () => current,
  });

  await reader();
  assert.equal(claudeReads, 1);

  // 백오프(90초) 안에서는 강제 갱신을 눌러도 업스트림을 부르지 않는다.
  current = new Date("2026-08-23T00:00:30Z");
  await reader({ force: true });
  assert.equal(claudeReads, 1);

  // 백오프가 지나면 다시 시도한다.
  current = new Date("2026-08-23T00:02:00Z");
  await reader({ force: true });
  assert.equal(claudeReads, 2);
});

test("한 공급자 실패는 다른 한도와 DB 통계를 가리지 않는다", async () => {
  const reader = createUsageSummaryReader({
    pool: {},
    codexReader: async () => {
      throw new Error("codex unavailable");
    },
    claudeReader: async () => ({
      fiveHour: { remaining: 80, resetAt: null },
      weekly: { remaining: 90, resetAt: null },
      plan: "Max",
    }),
    antigravityReader: async () => ({
      fiveHour: null,
      weekly: { remaining: 85, resetAt: null },
      plan: null,
    }),
    activityReader: async () => ({
      codex: {
        todayCostUSD: 1,
        recentTokens: 2,
        last30DaysCostUSD: 3,
        last30DaysTokens: 4,
      },
      claude: {
        todayCostUSD: 5,
        recentTokens: 6,
        last30DaysCostUSD: 7,
        last30DaysTokens: 8,
        costEstimateSupported: true,
      },
      antigravity: emptyActivityFixture(),
    }),
    now: () => new Date("2026-08-12T00:00:00Z"),
  });

  const summary = await reader();

  assert.equal(summary.codexFiveHour, null);
  assert.equal(summary.codexLimitError, "codex unavailable");
  assert.equal(summary.claudeFiveHour, 80);
  assert.equal(summary.claudeLimitError, null);
  assert.equal(summary.codexActivity.todayCostUSD, 1);
});

test("서버는 읽기 전용 사용량 요약 GET 경로를 연결한다", () => {
  const source = readFileSync(
    new URL("../src/server.mjs", import.meta.url),
    "utf8",
  );
  assert.match(source, /url\.pathname === "\/api\/usage-summary"/);
  assert.match(source, /await usageSummary\(response, url\)/);
});
