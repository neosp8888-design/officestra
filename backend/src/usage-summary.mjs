// 이 파일은 공급자 계정 한도와 로컬 DB 사용 통계를 직접 합친다.

import { execFile, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";

import {
  antigravityPlaywrightEnvironment,
} from "./antigravity-playwright.mjs";

const execFileAsync = promisify(execFile);
const fiveHourMinutes = 300;
const sevenDayMinutes = 10_080;

function finiteNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function remainingPercent(usedPercent) {
  const used = finiteNumber(usedPercent);
  if (used === null) return null;
  return Math.round(Math.min(100, Math.max(0, 100 - used)));
}

function normalizedPlan(value) {
  const plan = String(value ?? "").trim();
  if (!plan) return null;
  return plan
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((part) => part[0].toUpperCase() + part.slice(1))
    .join(" ");
}

function normalizedCodexPlan(value) {
  const key = String(value ?? "")
    .trim()
    .toLowerCase()
    .replace(/[\s_-]+/g, "");
  if (key === "pro") return "Pro 20x";
  if (key === "prolite") return "Pro 5x";
  return normalizedPlan(value);
}

/// rateLimitTier는 default_claude_max_5x처럼 배수를 담고 있을 때만 쓸모가
/// 있다. default_claude_ai처럼 등급이 빠진 값을 그대로 다듬으면 배지에
/// "Default Claude Ai"가 뜨므로, 그때는 구독 종류를 쓴다.
function normalizedClaudePlan(tier, subscriptionType) {
  const key = String(tier ?? "").trim().toLowerCase();
  if (/max[\s_-]*5x/.test(key)) return "Max 5x";
  if (/max[\s_-]*20x/.test(key)) return "Max 20x";
  return normalizedPlan(subscriptionType);
}

function isoDate(value) {
  if (value == null) return null;
  const date = typeof value === "number"
    ? new Date(value * 1_000)
    : new Date(value);
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

function codexWindow(window, expectedMinutes) {
  if (finiteNumber(window?.windowDurationMins) !== expectedMinutes) {
    return null;
  }
  return {
    remaining: remainingPercent(window.usedPercent),
    resetAt: isoDate(finiteNumber(window.resetsAt)),
  };
}

export function parseCodexRateLimits(result) {
  const rateLimits = result?.rateLimits ?? result ?? {};
  const windows = [rateLimits.primary, rateLimits.secondary]
    .filter(Boolean);
  const window = (minutes) => windows
    .map((entry) => codexWindow(entry, minutes))
    .find(Boolean) ?? null;
  return {
    fiveHour: window(fiveHourMinutes),
    weekly: window(sevenDayMinutes),
    plan: normalizedCodexPlan(rateLimits.planType),
  };
}

function decodedJWTPayload(token) {
  const parts = String(token ?? "").split(".");
  if (parts.length < 2 || !parts[1]) return null;
  try {
    return JSON.parse(
      Buffer.from(parts[1], "base64url").toString("utf8"),
    );
  } catch {
    return null;
  }
}

export function parseCodexSubscriptionExpiration(raw) {
  try {
    const credential = typeof raw === "string" ? JSON.parse(raw) : raw;
    const claims = decodedJWTPayload(credential?.tokens?.id_token);
    return isoDate(
      claims?.["https://api.openai.com/auth"]
        ?.chatgpt_subscription_active_until,
    );
  } catch {
    return null;
  }
}

async function readCodexSubscriptionExpiration() {
  const configuredHome = String(process.env.CODEX_HOME ?? "").trim();
  const credentialPath = resolve(
    configuredHome || join(homedir(), ".codex"),
    "auth.json",
  );
  try {
    return parseCodexSubscriptionExpiration(
      await readFile(credentialPath, "utf8"),
    );
  } catch {
    return null;
  }
}

function claudeWindow(window) {
  if (!window || typeof window !== "object") return null;
  return {
    remaining: remainingPercent(window.utilization),
    resetAt: isoDate(window.resets_at),
  };
}

export function parseClaudeRateLimits(payload, tier, subscriptionType) {
  return {
    fiveHour: claudeWindow(payload?.five_hour),
    weekly: claudeWindow(payload?.seven_day),
    plan: normalizedClaudePlan(tier, subscriptionType),
  };
}

export function parseAntigravityRateLimits(payload) {
  const groups = payload?.response?.groups ?? payload?.groups ?? [];
  const geminiGroup = groups.find((group) =>
    String(group?.displayName ?? "").toLowerCase().includes("gemini") ||
    (group?.buckets ?? []).some((bucket) =>
      String(bucket?.bucketId ?? "").toLowerCase().startsWith("gemini-")
    )
  );
  const buckets = Array.isArray(geminiGroup?.buckets)
    ? geminiGroup.buckets
    : [];
  const parsedBucket = (bucket) => {
    const fraction = finiteNumber(bucket?.remainingFraction);
    if (fraction === null) return null;
    return {
      remaining: Math.round(Math.min(1, Math.max(0, fraction)) * 100),
      resetAt: isoDate(bucket?.resetTime),
    };
  };
  const bucketFor = (...keys) => buckets.find((bucket) => {
    const id = String(bucket?.bucketId ?? "").toLowerCase();
    const window = String(bucket?.window ?? "").toLowerCase();
    return keys.some((key) => id.includes(key) || window === key);
  });
  const fiveHour = parsedBucket(bucketFor("five-hour", "five_hour", "5h"));
  const weekly = parsedBucket(bucketFor("weekly", "week"));
  if (!fiveHour && !weekly) {
    throw new Error("Antigravity 계정 한도 응답에 Gemini 잔여량이 없습니다.");
  }
  // 한도 응답은 상품명을 직접 주지 않는다. 현재 계정 계약에서는 무료
  // 계정은 주간 한도만, Google AI Pro 계정은 5시간 한도를 함께 받는다.
  // 제3자 모델 그룹은 Pro 응답에도 포함되므로 Ultra 판별 근거로 쓰지 않는다.
  const plan = fiveHour ? "Google AI Pro" : "Free";
  return { fiveHour, weekly, plan };
}

function safeError(error, fallback) {
  const message = error instanceof Error ? error.message : String(error ?? "");
  const normalized = message.trim();
  return normalized || fallback;
}

async function configuredCodexExecutable(pool) {
  const result = await pool.query(
    `
      SELECT config ->> 'executablePath' AS path
      FROM characters
      WHERE backend = 'codex'
        AND NULLIF(config ->> 'executablePath', '') IS NOT NULL
      ORDER BY updated_at DESC, id
      LIMIT 1
    `,
  );
  return String(result.rows?.[0]?.path ?? "").trim() || "codex";
}

async function configuredAntigravityExecutable(pool) {
  const result = await pool.query(
    `
      SELECT config ->> 'executablePath' AS path
      FROM characters
      WHERE backend = 'antigravity'
        AND NULLIF(config ->> 'executablePath', '') IS NOT NULL
      ORDER BY updated_at DESC, id
      LIMIT 1
    `,
  );
  return String(result.rows?.[0]?.path ?? "").trim() || "agy";
}

export async function readCodexRateLimits({
  pool,
  timeoutMs = 8_000,
  spawnProcess = spawn,
  subscriptionExpirationReader = readCodexSubscriptionExpiration,
}) {
  const executable = await configuredCodexExecutable(pool);
  const result = await new Promise((resolveResult, rejectResult) => {
    const child = spawnProcess(executable, ["app-server", "--stdio"], {
      cwd: homedir(),
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    let settled = false;

    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.kill();
      if (error) rejectResult(error);
      else resolveResult(value);
    };
    const timer = setTimeout(() => {
      finish(new Error("Codex 계정 한도 조회 시간이 초과됐습니다."));
    }, timeoutMs);
    timer.unref?.();

    child.on("error", () => {
      finish(new Error("Codex CLI를 실행할 수 없습니다."));
    });
    child.on("close", (code) => {
      if (!settled) {
        const detail = stderr.trim();
        finish(new Error(
          detail
            ? `Codex 계정 한도 조회가 실패했습니다: ${detail}`
            : `Codex 계정 한도 조회가 실패했습니다 (${code ?? "종료"}).`,
        ));
      }
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      if (stderr.length < 2_048) stderr += chunk;
    });
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
      for (;;) {
        const newline = stdout.indexOf("\n");
        if (newline < 0) break;
        const line = stdout.slice(0, newline);
        stdout = stdout.slice(newline + 1);
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          continue;
        }
        if (message.id !== 2) continue;
        if (message.error) {
          finish(new Error("Codex 계정 한도 응답이 실패했습니다."));
        } else {
          finish(null, message.result);
        }
      }
    });

    child.stdin.on("error", () => {
      finish(new Error("Codex CLI 요청을 전달할 수 없습니다."));
    });
    child.stdin.write(`${JSON.stringify({
      id: 1,
      method: "initialize",
      params: {
        clientInfo: { name: "officestra", version: "1" },
        capabilities: { experimentalApi: true },
      },
    })}\n`);
    child.stdin.write(`${JSON.stringify({
      method: "initialized",
      params: null,
    })}\n`);
    child.stdin.write(`${JSON.stringify({
      id: 2,
      method: "account/rateLimits/read",
      params: null,
    })}\n`);
  });
  let subscriptionExpiresAt = null;
  try {
    subscriptionExpiresAt = isoDate(
      await subscriptionExpirationReader(),
    );
  } catch {
    // 구독 만료 정보를 못 읽어도 계정 한도는 그대로 보여 준다.
  }
  return {
    ...parseCodexRateLimits(result),
    subscriptionExpiresAt,
  };
}

function parsedClaudeCredential(raw) {
  const value = JSON.parse(raw);
  const oauth = value?.claudeAiOauth ?? value;
  const accessToken = String(oauth?.accessToken ?? "").trim();
  if (!accessToken) return null;
  const scopes = Array.isArray(oauth.scopes) ? oauth.scopes : [];
  if (scopes.length > 0 && !scopes.includes("user:profile")) {
    throw new Error("Claude Code 계정 한도 조회 권한(user:profile)이 없습니다.");
  }
  return {
    accessToken,
    // subscriptionType은 "max"까지만 알려 주지만 rateLimitTier에는
    // default_claude_max_5x처럼 실제 배수가 들어 있다. 배수가 없는
    // 티어도 오므로 어느 한쪽으로 합치지 않고 둘 다 넘긴다.
    tier: oauth.rateLimitTier ?? null,
    subscriptionType: oauth.subscriptionType ?? null,
  };
}

async function readClaudeCredential() {
  const environmentToken = String(
    process.env.CLAUDE_CODE_OAUTH_TOKEN ?? "",
  ).trim();
  if (environmentToken) {
    return {
      accessToken: environmentToken,
      tier: null,
      subscriptionType: null,
    };
  }

  const credentialPath = resolve(
    process.env.CLAUDE_CONFIG_DIR ?? resolve(homedir(), ".claude"),
    ".credentials.json",
  );
  try {
    const credential = parsedClaudeCredential(
      await readFile(credentialPath, "utf8"),
    );
    if (credential) return credential;
  } catch (error) {
    if (error instanceof SyntaxError) {
      throw new Error("Claude Code 로그인 정보를 읽을 수 없습니다.");
    }
  }

  try {
    const { stdout } = await execFileAsync(
      "/usr/bin/security",
      ["find-generic-password", "-s", "Claude Code-credentials", "-w"],
      {
        encoding: "utf8",
        maxBuffer: 1_048_576,
        timeout: 3_000,
        killSignal: "SIGTERM",
      },
    );
    const credential = parsedClaudeCredential(stdout);
    if (credential) return credential;
  } catch (error) {
    if (error instanceof SyntaxError) {
      throw new Error("Claude Code Keychain 로그인 정보를 읽을 수 없습니다.");
    }
  }
  throw new Error("Claude Code 로그인 정보를 찾을 수 없습니다.");
}

export async function readClaudeRateLimits({
  fetchImplementation = fetch,
  credentialReader = readClaudeCredential,
  timeoutMs = 8_000,
} = {}) {
  const credential = await credentialReader();
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  timer.unref?.();
  try {
    const response = await fetchImplementation(
      "https://api.anthropic.com/api/oauth/usage",
      {
        method: "GET",
        headers: {
          authorization: `Bearer ${credential.accessToken}`,
          "anthropic-beta": "oauth-2025-04-20",
          "user-agent": "OFFICESTRA/1",
        },
        signal: controller.signal,
      },
    );
    if (!response.ok) {
      throw new Error(
        `Claude Code 계정 한도 조회가 실패했습니다 (HTTP ${response.status}).`,
      );
    }
    return parseClaudeRateLimits(
      await response.json(),
      credential.tier,
      credential.subscriptionType,
    );
  } catch (error) {
    if (error?.name === "AbortError") {
      throw new Error("Claude Code 계정 한도 조회 시간이 초과됐습니다.");
    }
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

export async function readAntigravityImageCooldown() {
  try {
    const filePath = join(
      homedir(),
      ".gemini",
      "antigravity-cli",
      "image-cooldown.json",
    );
    const raw = await readFile(filePath, "utf8");
    const parsed = JSON.parse(raw);
    const resetAt = parsed?.resetAt ? new Date(parsed.resetAt) : null;
    if (
      resetAt &&
      !Number.isNaN(resetAt.getTime()) &&
      resetAt.getTime() > Date.now()
    ) {
      return resetAt.toISOString();
    }
  } catch {
    // 쿨다운 파일이 없거나 만료되었으면 null
  }
  return null;
}

export async function writeAntigravityImageCooldown(resetAt) {
  try {
    const filePath = join(
      homedir(),
      ".gemini",
      "antigravity-cli",
      "image-cooldown.json",
    );
    const payload = JSON.stringify({
      resetAt: resetAt instanceof Date ? resetAt.toISOString() : resetAt,
    });
    await writeFile(filePath, payload, "utf8");
  } catch {
    // ignore
  }
}

export async function readAntigravityRateLimits({
  pool,
  timeoutMs = 8_000,
  quotaProbe = probeAntigravityQuota,
  imageCooldownReader = readAntigravityImageCooldown,
} = {}) {
  const executable = await configuredAntigravityExecutable(pool);
  const parsed = parseAntigravityRateLimits(await quotaProbe({
    executable,
    timeoutMs,
  }));
  const imageResetAt = await imageCooldownReader().catch(() => null);
  return { ...parsed, imageResetAt };
}

export async function probeAntigravityQuota({
  executable = "agy",
  timeoutMs = 8_000,
  spawnProcess = spawn,
  fetchImplementation = fetch,
  now = () => Date.now(),
} = {}) {
  const temporaryRoot = await mkdtemp(
    join(tmpdir(), "officestra-antigravity-quota-"),
  );
  const logPath = join(temporaryRoot, "agy.log");
  let child = null;
  let processError = null;
  let processClosed = false;
  let stderr = "";
  try {
    child = spawnProcess(
      executable,
      ["--log-file", logPath, "models"],
      {
        cwd: homedir(),
        env: antigravityPlaywrightEnvironment(process.env),
        stdio: ["ignore", "ignore", "pipe"],
      },
    );
    child.once("error", (error) => {
      processError = error;
    });
    child.once("close", () => {
      processClosed = true;
    });
    child.stderr?.setEncoding?.("utf8");
    child.stderr?.on?.("data", (chunk) => {
      if (stderr.length < 2_048) stderr += chunk;
    });

    const deadline = now() + timeoutMs;
    let port = null;
    while (now() < deadline) {
      if (processError) {
        throw new Error("Antigravity CLI를 실행할 수 없습니다.");
      }
      if (port === null) {
        const log = await readFile(logPath, "utf8").catch(() => "");
        const match = log.match(/random port at (\d+) for HTTP(?:\s|$)/);
        if (match) port = Number.parseInt(match[1], 10);
      }
      if (port !== null) {
        try {
          const response = await fetchImplementation(
            `http://127.0.0.1:${port}` +
              "/exa.language_server_pb.LanguageServerService/" +
              "RetrieveUserQuotaSummary",
            {
              method: "POST",
              headers: { "content-type": "application/json" },
              body: "{}",
            },
          );
          if (response.ok) {
            const payload = await response.json();
            if ((payload?.response?.groups ?? []).length > 0) {
              return payload;
            }
          }
        } catch {
          // 언어 서버가 인증을 복원하는 짧은 구간은 다음 poll에서 재시도한다.
        }
      }
      if (processClosed) break;
      await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
    }
    const detail = stderr.trim();
    throw new Error(
      detail
        ? `Antigravity 계정 한도 조회가 실패했습니다: ${detail}`
        : "Antigravity 계정 한도 조회 시간이 초과됐습니다.",
    );
  } finally {
    child?.kill?.();
    await rm(temporaryRoot, { recursive: true, force: true });
  }
}

function emptyActivity(costEstimateSupported = true) {
  return {
    todayCostUSD: 0,
    recentTokens: 0,
    last30DaysCostUSD: 0,
    last30DaysTokens: 0,
    costEstimateSupported,
  };
}

export async function readUsageActivity(pool, now = new Date()) {
  const dayStart = new Date(
    now.getFullYear(),
    now.getMonth(),
    now.getDate(),
  );
  const thirtyDaysAgo = new Date(now.getTime() - 30 * 86_400_000);
  const result = await pool.query(
    `
      WITH measured AS (
        SELECT
          COALESCE(NULLIF(turn_record.backend, ''), character.backend)
            AS provider,
          turn_record.started_at,
          COALESCE(usage.cost_usd, 0) AS cost_usd,
          CASE
            WHEN COALESCE(NULLIF(turn_record.backend, ''), character.backend)
              IN ('claude', 'antigravity')
            THEN
              COALESCE(usage.input_tokens, 0)
              + COALESCE(usage.output_tokens, 0)
              + COALESCE(usage.cached_input_tokens, 0)
              + COALESCE(usage.cache_write_input_tokens, 0)
            ELSE
              COALESCE(usage.input_tokens, 0)
              + COALESCE(usage.output_tokens, 0)
          END AS token_count
        FROM usage_records AS usage
        JOIN turns AS turn_record ON turn_record.id = usage.turn_id
        JOIN cli_sessions AS session
          ON session.id = turn_record.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE turn_record.started_at >= $2
          AND turn_record.provider_kind = 'cloud'
      )
      SELECT
        provider,
        COALESCE(
          SUM(cost_usd) FILTER (WHERE started_at >= $1),
          0
        )::double precision AS "todayCostUSD",
        COALESCE(
          SUM(token_count) FILTER (WHERE started_at >= $1),
          0
        )::double precision AS "recentTokens",
        COALESCE(SUM(cost_usd), 0)::double precision
          AS "last30DaysCostUSD",
        COALESCE(SUM(token_count), 0)::double precision
          AS "last30DaysTokens"
      FROM measured
      GROUP BY provider
    `,
    [dayStart, thirtyDaysAgo],
  );
  const activity = {
    codex: emptyActivity(),
    claude: emptyActivity(),
    antigravity: emptyActivity(),
  };
  for (const row of result.rows ?? []) {
    if (!Object.hasOwn(activity, row.provider)) continue;
    activity[row.provider] = {
      todayCostUSD: finiteNumber(row.todayCostUSD) ?? 0,
      recentTokens: Math.round(finiteNumber(row.recentTokens) ?? 0),
      last30DaysCostUSD: finiteNumber(row.last30DaysCostUSD) ?? 0,
      last30DaysTokens: Math.round(
        finiteNumber(row.last30DaysTokens) ?? 0,
      ),
      costEstimateSupported: true,
    };
  }
  return activity;
}

function flattenedLimits(
  codex,
  claude,
  antigravity,
  errors,
  fetchedAt,
  stale = { codex: false, claude: false, antigravity: false },
) {
  return {
    codexFiveHour: codex?.fiveHour?.remaining ?? null,
    codexFiveHourResetAt: codex?.fiveHour?.resetAt ?? null,
    codexWeekly: codex?.weekly?.remaining ?? null,
    codexWeeklyResetAt: codex?.weekly?.resetAt ?? null,
    claudeFiveHour: claude?.fiveHour?.remaining ?? null,
    claudeFiveHourResetAt: claude?.fiveHour?.resetAt ?? null,
    claudeWeekly: claude?.weekly?.remaining ?? null,
    claudeWeeklyResetAt: claude?.weekly?.resetAt ?? null,
    antigravityFiveHour: antigravity?.fiveHour?.remaining ?? null,
    antigravityFiveHourResetAt: antigravity?.fiveHour?.resetAt ?? null,
    antigravityWeekly: antigravity?.weekly?.remaining ?? null,
    antigravityWeeklyResetAt: antigravity?.weekly?.resetAt ?? null,
    antigravityImageResetAt: antigravity?.imageResetAt ?? null,
    codexPlan: codex?.plan ?? null,
    claudePlan: claude?.plan ?? null,
    antigravityPlan: antigravity?.plan ?? null,
    codexSubscriptionExpiresAt: codex?.subscriptionExpiresAt ?? null,
    codexLimitError: errors.codex,
    claudeLimitError: errors.claude,
    antigravityLimitError: errors.antigravity,
    codexLimitStale: Boolean(stale.codex),
    claudeLimitStale: Boolean(stale.claude),
    antigravityLimitStale: Boolean(stale.antigravity),
    fetchedAt: fetchedAt.toISOString(),
  };
}

// 조회에 실패하거나 호출을 건너뛰면 직전 정상값을 유지한다. 값을 지워
// 화면이 비어 보이는 것보다 조금 지난 값을 보여 주는 편이 낫다.
function resolvedProviderLimits(result, lastGood, fallbackMessage) {
  if (result.status === "fulfilled" && result.value) {
    return { value: result.value, error: null, stale: false };
  }
  const error = result.status === "rejected"
    ? safeError(result.reason, fallbackMessage)
    : null;
  return { value: lastGood, error, stale: Boolean(lastGood) };
}

export function createUsageSummaryReader({
  pool,
  codexReader = () => readCodexRateLimits({ pool }),
  claudeReader = () => readClaudeRateLimits(),
  antigravityReader = () => readAntigravityRateLimits({ pool }),
  activityReader = (now) => readUsageActivity(pool, now),
  now = () => new Date(),
  cacheLifetimeMs = 30_000,
  // Claude 한도 엔드포인트는 토큰 하나당 호출 간격이 좁다. 직원 CLI와
  // 사용자 세션이 같은 토큰을 공유해서 자주 부르면 429가 난다.
  claudeCacheLifetimeMs = 300_000,
  claudeRetryBackoffMs = 90_000,
  antigravityCacheLifetimeMs = 300_000,
  antigravityRetryBackoffMs = 90_000,
}) {
  let cachedLimits = null;
  let inFlightLimits = null;
  let lastGoodCodex = null;
  let lastGoodClaude = null;
  let lastGoodAntigravity = null;
  let claudeCooldownUntil = 0;
  let antigravityCooldownUntil = 0;

  async function limits(force) {
    const current = now();
    if (
      !force &&
      cachedLimits &&
      current.getTime() - cachedLimits.cachedAt < cacheLifetimeMs
    ) {
      return cachedLimits.value;
    }
    if (inFlightLimits) return await inFlightLimits;

    inFlightLimits = (async () => {
      // 성공 후에는 캐시 수명, 실패 후에는 백오프가 끝나야 다시 부른다.
      // force여도 이 간격은 지킨다. 그 사이에는 직전 정상값을 쓴다.
      const claudeDue = current.getTime() >= claudeCooldownUntil;
      const antigravityDue = current.getTime() >= antigravityCooldownUntil;
      const [codexResult, claudeResult, antigravityResult] =
        await Promise.allSettled([
        codexReader(),
        claudeDue ? claudeReader() : Promise.resolve(null),
        antigravityDue ? antigravityReader() : Promise.resolve(null),
      ]);
      const fetchedAt = now();
      if (claudeDue) {
        const claudeOK = claudeResult.status === "fulfilled" &&
          Boolean(claudeResult.value);
        claudeCooldownUntil = fetchedAt.getTime() +
          (claudeOK ? claudeCacheLifetimeMs : claudeRetryBackoffMs);
      }
      if (antigravityDue) {
        const antigravityOK = antigravityResult.status === "fulfilled" &&
          Boolean(antigravityResult.value);
        antigravityCooldownUntil = fetchedAt.getTime() +
          (antigravityOK
            ? antigravityCacheLifetimeMs
            : antigravityRetryBackoffMs);
      }

      const codex = resolvedProviderLimits(
        codexResult,
        lastGoodCodex,
        "Codex 한도를 확인할 수 없습니다.",
      );
      const claude = resolvedProviderLimits(
        claudeResult,
        lastGoodClaude,
        "Claude Code 한도를 확인할 수 없습니다.",
      );
      const antigravity = resolvedProviderLimits(
        antigravityResult,
        lastGoodAntigravity,
        "Antigravity 한도를 확인할 수 없습니다.",
      );
      lastGoodCodex = codex.value ?? lastGoodCodex;
      lastGoodClaude = claude.value ?? lastGoodClaude;
      lastGoodAntigravity = antigravity.value ?? lastGoodAntigravity;

      const value = flattenedLimits(
        codex.value,
        claude.value,
        antigravity.value,
        {
          codex: codex.error,
          claude: claude.error,
          antigravity: antigravity.error,
        },
        fetchedAt,
        {
          codex: codex.stale,
          claude: claude.stale,
          antigravity: antigravity.stale,
        },
      );
      cachedLimits = { cachedAt: fetchedAt.getTime(), value };
      return value;
    })();
    try {
      return await inFlightLimits;
    } finally {
      inFlightLimits = null;
    }
  }

  return async function usageSummary({ force = false } = {}) {
    const current = now();
    const [limitSnapshot, activity] = await Promise.all([
      limits(force),
      activityReader(current),
    ]);
    return {
      ...limitSnapshot,
      codexActivity: activity.codex,
      claudeActivity: activity.claude,
      antigravityActivity: activity.antigravity,
    };
  };
}
