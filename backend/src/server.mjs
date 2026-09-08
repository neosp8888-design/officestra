// 이 파일은 캐릭터 설정과 CLI 대화 기록 및 RAG 저장 API를 제공한다.

import { createServer } from "node:http";
import { randomUUID } from "node:crypto";
import { WebSocket, WebSocketServer } from "ws";

import {
  AgentBusyError,
  AgentDrainingError,
  AgentJobNotFoundError,
  AgentSessionNotFoundError,
  AgentRuntime,
  CharacterNotFoundError,
  locateExecutable,
} from "./agent-runtime.mjs";
import {
  canonicalProjectRoot,
  GitWorkspaceError,
  GitWorkspaceManager,
} from "./git-workspace.mjs";
import {
  appendLocalImagePreviews,
  generatedImageRoot,
  generatedImagesForTurn,
} from "./local-artifacts.mjs";
import { sessionContextUsage } from "./session-context-usage.mjs";
import {
  ProvenanceValidationError,
  isUUID,
  normalizeResponseSources,
  parseWorkRecordFilters,
  replaceTurnResponseSources,
} from "./work-record-provenance.mjs";
import { pool, withTransaction } from "./db.mjs";
import {
  officeBackendHealth,
  officeBackendMaintenanceStatus,
} from "./health.mjs";
import {
  characterSettingsRequireNewSession,
  readCharacterConfiguration,
  syncCharacters,
} from "./configuration.mjs";
import {
  CharacterSettingsDrainConflictError,
  CharacterSettingsRuntimeUnavailableError,
  CharacterSettingsTargetsNotFoundError,
  CharacterSettingsValidationError,
  normalizeCharacterSettingsUpdate,
  updateCharacterSettingsAtomically,
  withCharacterSessionLocks,
} from "./character-settings.mjs";
import {
  RuntimeCLIPathsValidationError,
  synchronizeRuntimeCLIPaths,
} from "./runtime-cli-paths.mjs";
import {
  AGENT_BACKENDS,
  backendEfforts,
  backendSupportsFastMode,
} from "./agent-provider.mjs";
import { migrate } from "./migrate.mjs";
import {
  reconcileTerminalWorkRecordReviews,
  syncWorkRecordRAGDocuments,
} from "./work-record-memory.mjs";
import {
  TurnFeedbackValidationError,
  normalizeTurnFeedback,
  replaceTurnFeedback,
} from "./turn-feedback.mjs";
import { startSlackBridge } from "./slack-bridge.mjs";
import {
  CharacterTurnCostSummaryCache,
  UsageReportError,
  readUsageReport,
} from "./usage-report.mjs";
import {
  WikiKnowledgeError,
  approveWikiProposal,
  archiveWikiPage,
  createWikiProposal,
  getWikiPage,
  isWikiPageKeyRaceError,
  listWikiPages,
  listWikiProposals,
  rejectWikiProposal,
  resolveWikiPageKeyRace,
  verifyWikiProposal,
} from "./wiki-knowledge.mjs";
import { createUsageSummaryReader } from "./usage-summary.mjs";
import { LocalEmbeddingService } from "./local-embedding.mjs";
import { searchRAGDocuments } from "./rag-search.mjs";
import {
  CLIUpdateBusyError,
  CLIUpdateUnknownPackageError,
  applyCLIUpdates,
  createCLIUpdateChecker,
  backendsForIdentifier,
  packagesForIdentifier,
  sharedInstallPrefix,
} from "./cli-updates.mjs";
import { TerminalSessionManager } from "./terminal-sessions.mjs";
import {
  createPostgresPricingCatalogStore,
  PricingCatalogService,
} from "./pricing-catalog.mjs";
import {
  createPostgresModelCatalogStore,
  ModelCatalogService,
  ModelCatalogValidationError,
} from "./model-catalog.mjs";

const port = Number(process.env.OFFICE_BACKEND_PORT ?? 4317);
const liveFeedMinimumTurnsPerCharacter = 10;
const sockets = new Set();
const webSocketServer = new WebSocketServer({ noServer: true });
let runtime;
let terminalSessions;
let pricingCatalogService;
let modelCatalogService;
let shuttingDown = false;
const readUsageSummary = createUsageSummaryReader({ pool });
const turnCostSummaryCache = new CharacterTurnCostSummaryCache(pool);
const cliUpdateChecker = createCLIUpdateChecker();
const localEmbeddingService = new LocalEmbeddingService();

async function hasRunningWork(backends) {
  const result = await pool.query(
    `
      SELECT 1
      FROM turns AS turn
      JOIN cli_sessions AS session ON session.id = turn.cli_session_id
      JOIN characters AS character ON character.id = session.character_id
      WHERE turn.status IN ('pending', 'running')
        AND ($1::text[] IS NULL OR character.backend = ANY($1))
      LIMIT 1
    `,
    [backends ?? null],
  );
  return result.rows.length > 0;
}

function broadcast(event) {
  turnCostSummaryCache.observe(event);
  const payload = JSON.stringify(event);
  for (const socket of sockets) {
    if (socket.readyState === WebSocket.OPEN) {
      socket.send(payload);
    }
  }
}

function send(response, status, body) {
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
  });
  response.end(JSON.stringify(body));
}

async function backendMaintenance(response, method, request) {
  if (!runtime) {
    send(response, 503, { error: "CLI 실행기가 준비되지 않았습니다." });
    return;
  }
  if (method === "GET") {
    send(response, 200, {
      ok: true,
      ...officeBackendMaintenanceStatus(runtime),
    });
    return;
  }
  if (!["POST", "DELETE"].includes(method)) {
    send(response, 404, { error: "경로를 찾을 수 없습니다." });
    return;
  }
  if (!trustedJSONMutation(request, response)) {
    return;
  }
  await readJSON(request);
  const status = method === "POST"
    ? runtime.beginDrain()
    : runtime.cancelDrain();
  send(response, 200, { ok: true, ...status });
}

async function withCharacterSessionLock(characterID, body) {
  return await withCharacterSessionLocks(pool, [characterID], body);
}

async function readJSON(request) {
  const chunks = [];
  for await (const chunk of request) {
    chunks.push(chunk);
  }
  if (chunks.length === 0) {
    return {};
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

async function configuredModelCatalogExecutable(provider) {
  const { rows } = await pool.query(
    `
      SELECT config
      FROM characters
      WHERE backend = $1
      ORDER BY updated_at DESC, id
      LIMIT 1
    `,
    [provider],
  );
  return locateExecutable({
    backend: provider,
    config: rows[0]?.config ?? {},
  });
}

async function modelCatalog(response, url) {
  if (!modelCatalogService) {
    send(response, 503, { error: "모델 목록 수집기가 준비되지 않았습니다." });
    return;
  }
  if (url.searchParams.get("force") === "1") {
    await modelCatalogService.refreshDue({ force: true });
  }
  send(response, 200, modelCatalogService.snapshot());
}

async function updateModelCatalogExclusions(response, body) {
  if (!modelCatalogService) {
    send(response, 503, { error: "모델 목록 수집기가 준비되지 않았습니다." });
    return;
  }
  const provider = String(body.provider ?? "");
  const snapshot = await modelCatalogService.setExclusions(
    provider,
    body.excludedModels,
  );
  send(response, 200, snapshot);
}

function routeCharacterName(pathname) {
  const match = pathname.match(/^\/api\/characters\/([^/]+)\/name$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeCharacterSettings(pathname) {
  const match = pathname.match(/^\/api\/characters\/([^/]+)\/settings$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeCharacterIdentityPrompt(pathname) {
  const match = pathname.match(
    /^\/api\/characters\/([^/]+)\/identity-prompt$/,
  );
  return match ? decodeURIComponent(match[1]) : null;
}

function routeCharacterContextSettings(pathname) {
  const match = pathname.match(
    /^\/api\/characters\/([^/]+)\/context-settings$/,
  );
  return match ? decodeURIComponent(match[1]) : null;
}

function routeCharacterContextCompact(pathname) {
  const match = pathname.match(
    /^\/api\/characters\/([^/]+)\/context\/compact$/,
  );
  return match ? decodeURIComponent(match[1]) : null;
}

function routeCharacterHistory(pathname) {
  const match = pathname.match(/^\/api\/characters\/([^/]+)\/history$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeAgentJob(pathname) {
  const match = pathname.match(/^\/api\/agent-jobs\/([^/]+)$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeTerminalSession(pathname) {
  const match = pathname.match(
    /^\/api\/terminal-sessions\/([^/]+)(?:\/(events|interrupt))?$/,
  );
  return match
    ? {
      characterID: decodeURIComponent(match[1]),
      events: match[2] === "events",
      interrupt: match[2] === "interrupt",
    }
    : null;
}

function routeLiveFeedTurn(pathname) {
  const match = pathname.match(/^\/api\/live-feed\/([^/]+)$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeTurnSources(pathname) {
  const match = pathname.match(/^\/api\/turns\/([^/]+)\/sources$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeTurnFeedback(pathname) {
  const match = pathname.match(/^\/api\/turns\/([^/]+)\/feedback$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function routeWorkspaceReview(pathname) {
  const match = pathname.match(
    /^\/api\/workspace-reviews\/([^/]+)(?:\/(approve|reject))?$/,
  );
  return match
    ? {
      turnID: decodeURIComponent(match[1]),
      action: match[2] ?? null,
    }
    : null;
}

async function listCharacters(response) {
  const result = await pool.query(
    `
      SELECT
        id,
        name,
        seat,
        backend,
        model,
        effort,
        fast_mode AS "fastMode",
        auto_compact_percent AS "autoCompactPercent",
        permission,
        identity_prompt AS "identityPrompt"
      FROM characters
      ORDER BY
        CASE id
          WHEN 'boss' THEN 1
          WHEN 'left-man' THEN 2
          WHEN 'left-woman' THEN 3
          WHEN 'right-woman' THEN 4
          WHEN 'right-man' THEN 5
          ELSE 99
        END
    `,
  );
  send(response, 200, { characters: result.rows });
}

async function listActiveSessions(response) {
  const result = await pool.query(
    `
      SELECT
        active.character_id AS "characterId",
        session.external_id AS "externalSessionId",
        session.conversation_id AS "conversationId",
        session.started_at AS "startedAt"
      FROM active_cli_sessions AS active
      JOIN cli_sessions AS session
        ON session.id = active.cli_session_id
      WHERE session.external_id IS NOT NULL
        AND session.ended_at IS NULL
      ORDER BY active.character_id
    `,
  );
  send(response, 200, { sessions: result.rows });
}

async function usageSummary(response, url) {
  const force = ["1", "true"].includes(
    String(url.searchParams.get("force") ?? "").toLowerCase(),
  );
  send(response, 200, await readUsageSummary({ force }));
}

async function usageReport(response, url) {
  send(
    response,
    200,
    await readUsageReport(pool, {
      backend: url.searchParams.get("backend"),
      granularity: url.searchParams.get("granularity") ?? "day",
      timeZone: url.searchParams.get("tz") ?? "UTC",
    }),
  );
}

async function cliUpdates(response, url) {
  const force = ["1", "true"].includes(
    String(url.searchParams.get("force") ?? "").toLowerCase(),
  );
  send(response, 200, await cliUpdateChecker.read({ force }));
}

async function applyCLIUpdatesEndpoint(response, request) {
  if (!trustedJSONMutation(request, response)) {
    return;
  }
  const body = await readJSON(request);
  try {
    const current = await cliUpdateChecker.read();
    const updatedBackends = backendsForIdentifier(body.id);
    const result = await applyCLIUpdates({
      hasRunningWork,
      packages: packagesForIdentifier(body.id),
      backends: updatedBackends,
      prefix: sharedInstallPrefix(current, body.id),
    });
    if (updatedBackends.includes("claude")) {
      runtime?.closeClaudeWorkers(
        new Error("Claude Code CLI가 갱신되어 지속 세션을 다시 시작합니다."),
      );
    }
    cliUpdateChecker.invalidate();
    send(response, 200, {
      ...result,
      status: await cliUpdateChecker.read({ force: true }),
    });
  } catch (error) {
    if (error instanceof CLIUpdateBusyError) {
      send(response, 409, { error: error.message });
      return;
    }
    if (error instanceof CLIUpdateUnknownPackageError) {
      send(response, 400, { error: error.message });
      return;
    }
    send(response, 500, {
      error: error instanceof Error ? error.message : String(error),
    });
  }
}

async function updateCharacterName(response, characterID, body) {
  const name = String(body.name ?? "").trim();
  if (name.length === 0 || name.length > 30) {
    send(response, 400, { error: "이름은 1자 이상 30자 이하여야 합니다." });
    return;
  }

  const result = await pool.query(
    `
      UPDATE characters
      SET name = $2, updated_at = now()
      WHERE id = $1
      RETURNING id, name
    `,
    [characterID, name],
  );
  if (result.rowCount === 0) {
    send(response, 404, { error: "캐릭터를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, result.rows[0]);
}

async function updateCharacterIdentityPrompt(response, characterID, body) {
  const identityPrompt = String(body.identityPrompt ?? "").trim();
  if (identityPrompt.length === 0 || identityPrompt.length > 1200) {
    send(response, 400, {
      error: "역할·업무 지침은 1자 이상 1,200자 이하여야 합니다.",
    });
    return;
  }

  const character = await withTransaction(async (client) => {
    const current = await client.query(
      `
        SELECT id, identity_prompt AS "identityPrompt"
        FROM characters
        WHERE id = $1
        FOR UPDATE
      `,
      [characterID],
    );
    if (current.rowCount === 0) {
      return null;
    }
    if (current.rows[0].identityPrompt === identityPrompt) {
      return current.rows[0];
    }

    const updated = await client.query(
      `
        UPDATE characters
        SET identity_prompt = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, identity_prompt AS "identityPrompt"
      `,
      [characterID, identityPrompt],
    );
    return updated.rows[0];
  });
  if (!character) {
    send(response, 404, { error: "캐릭터를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, character);
}

async function updateCharacterContextSettings(response, characterID, body) {
  const autoCompactPercent = Number(body.autoCompactPercent);
  if (
    !Number.isInteger(autoCompactPercent) ||
    autoCompactPercent < 20 ||
    autoCompactPercent > 95
  ) {
    send(response, 400, {
      error: "자동 압축 기준은 20% 이상 95% 이하의 정수여야 합니다.",
    });
    return;
  }
  const result = await pool.query(
    `
      UPDATE characters
      SET auto_compact_percent = $2, updated_at = now()
      WHERE id = $1
      RETURNING
        id,
        auto_compact_percent AS "autoCompactPercent"
    `,
    [characterID, autoCompactPercent],
  );
  if (result.rowCount === 0) {
    send(response, 404, { error: "캐릭터를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, result.rows[0]);
}

async function compactCharacterContext(response, characterID) {
  if (!runtime) {
    send(response, 503, { error: "CLI 실행기가 준비되지 않았습니다." });
    return;
  }
  try {
    send(response, 200, await runtime.compactContext(characterID));
  } catch (error) {
    if (
      error instanceof CharacterNotFoundError ||
      error instanceof AgentSessionNotFoundError
    ) {
      send(response, 404, { error: error.message });
      return;
    }
    throw error;
  }
}

async function updateCharacterSettings(response, characterID, body) {
  let normalized;
  try {
    normalized = normalizeCharacterSettingsUpdate(
      { ...body, characterId: characterID },
      {
        requireCharacterID: true,
        modelCatalog: modelCatalogService,
      },
    );
  } catch (error) {
    if (error instanceof CharacterSettingsValidationError) {
      send(response, 400, { error: error.message });
      return;
    }
    throw error;
  }
  const {
    backend,
    model,
    effort,
    fastMode,
    permission,
  } = normalized;

  const character = await withCharacterSessionLock(
    characterID,
    async (client) => {
      await client.query("BEGIN");
      try {
        const current = await client.query(
          `
            SELECT
              id,
              backend,
              model,
              effort,
              fast_mode AS "fastMode",
              permission
            FROM characters
            WHERE id = $1
            FOR UPDATE
          `,
          [characterID],
        );
        if (current.rowCount === 0) {
          await client.query("COMMIT");
          return null;
        }

        const previous = current.rows[0];
        const requiresNewSession = characterSettingsRequireNewSession(
          previous,
          { backend },
        );
        const changed =
          previous.backend !== backend ||
          previous.model !== model ||
          previous.effort !== effort ||
          previous.fastMode !== fastMode ||
          previous.permission !== permission;
        if (!changed) {
          await client.query("COMMIT");
          return previous;
        }
        if (requiresNewSession) {
          if (!runtime) {
            throw new AgentBusyError(
              "CLI 실행기가 준비된 뒤 설정을 변경하세요.",
            );
          }
          await runtime.prepareWorkspaceForSessionEnd(characterID);
        }

        const updated = await client.query(
          `
            UPDATE characters
            SET
              backend = $2,
              model = $3,
              effort = $4,
              fast_mode = $5,
              permission = $6,
              config = CASE
                WHEN characters.backend <> $2
                  THEN COALESCE(characters.config, '{}'::jsonb)
                    - 'executablePath'
                ELSE characters.config
              END,
              updated_at = now()
            WHERE id = $1
            RETURNING
              id,
              backend,
              model,
              effort,
              fast_mode AS "fastMode",
              permission
          `,
          [characterID, backend, model, effort, fastMode, permission],
        );
        await client.query("COMMIT");
        return updated.rows[0];
      } catch (error) {
        await client.query("ROLLBACK");
        throw error;
      }
    },
  );
  if (!character) {
    send(response, 404, { error: "캐릭터를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, character);
}

async function updateCharacterSettingsBulk(response, body) {
  try {
    send(
      response,
      200,
      await updateCharacterSettingsAtomically({
        pool,
        runtime,
        body,
        modelCatalog: modelCatalogService,
      }),
    );
  } catch (error) {
    if (error instanceof CharacterSettingsValidationError) {
      send(response, 400, { error: error.message });
      return;
    }
    if (error instanceof CharacterSettingsTargetsNotFoundError) {
      send(response, 404, {
        error: error.message,
        characterIds: error.characterIDs,
      });
      return;
    }
    if (error instanceof CharacterSettingsRuntimeUnavailableError) {
      send(response, 503, { error: error.message });
      return;
    }
    if (error instanceof CharacterSettingsDrainConflictError) {
      send(response, 409, { error: error.message });
      return;
    }
    throw error;
  }
}

async function updateRuntimeCLIPaths(response, body) {
  try {
    send(
      response,
      200,
      await synchronizeRuntimeCLIPaths({ pool, body }),
    );
  } catch (error) {
    if (error instanceof RuntimeCLIPathsValidationError) {
      send(response, 400, { error: error.message });
      return;
    }
    throw error;
  }
}

async function characterHistory(response, characterID) {
  const characterResult = await pool.query(
    `
      SELECT id, name, seat, backend
      FROM characters
      WHERE id = $1
    `,
    [characterID],
  );
  if (characterResult.rowCount === 0) {
    send(response, 404, { error: "캐릭터를 찾을 수 없습니다." });
    return;
  }

  const sessionsResult = await pool.query(
    `
      SELECT
        id,
        external_id AS "externalId",
        started_at AS "startedAt",
        ended_at AS "endedAt"
      FROM cli_sessions
      WHERE character_id = $1
      ORDER BY started_at DESC
    `,
    [characterID],
  );
  const turnsResult = await pool.query(
    `
      SELECT
        t.id,
        t.cli_session_id AS "sessionId",
        t.backend AS "executionBackend",
        t.model AS "executionModel",
        t.effort AS "executionEffort",
        t.fast_mode AS "executionFastMode",
        t.origin,
        COALESCE(
          CASE
            WHEN history_workspace.status IN ('merged', 'closed')
              THEN history_workspace.source_workdir
            ELSE history_workspace.execution_workdir
          END,
          conversation.workdir
        ) AS "conversationWorkdir",
        t.prompt,
        t.started_at AS "startedAt",
        t.ended_at AS "endedAt",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning",
        COALESCE(
          (
            SELECT text
            FROM messages
            WHERE turn_id = t.id AND role = 'assistant'
            ORDER BY received_at DESC
            LIMIT 1
          ),
          ''
        ) AS response,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', source.id,
                'sourceKind', source.source_kind,
                'title', source.title,
                'locator', source.locator,
                'excerpt', source.excerpt,
                'ragDocumentId', source.rag_document_id,
                'workRecordId', source.work_record_id
              )
              ORDER BY source.ordinal, source.created_at, source.id
            )
            FROM turn_response_sources AS source
            WHERE source.turn_id = t.id
          ),
          '[]'::json
        ) AS sources
      FROM turns t
      JOIN cli_sessions s ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      LEFT JOIN task_workspaces AS history_workspace
        ON history_workspace.id = t.task_workspace_id
      WHERE s.character_id = $1
      ORDER BY t.started_at DESC
    `,
    [characterID],
  );

  const sessions = sessionsResult.rows.map((session) => ({
    ...session,
    turns: turnsResult.rows.filter(
      (turn) => turn.sessionId === session.id,
    ).map((turn) => withArtifactPreviews(turn, session.externalId)),
  }));
  send(response, 200, {
    character: characterResult.rows[0],
    sessions,
  });
}

async function globalHistory(response, url) {
  const characterID = url.searchParams.get("characterId") || null;
  const from = url.searchParams.get("from") || null;
  const to = url.searchParams.get("to") || null;

  const result = await pool.query(
    `
      SELECT
        t.id,
        c.id AS "characterId",
        c.name AS "characterName",
        c.backend,
        t.backend AS "executionBackend",
        t.model AS "executionModel",
        t.effort AS "executionEffort",
        t.fast_mode AS "executionFastMode",
        t.origin,
        s.external_id AS "externalSessionId",
        COALESCE(
          CASE
            WHEN history_workspace.status IN ('merged', 'closed')
              THEN history_workspace.source_workdir
            ELSE history_workspace.execution_workdir
          END,
          conversation.workdir
        ) AS "conversationWorkdir",
        t.prompt,
        COALESCE(
          (
            SELECT text
            FROM messages
            WHERE turn_id = t.id AND role = 'assistant'
            ORDER BY received_at DESC
            LIMIT 1
          ),
          ''
        ) AS response,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', source.id,
                'sourceKind', source.source_kind,
                'title', source.title,
                'locator', source.locator,
                'excerpt', source.excerpt,
                'ragDocumentId', source.rag_document_id,
                'workRecordId', source.work_record_id
              )
              ORDER BY source.ordinal, source.created_at, source.id
            )
            FROM turn_response_sources AS source
            WHERE source.turn_id = t.id
          ),
          '[]'::json
        ) AS sources,
        t.started_at AS "startedAt",
        t.ended_at AS "endedAt",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning"
      FROM turns t
      JOIN cli_sessions s ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      LEFT JOIN task_workspaces AS history_workspace
        ON history_workspace.id = t.task_workspace_id
      JOIN characters c ON c.id = s.character_id
      WHERE ($1::text IS NULL OR c.id = $1)
        AND ($2::timestamptz IS NULL OR t.started_at >= $2)
        AND ($3::timestamptz IS NULL OR t.started_at <= $3)
      ORDER BY t.started_at DESC
      LIMIT 1000
    `,
    [characterID, from, to],
  );
  send(response, 200, {
    turns: result.rows.map((turn) => withArtifactPreviews(turn)),
  });
}

async function listWorkRecords(response, url) {
  const filters = parseWorkRecordFilters(url.searchParams);
  const result = await pool.query(
    `
      WITH matching AS (
        SELECT
          record.id,
          record.project_id AS "projectId",
          project.name AS "projectName",
          project.repository_root AS "repositoryRoot",
          record.record_type AS "recordType",
          record.lifecycle_state AS "lifecycleState",
          record.title,
          record.body,
          record.legacy_stage_number AS "legacyStageNumber",
          record.character_id AS "characterId",
          record.attribution,
          record.source_turn_id AS "sourceTurnId",
          record.source_workspace_id AS "sourceWorkspaceId",
          record.source_path AS "sourcePath",
          record.source_commit AS "sourceCommit",
          record.source_section_ordinal AS "sourceSectionOrdinal",
          record.source_line_start AS "sourceLineStart",
          record.source_line_end AS "sourceLineEnd",
          record.source_section_sha256 AS "sourceSectionSha256",
          record.metadata,
          record.recorded_at AS "recordedAt",
          record.updated_at AS "updatedAt",
          CASE
            WHEN $5::text IS NULL THEN NULL
            ELSE ts_rank(
              record.search_document,
              websearch_to_tsquery('simple', $5)
            )::double precision
          END AS score,
          COALESCE(
            (
              SELECT json_agg(
                json_build_object(
                  'ordinal', item.ordinal,
                  'text', item.item_text,
                  'isChecked', item.is_checked,
                  'metadata', item.metadata
                )
                ORDER BY item.ordinal
              )
              FROM work_record_items AS item
              WHERE item.record_id = record.id
            ),
            '[]'::json
          ) AS items
        FROM work_records AS record
        JOIN projects AS project
          ON project.id = record.project_id
        WHERE ($1::uuid IS NULL OR record.project_id = $1)
          AND ($2::text IS NULL OR record.record_type = $2)
          AND ($3::text IS NULL OR record.lifecycle_state = $3)
          AND ($4::text IS NULL OR record.attribution = $4)
          AND (
            $5::text IS NULL
            OR record.search_document @@ websearch_to_tsquery('simple', $5)
          )
      ), page AS (
        SELECT *
        FROM matching
        ORDER BY score DESC NULLS LAST, "recordedAt" DESC, id
        LIMIT $6
        OFFSET $7
      )
      SELECT
        (SELECT count(*)::integer FROM matching) AS total,
        COALESCE(
          (
            SELECT json_agg(
              row_to_json(page)
              ORDER BY score DESC NULLS LAST, "recordedAt" DESC, id
            )
            FROM page
          ),
          '[]'::json
        ) AS records
    `,
    [
      filters.projectID,
      filters.recordType,
      filters.lifecycleState,
      filters.attribution,
      filters.query,
      filters.limit,
      filters.offset,
    ],
  );
  send(response, 200, {
    records: result.rows[0].records,
    total: result.rows[0].total,
    limit: filters.limit,
    offset: filters.offset,
  });
}

async function listTurnSources(response, turnID) {
  if (!isUUID(turnID)) {
    throw new ProvenanceValidationError("turnId 값은 UUID여야 합니다.");
  }
  const result = await pool.query(
    `
      SELECT
        turn.id,
        turn.response_source_warning AS warning,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', source.id,
                'sourceKind', source.source_kind,
                'title', source.title,
                'locator', source.locator,
                'excerpt', source.excerpt,
                'ragDocumentId', source.rag_document_id,
                'workRecordId', source.work_record_id
              )
              ORDER BY source.ordinal, source.created_at, source.id
            )
            FROM turn_response_sources AS source
            WHERE source.turn_id = turn.id
          ),
          '[]'::json
        ) AS sources
      FROM turns AS turn
      WHERE turn.id = $1
    `,
    [turnID],
  );
  if (result.rowCount === 0) {
    send(response, 404, { error: "대화를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, {
    sources: result.rows[0].sources,
    warning: result.rows[0].warning,
  });
}

async function replaceTurnSources(response, turnID, body) {
  if (!isUUID(turnID)) {
    throw new ProvenanceValidationError("turnId 값은 UUID여야 합니다.");
  }
  const sources = normalizeResponseSources(body.sources);
  const stored = await withTransaction(async (client) => {
    const turn = await client.query(
      "SELECT id, status FROM turns WHERE id = $1 FOR UPDATE",
      [turnID],
    );
    if (turn.rowCount === 0) {
      return { outcome: "missing" };
    }
    if (["pending", "running"].includes(turn.rows[0].status)) {
      return { outcome: "running" };
    }
    const records = await replaceTurnResponseSources(
      client,
      turnID,
      sources,
    );
    await client.query(
      `
        UPDATE turns
        SET updated_at = now(), response_source_warning = NULL
        WHERE id = $1
      `,
      [turnID],
    );
    return { outcome: "stored", records };
  });
  if (stored.outcome === "missing") {
    send(response, 404, { error: "대화를 찾을 수 없습니다." });
    return;
  }
  if (stored.outcome === "running") {
    send(response, 409, {
      error: "진행 중인 대화의 출처는 완료 후 수정할 수 있습니다.",
    });
    return;
  }
  broadcast({ type: "feed.changed", turnId: turnID });
  send(response, 200, { sources: stored.records });
}

async function updateTurnFeedback(response, turnID, body) {
  if (!isUUID(turnID)) {
    throw new TurnFeedbackValidationError("turnId 값은 UUID여야 합니다.");
  }
  const feedback = normalizeTurnFeedback(body?.feedback);
  const stored = await withTransaction((client) =>
    replaceTurnFeedback(client, turnID, feedback)
  );
  if (stored.outcome === "missing") {
    send(response, 404, { error: "대화를 찾을 수 없습니다." });
    return;
  }
  if (stored.outcome === "unavailable") {
    send(response, 409, {
      error: "완료된 대화 응답만 평가할 수 있습니다.",
    });
    return;
  }
  broadcast({ type: "feed.changed", turnId: turnID });
  send(response, 200, { feedback: stored.feedback });
}

async function queryTurnFeed({
  turnID = null,
  query = null,
  limit,
  offset = 0,
  includesCharacterMinimums,
}) {
  const result = await pool.query(
    `
      WITH matching_turn_ids AS (
        SELECT t.id, t.started_at, session.character_id
        FROM turns AS t
        JOIN cli_sessions AS session
          ON session.id = t.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE $1::uuid IS NULL
          AND (
            $3::text IS NULL
            OR concat_ws(
              ' ',
              character.name,
              t.prompt,
              t.backend,
              t.model,
              t.effort,
              session.external_id,
              (
                SELECT text
                FROM messages
                WHERE turn_id = t.id
                  AND role = 'assistant'
                ORDER BY received_at DESC
                LIMIT 1
              )
            ) ILIKE '%' || $3 || '%'
          )
      ), ranked_character_turn_ids AS (
        SELECT
          matching.id,
          row_number() OVER (
            PARTITION BY matching.character_id
            ORDER BY matching.started_at DESC, matching.id DESC
          ) AS character_rank
        FROM matching_turn_ids AS matching
      ), selected_turn_ids AS (
        SELECT recent.id
        FROM (
          SELECT id
          FROM matching_turn_ids
          ORDER BY started_at DESC, id DESC
          LIMIT $2
          OFFSET $4
        ) AS recent
        UNION
        SELECT ranked.id
        FROM ranked_character_turn_ids AS ranked
        WHERE $1::uuid IS NULL
          AND $5::boolean
          AND $3::text IS NULL
          AND $4::integer = 0
          AND ranked.character_rank <= $6::integer
        UNION
        SELECT $1::uuid
        WHERE $1::uuid IS NOT NULL
      )
      SELECT
        t.id,
        c.id AS "characterId",
        c.name AS "characterName",
        c.backend AS "characterBackend",
        t.backend,
        t.model,
        t.effort,
        t.fast_mode AS "fastMode",
        t.origin,
        s.external_id AS "externalSessionId",
        conversation.workdir AS "conversationWorkdir",
        t.prompt,
        t.status,
        t.needs_input AS "needsInput",
        t.error_message AS "errorMessage",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning",
        turn_feedback.feedback,
        t.started_at AS "startedAt",
        t.ended_at AS "endedAt",
        t.updated_at AS "updatedAt",
        usage.cost_usd::double precision AS "estimatedCostUsd",
        workspace.review AS workspace,
        COALESCE(
          (
            SELECT text
            FROM messages
            WHERE turn_id = t.id
              AND role = 'assistant'
            ORDER BY received_at DESC
            LIMIT 1
          ),
          ''
        ) AS response,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', activity.id,
                'kind', activity.kind,
                'text', activity.text,
                'status', activity.status,
                'collaboration', activity.collaboration,
                'occurredAt', activity.occurred_at
              )
              ORDER BY activity.seq
            )
            FROM turn_activities AS activity
            WHERE activity.turn_id = t.id
          ),
          '[]'::json
        ) AS activities,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', source.id,
                'sourceKind', source.source_kind,
                'title', source.title,
                'locator', source.locator,
                'excerpt', source.excerpt,
                'ragDocumentId', source.rag_document_id,
                'workRecordId', source.work_record_id
              )
              ORDER BY source.ordinal, source.created_at, source.id
            )
            FROM turn_response_sources AS source
            WHERE source.turn_id = t.id
          ),
          '[]'::json
        ) AS sources
      FROM selected_turn_ids AS selected_turn
      JOIN turns AS t
        ON t.id = selected_turn.id
      JOIN cli_sessions AS s
        ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      JOIN characters AS c
        ON c.id = s.character_id
      LEFT JOIN usage_records AS usage
        ON usage.turn_id = t.id
      LEFT JOIN turn_response_feedback AS turn_feedback
        ON turn_feedback.turn_id = t.id
      LEFT JOIN LATERAL (
        SELECT json_build_object(
          'status', task_workspace.status,
          'repositoryRoot', task_workspace.repository_root,
          'worktreePath', task_workspace.worktree_path,
          'executionWorkdir', CASE
            WHEN task_workspace.status IN ('merged', 'closed')
              THEN task_workspace.source_workdir
            ELSE task_workspace.execution_workdir
          END,
          'branchName', task_workspace.branch_name,
          'baseBranch', task_workspace.base_branch,
          'baseCommit', task_workspace.base_commit,
          'reviewTree', task_workspace.review_tree,
          'headCommit', task_workspace.head_commit,
          'changedFiles', task_workspace.changed_files,
          'mergedCommit', task_workspace.merged_commit,
          'errorMessage', task_workspace.error_message
        ) AS review
        FROM task_workspaces AS task_workspace
        WHERE task_workspace.review_turn_id = t.id
          OR (
            task_workspace.review_turn_id IS NULL
            AND task_workspace.id = t.task_workspace_id
          )
        LIMIT 1
      ) AS workspace ON true
      ORDER BY t.started_at DESC, t.id DESC
    `,
    [
      turnID,
      limit,
      query,
      offset,
      includesCharacterMinimums,
      liveFeedMinimumTurnsPerCharacter,
    ],
  );
  return result.rows.map((turn) =>
    withSessionContext(withArtifactPreviews(turn)),
  );
}

async function queryLiveFeed({ turnID = null, limit }) {
  const page = await queryTurnFeed({
    turnID,
    limit,
    includesCharacterMinimums: true,
  });
  return page;
}

async function queryArchiveFeed({ query, limit, offset }) {
  const [turns, countResult] = await Promise.all([
    queryTurnFeed({
      query,
      limit,
      offset,
      includesCharacterMinimums: false,
    }),
    pool.query(
      `
        SELECT count(*)::integer AS total
        FROM turns AS t
        JOIN cli_sessions AS session
          ON session.id = t.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE (
          $1::text IS NULL
          OR concat_ws(
            ' ',
            character.name,
            t.prompt,
            t.backend,
            t.model,
            t.effort,
            session.external_id,
            (
              SELECT text
              FROM messages
              WHERE turn_id = t.id
                AND role = 'assistant'
              ORDER BY received_at DESC
              LIMIT 1
            )
          ) ILIKE '%' || $1 || '%'
        )
      `,
      [query],
    ),
  ]);
  return {
    turns,
    total: countResult.rows[0]?.total ?? 0,
  };
}

function withSessionContext(turn) {
  return {
    ...turn,
    sessionContext: sessionContextUsage({
      backend: turn.backend ?? turn.characterBackend,
      sessionID: turn.externalSessionId,
      model: turn.model,
      at: turn.endedAt ?? Date.now(),
    }),
  };
}

async function workspaceReview(response, route, method, request) {
  if (!runtime) {
    send(response, 503, { error: "CLI 실행기가 준비되지 않았습니다." });
    return;
  }

  try {
    if (method === "GET" && route.action === null) {
      send(
        response,
        200,
        await runtime.fetchWorkspaceReview(route.turnID),
      );
      return;
    }
    if (method === "POST" && route.action === "approve") {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      const body = await readJSON(request);
      send(
        response,
        200,
        await runtime.approveWorkspace(route.turnID, body.reviewTree),
      );
      return;
    }
    if (method === "POST" && route.action === "reject") {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await readJSON(request);
      send(
        response,
        200,
        await runtime.rejectWorkspace(route.turnID),
      );
      return;
    }
    send(response, 404, { error: "경로를 찾을 수 없습니다." });
  } catch (error) {
    if (error instanceof AgentJobNotFoundError) {
      send(response, 404, { error: error.message });
      return;
    }
    if (
      error instanceof AgentBusyError ||
      error instanceof GitWorkspaceError
    ) {
      send(response, 409, { error: error.message });
      return;
    }
    throw error;
  }
}

function trustedJSONMutation(request, response) {
  const contentType = String(request.headers["content-type"] ?? "")
    .toLowerCase();
  if (!contentType.startsWith("application/json")) {
    send(response, 415, {
      error: "상태 변경 요청은 application/json 형식이어야 합니다.",
    });
    return false;
  }
  const origin = request.headers.origin;
  if (!origin) {
    return true;
  }
  try {
    const originURL = new URL(origin);
    const hostname = originURL.hostname;
    if (
      ["127.0.0.1", "localhost", "::1"].includes(hostname) &&
      originURL.host === request.headers.host
    ) {
      return true;
    }
  } catch {
    // 올바르지 않은 Origin은 아래에서 거절한다.
  }
  send(response, 403, { error: "신뢰할 수 없는 요청 출처입니다." });
  return false;
}

async function liveFeed(response, url) {
  const requestedLimit = Number(url.searchParams.get("limit") ?? 120);
  const limit = Math.max(20, Math.min(requestedLimit, 300));
  send(response, 200, {
    turns: await queryLiveFeed({ limit }),
    costSummary: await turnCostSummaryCache.read(),
  });
}

async function archiveFeed(response, url) {
  const requestedLimit = Number(url.searchParams.get("limit") ?? 12);
  const requestedOffset = Number(url.searchParams.get("offset") ?? 0);
  const query = url.searchParams.get("query")?.trim() || null;
  const limit = Math.max(1, Math.min(requestedLimit, 50));
  const offset = Math.max(
    0,
    Number.isFinite(requestedOffset) ? Math.floor(requestedOffset) : 0,
  );
  send(response, 200, await queryArchiveFeed({ query, limit, offset }));
}

async function liveFeedTurn(response, turnID) {
  const turns = await queryLiveFeed({ turnID, limit: 1 });
  if (turns.length === 0) {
    send(response, 404, { error: "대화를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, {
    turn: turns[0],
    costSummary: await turnCostSummaryCache.read(),
  });
}

function withArtifactPreviews(turn, sessionID = turn.externalSessionId) {
  const generatedImages = generatedImagesForTurn({
    sessionID,
    startedAt: turn.startedAt,
    endedAt: turn.endedAt,
    backend: turn.backend,
    generatedRoot: generatedImageRoot(turn.backend),
  });
  return {
    ...turn,
    response: appendLocalImagePreviews(
      turn.response,
      generatedImages,
    ),
  };
}

async function startAgentJob(response, body) {
  if (!runtime) {
    send(response, 503, { error: "CLI 실행기가 준비되지 않았습니다." });
    return;
  }

  try {
    const job = await runtime.start({
      characterID: String(body.characterId ?? ""),
      prompt: body.prompt,
      conversationID: body.conversationId,
      attachmentPaths: body.attachmentPaths,
    });
    send(response, 202, job);
  } catch (error) {
    if (error instanceof AgentDrainingError) {
      send(response, 503, { error: error.message });
      return;
    }
    if (error instanceof CharacterNotFoundError) {
      send(response, 404, { error: error.message });
      return;
    }
    if (error instanceof AgentBusyError || error instanceof GitWorkspaceError) {
      send(response, 409, { error: error.message });
      return;
    }
    throw error;
  }
}

async function openTerminalSession(response, body) {
  if (!terminalSessions) {
    send(response, 503, { error: "터미널 실행기가 준비되지 않았습니다." });
    return;
  }
  try {
    send(
      response,
      201,
      await terminalSessions.open(String(body.characterId ?? "")),
    );
  } catch (error) {
    if (error instanceof AgentDrainingError) {
      send(response, 503, { error: error.message });
      return;
    }
    if (error instanceof CharacterNotFoundError) {
      send(response, 404, { error: error.message });
      return;
    }
    if (error instanceof AgentBusyError) {
      send(response, 409, { error: error.message });
      return;
    }
    throw error;
  }
}

async function closeTerminalSession(response, characterID) {
  if (!terminalSessions) {
    send(response, 503, { error: "터미널 실행기가 준비되지 않았습니다." });
    return;
  }
  const closed = await terminalSessions.close(characterID);
  send(response, closed ? 200 : 404, closed
    ? { ok: true }
    : { error: "열린 터미널 세션을 찾을 수 없습니다." });
}

async function recordTerminalEvent(response, characterID, body) {
  if (!terminalSessions) {
    send(response, 503, { error: "터미널 실행기가 준비되지 않았습니다." });
    return;
  }
  try {
    send(response, 202, await terminalSessions.handleEvent(characterID, body));
  } catch (error) {
    if (error instanceof AgentBusyError) {
      send(response, 409, { error: error.message });
      return;
    }
    send(response, 404, {
      error: error instanceof Error ? error.message : String(error),
    });
  }
}

async function interruptTerminalSession(response, characterID) {
  if (!terminalSessions) {
    send(response, 503, { error: "터미널 실행기가 준비되지 않았습니다." });
    return;
  }
  try {
    send(response, 200, await terminalSessions.interrupt(characterID));
  } catch (error) {
    send(response, 404, {
      error: error instanceof Error ? error.message : String(error),
    });
  }
}

async function cancelAgentJob(response, characterID) {
  if (!runtime) {
    send(response, 503, { error: "CLI 실행기가 준비되지 않았습니다." });
    return;
  }

  try {
    send(response, 200, await runtime.cancel(characterID));
  } catch (error) {
    if (error instanceof AgentJobNotFoundError) {
      send(response, 404, { error: error.message });
      return;
    }
    throw error;
  }
}

async function sessionForTurn(
  client,
  conversationID,
  characterID,
  externalSessionID,
) {
  const activeSession = await client.query(
    `
      SELECT active.cli_session_id
      FROM active_cli_sessions AS active
      WHERE active.character_id = $1
      FOR UPDATE
    `,
    [characterID],
  );
  const previousSessionID =
    activeSession.rows[0]?.cli_session_id ?? null;

  if (!externalSessionID) {
    if (previousSessionID) {
      const previous = await client.query(
        `
          SELECT id
          FROM cli_sessions
          WHERE id = $1
            AND ended_at IS NULL
        `,
        [previousSessionID],
      );
      if (previous.rowCount > 0) {
        return previous.rows[0].id;
      }
      await client.query(
        `
          DELETE FROM active_cli_sessions
          WHERE character_id = $1
        `,
        [characterID],
      );
    }

    const pending = await client.query(
      `
        SELECT id
        FROM cli_sessions
        WHERE conversation_id = $1
          AND character_id = $2
          AND external_id IS NULL
          AND ended_at IS NULL
        ORDER BY started_at DESC, id DESC
        LIMIT 1
      `,
      [conversationID, characterID],
    );
    if (pending.rowCount > 0) {
      return pending.rows[0].id;
    }

    const inserted = await client.query(
      `
        INSERT INTO cli_sessions (
          conversation_id,
          character_id,
          previous_session_id
        )
        VALUES ($1, $2, $3)
        RETURNING id
      `,
      [conversationID, characterID, previousSessionID],
    );
    return inserted.rows[0].id;
  }

  let session = await client.query(
    `
      SELECT id, ended_at AS "endedAt"
      FROM cli_sessions
      WHERE character_id = $1
        AND external_id = $2
    `,
    [characterID, externalSessionID],
  );

  if (session.rows[0]?.endedAt) {
    throw new Error("종료된 CLI 세션은 다시 활성화할 수 없습니다.");
  }

  if (session.rowCount === 0) {
    const pending = await client.query(
      `
        SELECT id
        FROM cli_sessions
        WHERE conversation_id = $1
          AND character_id = $2
          AND external_id IS NULL
          AND ended_at IS NULL
        ORDER BY started_at DESC, id DESC
        LIMIT 1
      `,
      [conversationID, characterID],
    );
    if (pending.rowCount > 0) {
      session = await client.query(
        `
          UPDATE cli_sessions
          SET external_id = $2
          WHERE id = $1
          RETURNING id
        `,
        [pending.rows[0].id, externalSessionID],
      );
    } else {
      session = await client.query(
        `
          INSERT INTO cli_sessions (
            conversation_id,
            character_id,
            external_id,
            previous_session_id
          )
          VALUES ($1, $2, $3, $4)
          RETURNING id
        `,
        [
          conversationID,
          characterID,
          externalSessionID,
          previousSessionID,
        ],
      );
    }
  }

  const sessionID = session.rows[0].id;
  await client.query(
    `
      UPDATE cli_sessions
      SET ended_at = COALESCE(ended_at, now())
      WHERE character_id = $1
        AND id <> $2
        AND ended_at IS NULL
    `,
    [characterID, sessionID],
  );
  await client.query(
    `
      UPDATE cli_sessions
      SET ended_at = NULL
      WHERE id = $1
    `,
    [sessionID],
  );
  await client.query(
    `
      INSERT INTO active_cli_sessions (
        character_id,
        cli_session_id
      )
      VALUES ($1, $2)
      ON CONFLICT (character_id) DO UPDATE
      SET
        cli_session_id = EXCLUDED.cli_session_id,
        activated_at = CASE
          WHEN active_cli_sessions.cli_session_id =
            EXCLUDED.cli_session_id
          THEN active_cli_sessions.activated_at
          ELSE now()
        END,
        updated_at = now()
    `,
    [characterID, sessionID],
  );
  return sessionID;
}

async function recordTurn(response, body) {
  const turnID = body.turnId ?? randomUUID();
  const conversationID = body.conversationId;
  const characterID = body.characterId;
  const externalSessionID = String(
    body.externalSessionId ?? "",
  ).trim() || null;
  const executionBackend = String(body.backend ?? "").trim() || null;
  const executionModel = String(body.model ?? "").trim() || null;
  const executionEffort = String(body.effort ?? "").trim() || null;
  const hasExecutionFastMode = Object.prototype.hasOwnProperty.call(
    body,
    "fastMode",
  );
  const executionFastMode = hasExecutionFastMode &&
      typeof body.fastMode === "boolean"
    ? body.fastMode
    : false;
  const executionCapabilities = executionBackend && executionModel
    ? modelCatalogService?.modelCapabilities(
      executionBackend,
      executionModel,
    ) ?? null
    : null;

  if (!conversationID || !characterID || !body.prompt) {
    send(response, 400, { error: "대화, 캐릭터, 프롬프트가 필요합니다." });
    return;
  }
  if (
    executionBackend &&
    !AGENT_BACKENDS.includes(executionBackend)
  ) {
    send(response, 400, { error: "지원하지 않는 실행 CLI입니다." });
    return;
  }
  if (
    executionEffort &&
    !(executionCapabilities?.efforts ?? backendEfforts(
      executionBackend,
      executionModel,
    )).includes(
      executionEffort,
    )
  ) {
    send(response, 400, { error: "지원하지 않는 실행 추론 레벨입니다." });
    return;
  }
  if (hasExecutionFastMode && typeof body.fastMode !== "boolean") {
    send(response, 400, { error: "실행 Fast 모드 값이 올바르지 않습니다." });
    return;
  }
  if (
    executionBackend &&
    executionFastMode === true &&
    !(executionCapabilities
      ? executionCapabilities.supportsFastMode === true
      : backendSupportsFastMode(executionBackend, executionModel))
  ) {
    send(response, 400, {
      error: "선택한 실행 CLI와 모델은 Fast 모드를 지원하지 않습니다.",
    });
    return;
  }

  await withTransaction(async (client) => {
    await client.query(
      `
        SELECT id
        FROM characters
        WHERE id = $1
        FOR UPDATE
      `,
      [characterID],
    );

    const existingTurn = await client.query(
      `
        SELECT id
        FROM turns
        WHERE id = $1
      `,
      [turnID],
    );
    if (existingTurn.rowCount > 0) {
      return;
    }

    await client.query(
      `
        INSERT INTO conversations (id, title, workdir)
        VALUES ($1, $2, $3)
        ON CONFLICT (id) DO NOTHING
      `,
      [
        conversationID,
        body.title ?? "새 업무",
        body.workdir ?? "",
      ],
    );

    const sessionID = await sessionForTurn(
      client,
      conversationID,
      characterID,
      externalSessionID,
    );

    const insertedTurn = await client.query(
      `
        INSERT INTO turns (
          id,
          cli_session_id,
          backend,
          model,
          effort,
          fast_mode,
          origin,
          prompt,
          started_at,
          ended_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'gui', $7, $8, $9)
        ON CONFLICT (id) DO NOTHING
        RETURNING id
      `,
      [
        turnID,
        sessionID,
        executionBackend,
        executionModel,
        executionEffort,
        executionFastMode,
        body.prompt,
        body.startedAt ?? new Date().toISOString(),
        body.finishedAt ?? new Date().toISOString(),
      ],
    );

    if (insertedTurn.rowCount > 0) {
      await client.query(
        `
          INSERT INTO messages (turn_id, role, text)
          VALUES ($1, 'user', $2), ($1, 'assistant', $3)
        `,
        [turnID, body.prompt, body.response ?? ""],
      );
    }
  });

  send(response, 201, { turnId: turnID });
}

async function addRAGDocument(response, body) {
  const content = String(body.content ?? "").trim();
  if (!content) {
    send(response, 400, { error: "RAG 문서 내용이 필요합니다." });
    return;
  }

  const embedding = Array.isArray(body.embedding)
    ? `[${body.embedding.join(",")}]`
    : null;
  const embeddingModel = embedding
    ? String(body.embeddingModel ?? "external:unspecified").trim() ||
      "external:unspecified"
    : null;
  const result = await pool.query(
    `
      INSERT INTO rag_documents (
        source,
        title,
        content,
        metadata,
        embedding,
        embedding_model,
        embedding_updated_at
      )
      VALUES ($1, $2, $3, $4::jsonb, $5::vector, $6, CASE
        WHEN $5::vector IS NULL THEN NULL
        ELSE now()
      END)
      RETURNING id
    `,
    [
      body.source ?? null,
      body.title ?? null,
      content,
      JSON.stringify(body.metadata ?? {}),
      embedding,
      embeddingModel,
    ],
  );
  send(response, 201, result.rows[0]);
}

function routeWikiPage(pathname) {
  const match = pathname.match(/^\/api\/wiki\/pages\/([0-9a-f-]{36})$/i);
  return match ? match[1] : null;
}

function routeWikiProposalAction(pathname) {
  const match = pathname.match(
    /^\/api\/wiki\/proposals\/([0-9a-f-]{36})\/(verify|approve|reject)$/i,
  );
  return match ? { proposalID: match[1], action: match[2] } : null;
}

async function wikiPagesEndpoint(response, url) {
  const pages = await listWikiPages(pool, {
    query: url.searchParams.get("query"),
    limit: url.searchParams.get("limit") ?? undefined,
    repositoryRoot: runtime.repositoryRoot,
  });
  send(response, 200, { pages });
}

async function wikiPageEndpoint(response, pageID) {
  const page = await getWikiPage(pool, pageID, {
    repositoryRoot: runtime.repositoryRoot,
  });
  if (!page) {
    send(response, 404, { error: "위키 페이지를 찾을 수 없습니다." });
    return;
  }
  send(response, 200, { page });
}

async function deleteWikiPageEndpoint(response, request, pageID) {
  if (!trustedJSONMutation(request, response)) {
    return;
  }
  // 승인·거절과 같은 intent guard다. 사내 위키 화면의 삭제 버튼만
  // 이 헤더를 보내므로 일반 API 호출로는 문서가 지워지지 않는다.
  const userDecision = request.headers["x-officestra-user-decision"];
  if (userDecision !== `delete:${pageID}`) {
    send(response, 403, {
      error: "사내 위키 화면에서 사용자가 직접 결정해야 합니다.",
    });
    return;
  }
  const page = await withTransaction((client) =>
    archiveWikiPage(client, {
      pageID,
      repositoryRoot: runtime.repositoryRoot,
    }));
  send(response, 200, { page });
}

async function wikiProposalsEndpoint(response, url) {
  const proposals = await listWikiProposals(pool, {
    state: url.searchParams.get("state"),
    repositoryRoot: runtime.repositoryRoot,
  });
  send(response, 200, { proposals });
}

async function createWikiProposalEndpoint(response, request) {
  if (!trustedJSONMutation(request, response)) {
    return;
  }
  const body = await readJSON(request);
  if (body.approvalTier != null && body.approvalTier !== "user") {
    send(response, 400, {
      error: "공개 제안 API는 사용자 승인 등급만 만들 수 있습니다.",
    });
    return;
  }
  const proposal = await withTransaction(
    (client) => createWikiProposal(client, {
      repositoryRoot: runtime.repositoryRoot,
      pageKey: body.pageKey,
      kind: body.kind,
      approvalGrade: "user",
      state: "pending_user",
      draftTitle: body.title,
      draftBody: body.body,
      sourceWorkRecordIds: body.sourceRecordIds,
      baseVersion: body.baseVersion,
    }),
  );
  send(response, 201, { proposal });
}

async function wikiProposalActionEndpoint(response, request, route) {
  if (!trustedJSONMutation(request, response)) {
    return;
  }
  // 로컬 단일 사용자 앱의 명시적인 버튼 동작과 일반 API 호출을
  // 구분하는 intent guard다. 같은 OS 사용자에 대한 보안 경계는 아니다.
  const userDecision = request.headers["x-officestra-user-decision"];
  if (
    ["approve", "reject"].includes(route.action) &&
    userDecision !== `${route.action}:${route.proposalID}`
  ) {
    send(response, 403, {
      error: "사내 위키 화면에서 사용자가 직접 결정해야 합니다.",
    });
    return;
  }
  const body = await readJSON(request);
  if (route.action === "verify") {
    const proposal = await withTransaction((client) =>
      verifyWikiProposal(client, {
        proposalID: route.proposalID,
        verifierCharacterID: body.characterId ?? null,
        repositoryRoot: runtime.repositoryRoot,
      }));
    send(response, 200, { proposal });
    return;
  }
  if (route.action === "reject") {
    const proposal = await withTransaction((client) =>
      rejectWikiProposal(client, {
        proposalID: route.proposalID,
        reason: body.reason ?? null,
        repositoryRoot: runtime.repositoryRoot,
      }));
    send(response, 200, { proposal });
    return;
  }
  try {
    const outcome = await withTransaction((client) =>
      approveWikiProposal(client, {
        proposalID: route.proposalID,
        actorType: "user",
        repositoryRoot: runtime.repositoryRoot,
      }));
    send(response, outcome.conflicted ? 409 : 200, {
      proposal: outcome.proposal,
    });
  } catch (error) {
    // 같은 pageKey 동시 게시는 유니크 제약으로 트랜잭션이 중단되므로
    // 새 트랜잭션에서 제안을 conflict로 종결한다.
    if (!isWikiPageKeyRaceError(error)) {
      throw error;
    }
    const proposal = await withTransaction((client) =>
      resolveWikiPageKeyRace(client, route.proposalID, {
        repositoryRoot: runtime.repositoryRoot,
      }));
    send(response, 409, { proposal });
  }
}

async function searchRAG(response, body) {
  const result = await searchRAGDocuments(pool, {
    query: body.query,
    embedding: body.embedding,
    limit: body.limit,
    embeddingService: localEmbeddingService,
  });
  if (result.fallbackError) {
    console.warn(
      `임베딩 비활성(사유): ${result.fallbackError}. ` +
        "PostgreSQL 전문검색으로 대체했습니다.",
    );
  }
  send(response, 200, { documents: result.documents });
}

const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    const characterID = routeCharacterName(url.pathname);
    const settingsCharacterID = routeCharacterSettings(url.pathname);
    const identityPromptCharacterID = routeCharacterIdentityPrompt(
      url.pathname,
    );
    const contextSettingsCharacterID = routeCharacterContextSettings(
      url.pathname,
    );
    const contextCompactCharacterID = routeCharacterContextCompact(
      url.pathname,
    );
    const historyCharacterID = routeCharacterHistory(url.pathname);
    const jobCharacterID = routeAgentJob(url.pathname);
    const terminalSessionRoute = routeTerminalSession(url.pathname);
    const liveFeedTurnID = routeLiveFeedTurn(url.pathname);
    const turnSourcesID = routeTurnSources(url.pathname);
    const turnFeedbackID = routeTurnFeedback(url.pathname);
    const workspaceReviewRoute = routeWorkspaceReview(url.pathname);

    if (request.method === "GET" && url.pathname === "/health") {
      try {
        await pool.query("SELECT 1");
        send(response, 200, officeBackendHealth({ runtime }));
      } catch {
        send(response, 503, officeBackendHealth({
          runtime,
          databaseOK: false,
        }));
      }
    } else if (url.pathname === "/api/maintenance/drain") {
      await backendMaintenance(response, request.method ?? "GET", request);
    } else if (
      request.method === "PUT" &&
      url.pathname === "/api/runtime/cli-paths"
    ) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await updateRuntimeCLIPaths(response, await readJSON(request));
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/model-catalog"
    ) {
      await modelCatalog(response, url);
    } else if (
      request.method === "PUT" &&
      url.pathname === "/api/model-catalog/exclusions"
    ) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await updateModelCatalogExclusions(response, await readJSON(request));
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/characters"
    ) {
      await listCharacters(response);
    } else if (
      request.method === "PUT" &&
      url.pathname === "/api/characters/settings/bulk"
    ) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await updateCharacterSettingsBulk(response, await readJSON(request));
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/active-sessions"
    ) {
      await listActiveSessions(response);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/usage-summary"
    ) {
      await usageSummary(response, url);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/usage-report"
    ) {
      await usageReport(response, url);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/cli-updates"
    ) {
      await cliUpdates(response, url);
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/cli-updates/apply"
    ) {
      await applyCLIUpdatesEndpoint(response, request);
    } else if (request.method === "GET" && historyCharacterID) {
      await characterHistory(response, historyCharacterID);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/history"
    ) {
      await globalHistory(response, url);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/work-records"
    ) {
      await listWorkRecords(response, url);
    } else if (request.method === "GET" && turnSourcesID) {
      await listTurnSources(response, turnSourcesID);
    } else if (request.method === "PUT" && turnSourcesID) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await replaceTurnSources(
        response,
        turnSourcesID,
        await readJSON(request),
      );
    } else if (request.method === "PUT" && turnFeedbackID) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await updateTurnFeedback(
        response,
        turnFeedbackID,
        await readJSON(request),
      );
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/live-feed"
    ) {
      await liveFeed(response, url);
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/archive-feed"
    ) {
      await archiveFeed(response, url);
    } else if (request.method === "GET" && liveFeedTurnID) {
      await liveFeedTurn(response, liveFeedTurnID);
    } else if (workspaceReviewRoute) {
      await workspaceReview(
        response,
        workspaceReviewRoute,
        request.method ?? "GET",
        request,
      );
    } else if (request.method === "PUT" && characterID) {
      await updateCharacterName(
        response,
        characterID,
        await readJSON(request),
      );
    } else if (request.method === "PUT" && settingsCharacterID) {
      await updateCharacterSettings(
        response,
        settingsCharacterID,
        await readJSON(request),
      );
    } else if (request.method === "PUT" && identityPromptCharacterID) {
      await updateCharacterIdentityPrompt(
        response,
        identityPromptCharacterID,
        await readJSON(request),
      );
    } else if (
      request.method === "PUT" &&
      contextSettingsCharacterID
    ) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await updateCharacterContextSettings(
        response,
        contextSettingsCharacterID,
        await readJSON(request),
      );
    } else if (
      request.method === "POST" &&
      contextCompactCharacterID
    ) {
      if (!trustedJSONMutation(request, response)) {
        return;
      }
      await readJSON(request);
      await compactCharacterContext(response, contextCompactCharacterID);
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/turns"
    ) {
      await recordTurn(response, await readJSON(request));
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/terminal-sessions"
    ) {
      send(response, 200, { sessions: terminalSessions?.list() ?? [] });
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/terminal-sessions"
    ) {
      if (!trustedJSONMutation(request, response)) return;
      await openTerminalSession(response, await readJSON(request));
    } else if (
      request.method === "POST" &&
      terminalSessionRoute?.events
    ) {
      if (!trustedJSONMutation(request, response)) return;
      await recordTerminalEvent(
        response,
        terminalSessionRoute.characterID,
        await readJSON(request),
      );
    } else if (
      request.method === "POST" &&
      terminalSessionRoute?.interrupt
    ) {
      if (!trustedJSONMutation(request, response)) return;
      await readJSON(request);
      await interruptTerminalSession(
        response,
        terminalSessionRoute.characterID,
      );
    } else if (
      request.method === "DELETE" &&
      terminalSessionRoute &&
      !terminalSessionRoute.events &&
      !terminalSessionRoute.interrupt
    ) {
      if (!trustedJSONMutation(request, response)) return;
      await readJSON(request);
      await closeTerminalSession(
        response,
        terminalSessionRoute.characterID,
      );
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/agent-jobs"
    ) {
      await startAgentJob(response, await readJSON(request));
    } else if (
      request.method === "DELETE" &&
      jobCharacterID
    ) {
      await cancelAgentJob(response, jobCharacterID);
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/rag/documents"
    ) {
      await addRAGDocument(response, await readJSON(request));
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/rag/search"
    ) {
      await searchRAG(response, await readJSON(request));
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/wiki/pages"
    ) {
      await wikiPagesEndpoint(response, url);
    } else if (
      request.method === "GET" &&
      routeWikiPage(url.pathname)
    ) {
      await wikiPageEndpoint(response, routeWikiPage(url.pathname));
    } else if (
      request.method === "DELETE" &&
      routeWikiPage(url.pathname)
    ) {
      await deleteWikiPageEndpoint(
        response,
        request,
        routeWikiPage(url.pathname),
      );
    } else if (
      request.method === "GET" &&
      url.pathname === "/api/wiki/proposals"
    ) {
      await wikiProposalsEndpoint(response, url);
    } else if (
      request.method === "POST" &&
      url.pathname === "/api/wiki/proposals"
    ) {
      await createWikiProposalEndpoint(response, request);
    } else if (
      request.method === "POST" &&
      routeWikiProposalAction(url.pathname)
    ) {
      await wikiProposalActionEndpoint(
        response,
        request,
        routeWikiProposalAction(url.pathname),
      );
    } else {
      send(response, 404, { error: "경로를 찾을 수 없습니다." });
    }
  } catch (error) {
    if (
      error instanceof WikiKnowledgeError ||
      error instanceof UsageReportError
    ) {
      send(response, error.statusCode, { error: error.message });
      return;
    }
    if (
      error instanceof ProvenanceValidationError ||
      error instanceof TurnFeedbackValidationError ||
      error instanceof ModelCatalogValidationError
    ) {
      send(response, 400, { error: error.message });
      return;
    }
    if (
      error instanceof AgentDrainingError
    ) {
      send(response, 503, { error: error.message });
      return;
    }
    if (
      error instanceof AgentBusyError ||
      error instanceof GitWorkspaceError
    ) {
      send(response, 409, { error: error.message });
      return;
    }
    send(response, 500, { error: error.message });
  }
});

webSocketServer.on("connection", (socket) => {
  sockets.add(socket);
  socket.send(JSON.stringify({
    type: "ready",
    compactingCharacterIds: runtime?.compactingCharacterIDs() ?? [],
  }));
  socket.on("close", () => {
    sockets.delete(socket);
  });
  socket.on("error", () => {
    sockets.delete(socket);
  });
});

server.on("upgrade", (request, socket, head) => {
  const url = new URL(request.url ?? "/", "http://127.0.0.1");
  if (url.pathname !== "/ws") {
    socket.destroy();
    return;
  }
  webSocketServer.handleUpgrade(request, socket, head, (client) => {
    webSocketServer.emit("connection", client, request);
  });
});

async function shutdown(signal) {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  console.log(`${signal} 신호를 받아 사무실 백엔드를 종료합니다.`);
  modelCatalogService?.stop();
  pricingCatalogService?.stop();
  await terminalSessions?.shutdown();
  runtime?.shutdown();
  for (const socket of sockets) {
    socket.terminate();
  }
  await new Promise((resolveClose) => {
    if (!server.listening) {
      resolveClose();
      return;
    }
    server.close(resolveClose);
  });
  await pool.end();
}

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.once(signal, () => {
    void shutdown(signal)
      .then(() => process.exit(0))
      .catch((error) => {
        console.error(
          "백엔드 종료 중 오류가 발생했습니다.",
          error instanceof Error ? error.message : String(error),
        );
        process.exit(1);
      });
  });
}

try {
  await migrate();
  const configuration = await readCharacterConfiguration();
  const repositoryRoot = await canonicalProjectRoot(configuration.workdir);
  const workspaceManager = new GitWorkspaceManager({
    sourceWorkdir: configuration.workdir,
    worktreeRoot: process.env.OFFICE_WORKTREE_ROOT,
  });
  await withTransaction(async (client) => {
    await syncCharacters(client, configuration);
  });
  pricingCatalogService = new PricingCatalogService({
    store: createPostgresPricingCatalogStore(pool),
  });
  try {
    await pricingCatalogService.loadCached();
  } catch (error) {
    console.warn(
      "저장된 가격 카탈로그를 읽지 못해 내장 기본값으로 시작합니다.",
      error instanceof Error ? error.message : String(error),
    );
  }
  pricingCatalogService.start();
  modelCatalogService = new ModelCatalogService({
    store: createPostgresModelCatalogStore(pool),
    resolveExecutable: configuredModelCatalogExecutable,
    onChanged(provider) {
      broadcast({ type: "model-catalog.changed", provider });
    },
  });
  try {
    await modelCatalogService.loadCached();
  } catch (error) {
    console.warn(
      "저장된 모델 카탈로그를 읽지 못해 내장 기본값으로 시작합니다.",
      error instanceof Error ? error.message : String(error),
    );
  }
  modelCatalogService.start();
  try {
    await withTransaction(async (client) => {
      await reconcileTerminalWorkRecordReviews(client, {
        repositoryRoot,
      });
    });
  } catch (error) {
    console.warn(
      "종료된 작업 기록 상태를 재조정하지 못했지만 기동을 계속합니다.",
      error instanceof Error ? error.message : String(error),
    );
  }
  try {
    await withTransaction(async (client) => {
      await syncWorkRecordRAGDocuments(client, {
        repositoryRoot,
      });
    });
  } catch (error) {
    console.warn(
      "파생 RAG 재색인에 실패했지만 백엔드를 계속 시작합니다.",
      error instanceof Error ? error.message : String(error),
    );
  }
  runtime = new AgentRuntime({
    pool,
    withTransaction,
    workdir: configuration.workdir,
    repositoryRoot,
    workspaceManager,
    broadcast,
    embeddingService: localEmbeddingService,
  });
  terminalSessions = new TerminalSessionManager({
    runtime,
    broadcast,
    port,
  });
  runtime.setTerminalSessionRegistry(terminalSessions);
  await runtime.recoverInterruptedJobs();
  server.listen(port, "127.0.0.1", () => {
    console.log(`사무실 백엔드 실행 중 http://127.0.0.1:${port}`);
    void startSlackBridge({
      pool,
      backendURL: `http://127.0.0.1:${port}`,
    }).catch((error) => {
      console.error(
        "Slack 연동을 시작하지 못했습니다.",
        error instanceof Error ? error.message : String(error),
      );
    });
  });
} catch (error) {
  console.error(error);
  await pool.end();
  process.exitCode = 1;
}
