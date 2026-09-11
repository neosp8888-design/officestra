// 이 파일은 백엔드에서 CLI 업무를 실행하고 공개 진행 상태를 PostgreSQL과 WebSocket에 전달한다.

import { spawn, spawnSync } from "node:child_process";
import { resolveLocalCharacter, snapshotLocalTurn, validateLocalResume } from './local-provider-service.mjs';
import { once } from "node:events";
import {
  accessSync,
  closeSync,
  constants,
  copyFileSync,
  existsSync,
  linkSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import {
  basename,
  delimiter,
  dirname,
  extname,
  isAbsolute,
  join,
  resolve,
} from "node:path";
import { createInterface } from "node:readline";
import { randomUUID } from "node:crypto";

import {
  claudeSessionUsage,
  decodeAgentResponse,
  fileChangeActivityText,
  parseAgentEvent,
} from "./agent-event-parser.mjs";
import { backendExecutableName } from "./agent-provider.mjs";
import {
  antigravityPlaywrightEnvironment,
} from "./antigravity-playwright.mjs";
import {
  antigravitySessionUsage,
} from "./antigravity-local-state.mjs";
import {
  antigravityLatestStepIndex,
  antigravityStepReasonings,
} from "./antigravity-reasoning.mjs";
import {
  claudeTranscriptTurnUsage,
  codexRolloutTurnUsage,
} from "./terminal-usage.mjs";
import {
  CodexRolloutCollaborationTracker,
} from "./codex-rollout-collaboration.mjs";
import { compactCodexThread } from "./codex-context-compactor.mjs";
import { ClaudePersistentWorker } from "./claude-persistent-worker.mjs";
import {
  appendLocalImagePreviews,
  generatedImageRoot,
  listGeneratedImages,
} from "./local-artifacts.mjs";
import { GitWorkspaceError } from "./git-workspace.mjs";
import { estimateTokenCost } from "./token-cost-estimator.mjs";
import { sessionContextUsage } from "./session-context-usage.mjs";
import {
  STRUCTURED_RESULT_ENV,
  applyStructuredTurnResult,
  consumeStructuredTurnResult,
  discardStructuredTurnResult,
  identityPromptWithStructuredResult,
  prepareStructuredTurnResult,
  structuredResultToolDirectory,
  structuredTurnResultPath,
} from "./structured-turn-result.mjs";
import {
  ProvenanceValidationError,
  portableResponseSources,
  replaceTurnResponseSources,
} from "./work-record-provenance.mjs";
import {
  persistCompletedTurnWorkRecord,
  syncWorkRecordRAGDocuments,
  transitionTurnWorkRecordReview,
} from "./work-record-memory.mjs";
import { embedRAGDocumentsBestEffort } from "./rag-embeddings.mjs";
import { createWikiProposal } from "./wiki-knowledge.mjs";

const MAX_FILE_SNAPSHOT_BYTES = 8 * 1024 * 1024;
const MAX_TURN_SNAPSHOT_BYTES = 24 * 1024 * 1024;
const ROLLOUT_TAIL_CHUNK_BYTES = 64 * 1024;
const MAX_WORKSPACE_DIFF_BYTES = 512 * 1024;
const CODEX_ROLLOUT_MONITOR_INTERVAL_MS = 400;
const ANTIGRAVITY_REASONING_MONITOR_INTERVAL_MS = 400;
const rolloutPathCache = new Map();

const BLOCKED_WORKSPACE_STATUSES = new Set([
  "awaiting_approval",
  "merging",
  "conflict",
]);
const TASK_WORKSPACE_EXECUTION_CONSTRAINT =
  "task_workspaces_one_open_per_session_idx";
export class AgentBusyError extends Error {}
export class AgentDrainingError extends Error {}
export class AgentJobNotFoundError extends Error {}
export class AgentSessionNotFoundError extends Error {}
export class CharacterNotFoundError extends Error {}

export async function persistTurnWikiProposals(client, {
  repositoryRoot,
  workRecordID,
  turnID,
  characterID,
  proposals = [],
  parserWarning = null,
  needsInput = false,
}) {
  const warnings = parserWarning ? [parserWarning] : [];
  if (
    needsInput ||
    !workRecordID ||
    !Array.isArray(proposals) ||
    proposals.length === 0
  ) {
    return warnings.length > 0 ? warnings.join(" ") : null;
  }

  for (const [ordinal, proposal] of proposals.entries()) {
    const savepoint = `wiki_proposal_${ordinal}`;
    await client.query(`SAVEPOINT ${savepoint}`);
    try {
      await createWikiProposal(client, {
        repositoryRoot,
        pageKey: proposal.pageKey,
        kind: proposal.kind,
        approvalGrade: proposal.approvalTier,
        state: proposal.approvalTier === "user"
          ? "pending_user"
          : "drafted",
        ordinal,
        draftTitle: proposal.title,
        draftBody: proposal.body,
        sourceWorkRecordIds: [workRecordID],
        authorCharacterId: characterID,
        authorTurnId: turnID,
      });
    } catch (error) {
      await client.query(`ROLLBACK TO SAVEPOINT ${savepoint}`);
      warnings.push(
        `위키 수정안 ${ordinal + 1}번을 저장하지 못했습니다: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    } finally {
      await client.query(`RELEASE SAVEPOINT ${savepoint}`);
    }
  }
  return warnings.length > 0 ? warnings.join(" ") : null;
}

export class AgentRuntime {
  constructor({
    pool,
    withTransaction,
    workdir,
    repositoryRoot = workdir,
    broadcast,
    workspaceManager = null,
    rolloutReaderFactory = createRolloutReader,
    claudeWorkerFactory = (options) => new ClaudePersistentWorker(options),
    codexCompactor = compactCodexThread,
    contextUsageReader = sessionContextUsage,
    antigravityUsageReader = antigravitySessionUsage,
    antigravityReasoningReader = antigravityStepReasonings,
    antigravityReasoningBaselineReader = antigravityLatestStepIndex,
    embeddingService = null,
    localProviders = null,
  }) {
    this.pool = pool;
    this.withTransaction = withTransaction;
    this.workdir = workdir;
    this.repositoryRoot = repositoryRoot;
    this.broadcast = broadcast;
    this.workspaceManager = workspaceManager;
    this.rolloutReaderFactory = rolloutReaderFactory;
    this.claudeWorkerFactory = claudeWorkerFactory;
    this.codexCompactor = codexCompactor;
    this.contextUsageReader = contextUsageReader;
    this.antigravityUsageReader = antigravityUsageReader;
    this.antigravityReasoningReader = antigravityReasoningReader;
    this.antigravityReasoningBaselineReader = antigravityReasoningBaselineReader;
    this.embeddingService = embeddingService;
    this.localProviders = localProviders;
    this.running = new Map();
    this.claudeWorkers = new Map();
    this.compactingCharacters = new Set();
    this.preparingCharacters = new Set();
    this.preparingJobs = new Set();
    this.postProcessingJobs = new Set();
    this.terminalSessionRegistry = null;
    this.draining = false;
  }

  setTerminalSessionRegistry(registry) {
    this.terminalSessionRegistry = registry;
  }

  get acceptingJobs() {
    return !this.draining;
  }

  activeWorkCount() {
    return this.preparingJobs.size +
      this.running.size +
      this.compactingCharacters.size +
      this.postProcessingJobs.size +
      (this.terminalSessionRegistry?.size ?? 0);
  }

  maintenanceStatus() {
    const activeTurnCount = this.activeWorkCount();
    return {
      acceptingJobs: this.acceptingJobs,
      draining: this.draining,
      activeTurnCount,
      idle: activeTurnCount === 0,
    };
  }

  compactingCharacterIDs() {
    return [...this.compactingCharacters].sort();
  }

  beginDrain() {
    this.draining = true;
    return this.maintenanceStatus();
  }

  cancelDrain() {
    this.draining = false;
    return this.maintenanceStatus();
  }

  async withPostProcessing(label, operation) {
    const token = Symbol(label);
    this.postProcessingJobs.add(token);
    try {
      return await operation();
    } finally {
      this.postProcessingJobs.delete(token);
    }
  }

  async recoverInterruptedJobs() {
    const result = await this.pool.query(
      `
        WITH existing_terminal_turns AS (
          SELECT id, status
          FROM turns
          WHERE status IN ('completed', 'failed', 'interrupted')
        ), interrupted_turns AS (
          UPDATE turns
          SET
            status = 'interrupted',
            error_message =
              '백엔드가 재시작되어 이전 실시간 출력 연결이 종료됐습니다.',
            ended_at = COALESCE(ended_at, now()),
            updated_at = now()
          WHERE status IN ('pending', 'running')
          RETURNING id, task_workspace_id AS "taskWorkspaceID"
        ), terminal_turns AS (
          SELECT id, status FROM existing_terminal_turns
          UNION ALL
          SELECT id, 'interrupted' AS status FROM interrupted_turns
        ), closed_activities AS (
          UPDATE turn_activities AS activity
          SET
            status = CASE
              WHEN turn.status = 'completed' THEN 'completed'
              ELSE 'failed'
            END,
            collaboration = CASE
              WHEN activity.kind = 'collaboration'
                AND jsonb_typeof(activity.collaboration) = 'object'
              THEN jsonb_set(
                activity.collaboration,
                '{agentStatus}',
                to_jsonb((CASE
                  WHEN turn.status = 'completed' THEN 'completed'
                  ELSE 'errored'
                END)::text),
                true
              )
              ELSE activity.collaboration
            END
          FROM terminal_turns AS turn
          WHERE activity.turn_id = turn.id
            AND activity.status = 'running'
          RETURNING activity.id
        )
        SELECT id, "taskWorkspaceID" FROM interrupted_turns
      `,
    );
    const interruptedProvisioning = await this.pool.query(
      `
        SELECT *
        FROM task_workspaces
        WHERE status = 'provisioning'
      `,
    );
    const cleanupFailures = [];
    if (this.workspaceManager) {
      for (const row of interruptedProvisioning.rows ?? []) {
        const workspace = workspaceFromRow(row);
        try {
          if (
            typeof this.workspaceManager.cleanupProvisioning === "function"
          ) {
            await this.workspaceManager.cleanupProvisioning(workspace);
          } else {
            await this.workspaceManager.cleanup(workspace);
          }
        } catch (error) {
          cleanupFailures.push({
            workspaceID: workspace.id,
            message: error instanceof Error ? error.message : String(error),
          });
        }
      }
    }
    await this.pool.query(
      `
        UPDATE task_workspaces
        SET
          status = 'failed',
          error_message = CASE status
            WHEN 'merging' THEN
              '백엔드 재시작 전에 통합이 시작됐습니다. 원본 Git 상태를 확인하세요.'
            ELSE
              '백엔드 재시작 전에 작업 공간 준비가 끝나지 않았습니다.'
          END,
          updated_at = now()
        WHERE status IN ('provisioning', 'merging')
      `,
    );
    for (const failure of cleanupFailures) {
      await this.pool.query(
        `
          UPDATE task_workspaces
          SET
            error_message = $2,
            updated_at = now()
          WHERE id = $1
            AND status = 'failed'
        `,
        [
          failure.workspaceID,
          `중단된 작업 공간 자동 정리에 실패했습니다. ${failure.message}`,
        ],
      );
    }
    if (this.workspaceManager) {
      const mergedWorkspaces = await this.pool.query(
        `
          SELECT *
          FROM task_workspaces
          WHERE status = 'merged'
        `,
      );
      for (const row of mergedWorkspaces.rows ?? []) {
        const workspace = workspaceFromRow(row);
        try {
          await this.workspaceManager.cleanup(workspace);
        } catch (error) {
          await this.pool.query(
            `
              UPDATE task_workspaces
              SET error_message = $2, updated_at = now()
              WHERE id = $1
                AND status = 'merged'
            `,
            [
              workspace.id,
              `통합된 작업 공간 정리에 실패했습니다. ${
                error instanceof Error ? error.message : String(error)
              }`,
            ],
          );
        }
      }
    }
    if (typeof this.workspaceManager?.validateWorkspace === "function") {
      const activeWorkspaces = await this.pool.query(
        `
          SELECT *
          FROM task_workspaces
          WHERE status = 'active'
        `,
      );
      for (const row of activeWorkspaces.rows ?? []) {
        const workspace = workspaceFromRow(row);
        try {
          await this.workspaceManager.validateWorkspace(workspace);
        } catch (error) {
          const message =
            `활성 작업 공간을 복구할 수 없습니다. ${
              error instanceof Error ? error.message : String(error)
            }`;
          await this.withTransaction(async (client) => {
            await client.query(
              `
                UPDATE task_workspaces
                SET status = 'failed', error_message = $2, updated_at = now()
                WHERE id = $1
                  AND status = 'active'
              `,
              [workspace.id, message],
            );
          });
        }
      }
    }
    return result.rowCount;
  }

  async start(options) {
    if (this.draining) {
      throw new AgentDrainingError(
        "백엔드가 안전한 전환을 준비 중이라 새 업무를 받지 않습니다.",
      );
    }

    const characterID = String(options?.characterID ?? "");
    if (this.terminalSessionRegistry?.has(characterID)) {
      return await this.terminalSessionRegistry.dispatch({ ...options, characterID });
    }
    if (
      this.compactingCharacters.has(characterID) ||
      this.preparingCharacters.has(characterID)
    ) {
      throw new AgentBusyError(
        "이 직원의 컨텍스트 압축 또는 업무 준비가 끝난 뒤 시작하세요.",
      );
    }

    const preparation = Symbol(
      `prepare-agent-job:${characterID || "unknown"}`,
    );
    this.preparingJobs.add(preparation);
    this.preparingCharacters.add(characterID);
    try {
      return await this.startAccepted(options);
    } finally {
      this.preparingJobs.delete(preparation);
      this.preparingCharacters.delete(characterID);
    }
  }

  async startAccepted({
    characterID,
    prompt,
    conversationID,
    attachmentPaths = [],
    senderCharacterID = null,
  }) {
    const cleanPrompt = String(prompt ?? "").trim();
    if (!cleanPrompt && attachmentPaths.length === 0) {
      throw new Error("업무 내용을 입력하세요.");
    }
    if (
      this.running.has(characterID) ||
      this.compactingCharacters.has(characterID)
    ) {
      throw new AgentBusyError(
        "이 직원의 현재 업무가 끝난 뒤 새 업무를 시작하세요.",
      );
    }

    let prepared;
    let attachments = [];
    try {
      prepared = await this.prepareTurn({
        characterID,
        prompt: cleanPrompt || "첨부 파일을 확인해줘.",
        conversationID: conversationID || randomUUID(),
        isolateGitWorkdir: false,
        senderCharacterID,
      });
      const recordPrompt = cleanPrompt || "첨부 파일을 확인해줘.";
      attachments = stageAttachments({
        attachmentPaths,
        workdir: this.workdir,
      });
      // 과거 작업 기록은 자동으로 주입하지 않는다. 무관한 기록이 매 턴
      // 섞이면 단순 질문의 답변까지 오염된다. 직원이 과거 정보가 필요할
      // 때 GET /api/work-records나 POST /api/rag/search를 직접 호출한다.
      const effectivePrompt = promptWithAttachments(
        recordPrompt,
        attachments,
      );
      prepared = {
        ...prepared,
        prompt: effectivePrompt,
        recordPrompt: effectivePrompt,
        executionPrompt: effectivePrompt,
        workspace: null,
        workspaceID: null,
        workdir: this.workdir,
      };
      prepared.structuredResultPath = prepareStructuredTurnResult({
        workdir: prepared.workdir,
        characterID: prepared.character.id,
      });
      await this.beginPreparedTurn(prepared.turnID, effectivePrompt);
    } catch (error) {
      discardStructuredTurnResult(prepared?.structuredResultPath);
      removeStagedAttachments(attachments);
      if (prepared?.turnID) {
        try {
          await this.failPreparedTurn(prepared, error);
        } catch {
          // 준비 단계의 원래 실패 원인을 호출자에게 유지한다.
        }
      }
      throw error;
    }

    const resumedCodexSession =
      prepared.character.backend === "codex" &&
      Boolean(prepared.externalSessionID);
    const resumedAntigravitySession =
      prepared.character.backend === "antigravity" &&
      Boolean(prepared.externalSessionID);
    const usageBaseline = resumedCodexSession
      ? latestCodexUsageFromRollout(
          findRolloutPath(prepared.externalSessionID),
        )
      : resumedAntigravitySession
        ? this.antigravityUsageReader(prepared.externalSessionID)
        : null;
    const antigravityReasoningBaseline = resumedAntigravitySession
      ? this.antigravityReasoningBaselineReader(prepared.externalSessionID)
      : null;
    const state = {
      ...prepared,
      process: null,
      attachments,
      cancelRequested: false,
      initialGeneratedImages: new Set(
        listGeneratedImages(
          prepared.externalSessionID,
          generatedImageRoot(prepared.character.backend),
        ),
      ),
      sequence: 0,
      lastActivity: null,
      activityRecords: new Map(),
      activityWritePromise: null,
      fileChangeSnapshots: new Map(),
      rolloutReader: prepared.character.backend === "codex"
        ? this.rolloutReaderFactory(prepared.externalSessionID, true)
        : null,
      rolloutMonitorTimer: null,
      rolloutPollPromise: null,
      reasoningMonitorTimer: null,
      reasoningPollPromise: null,
      emittedReasoning: new Map(),
      hasSeenInitialCodexReasoning: false,
      pendingInitialCodexReasoning: null,
      pendingAgentMessage: null,
      visibleAgentMessages: [],
      streamMessageID: null,
      responseText: "",
      partialText: "",
      lastPartialPersistedAt: 0,
      resumedCodexSession,
      resumedAntigravitySession,
      antigravityUsageReader: this.antigravityUsageReader,
      antigravityReasoningBaseline,
      usageBaseline,
      usage: null,
      warning: null,
      failure: null,
    };
    this.running.set(characterID, state);
    this.broadcast({ type: "feed.changed", turnId: state.turnID });

    void this.execute(state).catch(async (error) => {
      await this.fail(state, error);
    });

    return {
      turnId: state.turnID,
      conversationId: state.conversationID,
      status: "running",
    };
  }

  async ensureWorkspace(prepared) {
    if (prepared.workspace) {
      return prepared.workspace;
    }
    if (!this.workspaceManager) {
      return null;
    }
    const isolationExpected = prepared.isolateGitWorkdir ?? true;
    if (!isolationExpected) {
      return null;
    }

    const workspaceID = prepared.workspaceID ?? randomUUID();
    prepared.workspaceID = workspaceID;
    const provisionInput = {
      workspaceID,
      characterID: prepared.character.id,
    };
    if (
      typeof this.workspaceManager.planProvision !== "function" ||
      typeof this.workspaceManager.provisionPlanned !== "function"
    ) {
      const provisioned = await this.workspaceManager.provision(
        provisionInput,
      );
      if (!provisioned) {
        throw new GitWorkspaceError(
          "invalid-state",
          "Git 저장소 확인 뒤 작업 공간을 준비할 수 없게 됐습니다.",
        );
      }
      return this.persistProvisionedWorkspace(prepared, provisioned);
    }

    const plan = await this.workspaceManager.planProvision(provisionInput);
    if (!plan) {
      throw new GitWorkspaceError(
        "invalid-state",
        "Git 저장소 확인 뒤 작업 공간을 준비할 수 없게 됐습니다.",
      );
    }
    try {
      await this.pool.query(
        `
          WITH inserted_workspace AS (
            INSERT INTO task_workspaces (
              id,
              cli_session_id,
              status,
              repository_root,
              source_workdir,
              worktree_path,
              execution_workdir,
              branch_name,
              base_branch,
              base_commit
            )
            VALUES (
              $1,
              $2,
              'provisioning',
              $3,
              $4,
              $5,
              $6,
              $7,
              $8,
              $9
            )
            RETURNING id
          )
          UPDATE turns AS turn
          SET task_workspace_id = inserted_workspace.id
          FROM inserted_workspace
          WHERE turn.id = $10
        `,
        [
          workspaceID,
          prepared.sessionID,
          plan.repositoryRoot,
          plan.sourceWorkdir,
          plan.worktreePath,
          plan.executionWorkdir,
          plan.branchName,
          plan.baseBranch,
          plan.baseCommit,
          prepared.turnID,
        ],
      );
    } catch (error) {
      if (isTaskWorkspaceExecutionConflict(error)) {
        throw new AgentBusyError(
          "이 직원의 다른 업무가 작업 공간을 준비 중입니다. 잠시 뒤 다시 시도하세요.",
        );
      }
      throw error;
    }

    let provisioned;
    try {
      provisioned = await this.workspaceManager.provisionPlanned(plan);
    } catch (error) {
      await this.pool.query(
        `
          UPDATE task_workspaces
          SET status = 'failed', error_message = $2, updated_at = now()
          WHERE id = $1
            AND status = 'provisioning'
        `,
        [
          workspaceID,
          error instanceof Error ? error.message : String(error),
        ],
      );
      throw error;
    }

    try {
      const result = await this.pool.query(
        `
          UPDATE task_workspaces
          SET
            status = 'active',
            repository_root = $2,
            source_workdir = $3,
            worktree_path = $4,
            execution_workdir = $5,
            branch_name = $6,
            base_branch = $7,
            base_commit = $8,
            error_message = NULL,
            updated_at = now()
          WHERE id = $1
            AND status = 'provisioning'
          RETURNING *
        `,
        [
          workspaceID,
          provisioned.repositoryRoot,
          provisioned.sourceWorkdir,
          provisioned.worktreePath,
          provisioned.executionWorkdir,
          provisioned.branchName,
          provisioned.baseBranch,
          provisioned.baseCommit,
        ],
      );
      if (result.rowCount === 0) {
        throw new Error("작업 공간 준비 상태가 DB에서 변경됐습니다.");
      }
      const workspace = workspaceFromRow(result.rows[0]);
      this.broadcast({
        type: "workspace.changed",
        turnId: prepared.turnID,
        characterId: prepared.character.id,
        status: workspace.status,
      });
      return workspace;
    } catch (error) {
      try {
        await this.workspaceManager.cleanup(provisioned);
      } catch {
        // 실패한 provisioning 기록으로 남겨 재시작 시 다시 확인한다.
      }
      await this.pool.query(
        `
          UPDATE task_workspaces
          SET status = 'failed', error_message = $2, updated_at = now()
          WHERE id = $1
            AND status = 'provisioning'
        `,
        [
          workspaceID,
          error instanceof Error ? error.message : String(error),
        ],
      );
      throw error;
    }
  }

  async persistProvisionedWorkspace(prepared, provisioned) {
    let result;
    try {
      result = await this.pool.query(
        `
          WITH inserted_workspace AS (
            INSERT INTO task_workspaces (
              id,
              cli_session_id,
              status,
              repository_root,
              source_workdir,
              worktree_path,
              execution_workdir,
              branch_name,
              base_branch,
              base_commit
            )
            VALUES (
              $1,
              $2,
              'active',
              $3,
              $4,
              $5,
              $6,
              $7,
              $8,
              $9
            )
            RETURNING *
          ), linked_turn AS (
            UPDATE turns AS turn
            SET task_workspace_id = inserted_workspace.id
            FROM inserted_workspace
            WHERE turn.id = $10
            RETURNING turn.id
          )
          SELECT * FROM inserted_workspace
        `,
        [
          prepared.workspaceID,
          prepared.sessionID,
          provisioned.repositoryRoot,
          provisioned.sourceWorkdir,
          provisioned.worktreePath,
          provisioned.executionWorkdir,
          provisioned.branchName,
          provisioned.baseBranch,
          provisioned.baseCommit,
          prepared.turnID,
        ],
      );
    } catch (error) {
      try {
        await this.workspaceManager.cleanup(provisioned);
      } catch {
        // DB 기록에 실패한 작업 공간은 가능한 범위에서만 정리한다.
      }
      if (isTaskWorkspaceExecutionConflict(error)) {
        throw new AgentBusyError(
          "이 직원의 다른 업무가 작업 공간을 준비 중입니다. 잠시 뒤 다시 시도하세요.",
        );
      }
      throw error;
    }
    const workspace = workspaceFromRow(result.rows[0]);
    this.broadcast({
      type: "workspace.changed",
      turnId: prepared.turnID,
      characterId: prepared.character.id,
      status: workspace.status,
    });
    return workspace;
  }

  async beginPreparedTurn(turnID, prompt) {
    await this.withTransaction(async (client) => {
      await client.query(
        `
          UPDATE turns
          SET
            prompt = $2,
            status = 'running',
            started_at = now(),
            updated_at = now()
          WHERE id = $1
            AND status = 'pending'
        `,
        [turnID, prompt],
      );
      await client.query(
        `
          UPDATE messages
          SET text = $2, received_at = now()
          WHERE turn_id = $1
            AND role = 'user'
        `,
        [turnID, prompt],
      );
    });
  }

  async failPreparedTurn(prepared, error) {
    const message = error instanceof Error ? error.message : String(error);
    await this.withTransaction(async (client) => {
      await client.query(
        `
          UPDATE turns
          SET
            status = 'failed',
            error_message = $2,
            ended_at = now(),
            updated_at = now()
          WHERE id = $1
            AND status = 'pending'
        `,
        [prepared.turnID, message],
      );
      if (!prepared.externalSessionID) {
        await client.query(
          `
            UPDATE cli_sessions
            SET ended_at = COALESCE(ended_at, now())
            WHERE id = $1
          `,
          [prepared.sessionID],
        );
      }
      if (prepared.workspaceID) {
        await client.query(
          `
            UPDATE task_workspaces
            SET
              status = 'failed',
              error_message = $2,
              updated_at = now()
            WHERE id = $1
              AND status IN ('provisioning', 'active')
          `,
          [prepared.workspaceID, message],
        );
      }
    });
    this.broadcast({
      type: "feed.changed",
      turnId: prepared.turnID,
      characterId: prepared.character.id,
    });
  }

  async cancel(characterID) {
    const state = this.running.get(characterID);
    if (!state) {
      throw new AgentJobNotFoundError(
        "실행 중인 업무를 찾을 수 없습니다.",
      );
    }

    state.cancelRequested = true;
    if (state.claudeWorker) {
      // 워커는 중단 result를 받을 때까지 기다리므로, 이 뒤의 사용량
      // 저장에는 Claude Code가 보고한 비용이 들어간다.
      await state.claudeWorker.cancelCurrent();
    } else {
      terminateProcessGroup(state.process);
    }
    const message = "사용자가 업무를 중단했습니다.";
    try {
      await this.completePendingInitialCodexReasoning(state);
      await this.finalizeRunningActivities(state, "failed");
      await this.pool.query(
        `
          UPDATE turns
          SET
            status = 'interrupted',
            needs_input = false,
            error_message = $2,
            ended_at = now(),
            updated_at = now()
          WHERE id = $1
        `,
        [state.turnID, message],
      );
      if (
        state.workspace &&
        !state.externalSessionID
      ) {
        await this.pool.query(
          `
            UPDATE task_workspaces
            SET
              status = 'failed',
              error_message = $2,
              updated_at = now()
            WHERE id = $1
              AND status = 'active'
          `,
          [state.workspace.id, message],
        );
        await this.pool.query(
          `
            UPDATE cli_sessions
            SET ended_at = COALESCE(ended_at, now())
            WHERE id = $1
          `,
          [state.sessionID],
        );
      }
      recoverInterruptedUsage(state);
      await this.persistUsageRecord(this.pool, state);
    } finally {
      discardStructuredTurnResult(state.structuredResultPath);
      state.structuredResultPath = null;
      if (this.running.get(characterID) === state) {
        this.running.delete(characterID);
      }
      this.broadcast({
        type: "feed.changed",
        turnId: state.turnID,
        characterId: characterID,
        costChanged: true,
      });
    }

    return {
      turnId: state.turnID,
      status: "interrupted",
    };
  }

  async prepareTurn({
    characterID,
    prompt,
    conversationID,
    isolateGitWorkdir = false,
    senderCharacterID = null,
  }) {
    return this.withTransaction(async (client) => {
      await client.query(
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [`officestra:character:${characterID}`],
      );
      const characterResult = await client.query(
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
            identity_prompt AS "identityPrompt",
            config
          FROM characters
          WHERE id = $1
          FOR UPDATE
        `,
        [characterID],
      );
      if (characterResult.rowCount === 0) {
        throw new CharacterNotFoundError(
          "캐릭터를 찾을 수 없습니다.",
        );
      }

      const busy = await client.query(
        `
          SELECT turn.id
          FROM turns AS turn
          JOIN cli_sessions AS session
            ON session.id = turn.cli_session_id
          WHERE session.character_id = $1
            AND turn.status IN ('pending', 'running')
          LIMIT 1
        `,
        [characterID],
      );
      if (busy.rowCount > 0) {
        throw new AgentBusyError(
          "이 직원은 이미 업무를 처리하고 있습니다.",
        );
      }

      const activeResult = await client.query(
        `
          SELECT
            session.id,
            session.external_id AS "externalSessionID",
            session.conversation_id AS "conversationID",
            conversation.workdir AS "conversationWorkdir",
            session_scope.repository_root AS "sessionRepositoryRoot",
            workspace.id AS "workspaceID",
            workspace.status AS "workspaceStatus",
            workspace.repository_root AS "workspaceRepositoryRoot",
            workspace.source_workdir AS "workspaceSourceWorkdir",
            workspace.worktree_path AS "workspaceWorktreePath",
            workspace.execution_workdir AS "workspaceExecutionWorkdir",
            workspace.branch_name AS "workspaceBranchName",
            workspace.base_branch AS "workspaceBaseBranch",
            workspace.base_commit AS "workspaceBaseCommit",
            workspace.review_turn_id AS "workspaceReviewTurnID",
            workspace.review_tree AS "workspaceReviewTree",
            workspace.head_commit AS "workspaceHeadCommit",
            workspace.changed_files AS "workspaceChangedFiles",
            workspace.task_commit AS "workspaceTaskCommit",
            workspace.merged_commit AS "workspaceMergedCommit",
            workspace.error_message AS "workspaceErrorMessage",
            workspace.created_at AS "workspaceCreatedAt",
            workspace.updated_at AS "workspaceUpdatedAt",
            workspace.review_requested_at AS "workspaceReviewRequestedAt",
            workspace.merged_at AS "workspaceMergedAt",
            workspace.rejected_at AS "workspaceRejectedAt",
            resume_workspace.execution_workdir AS "resumeExecutionWorkdir"
          FROM active_cli_sessions AS active
          JOIN cli_sessions AS session
            ON session.id = active.cli_session_id
          JOIN conversations AS conversation
            ON conversation.id = session.conversation_id
          LEFT JOIN LATERAL (
            SELECT candidate.repository_root
            FROM task_workspaces AS candidate
            WHERE candidate.cli_session_id = session.id
              AND candidate.repository_root IS NOT NULL
            ORDER BY candidate.updated_at DESC, candidate.id DESC
            LIMIT 1
          ) AS session_scope ON true
          LEFT JOIN LATERAL (
            SELECT candidate.*
            FROM task_workspaces AS candidate
            WHERE candidate.cli_session_id = session.id
              AND candidate.status IN ('provisioning', 'active')
            ORDER BY candidate.updated_at DESC
            LIMIT 1
          ) AS workspace ON true
          LEFT JOIN LATERAL (
            SELECT candidate.execution_workdir
            FROM turns AS completed_turn
            JOIN task_workspaces AS candidate
              ON candidate.id = completed_turn.task_workspace_id
            WHERE completed_turn.cli_session_id = session.id
              AND completed_turn.status = 'completed'
              AND candidate.execution_workdir IS NOT NULL
            ORDER BY
              completed_turn.ended_at DESC NULLS LAST,
              completed_turn.started_at DESC,
              completed_turn.id DESC
            LIMIT 1
          ) AS resume_workspace ON true
          WHERE active.character_id = $1
            AND session.ended_at IS NULL
          LIMIT 1
        `,
        [characterID],
      );

      let sessionID;
      let externalSessionID;
      let effectiveConversationID;
      let workspace = null;
      const activeCandidate = activeResult.rows[0] ?? null;
      const active = activeSessionMatchesRuntime(activeCandidate, {
        workdir: this.workdir,
        repositoryRoot: this.repositoryRoot,
      })
        ? activeCandidate
        : null;
      const reusedSession = Boolean(active);
      if (active) {
        sessionID = active.id;
        externalSessionID = active.externalSessionID;
        effectiveConversationID = active.conversationID;
        // 신규 업무는 언제나 설정된 프로젝트 폴더에서 실행한다. 과거
        // 버전이 남긴 활성 worktree는 복구·검토용 기록으로만 유지한다.
        workspace = isolateGitWorkdir
          ? workspaceFromActiveRow(active)
          : null;
      } else {
        effectiveConversationID = conversationID;
        await client.query(
          `
            INSERT INTO conversations (id, title, workdir)
            VALUES ($1, $2, $3)
            ON CONFLICT (id) DO NOTHING
          `,
          [
            effectiveConversationID,
            prompt.slice(0, 60),
            this.workdir,
          ],
        );

        const previous = await client.query(
          `
            SELECT id
            FROM cli_sessions
            WHERE character_id = $1
            ORDER BY started_at DESC, id DESC
            LIMIT 1
          `,
          [characterID],
        );
        const insertedSession = await client.query(
          `
            INSERT INTO cli_sessions (
              conversation_id,
              character_id,
              previous_session_id
            )
            VALUES ($1, $2, $3)
            RETURNING id
          `,
          [
            effectiveConversationID,
            characterID,
            previous.rows[0]?.id ?? null,
          ],
        );
        sessionID = insertedSession.rows[0].id;
        externalSessionID = null;
      }

      const turnID = randomUUID();
      const character = characterResult.rows[0];
      await resolveLocalCharacter(client, character);
      await validateLocalResume(client, sessionID, character, externalSessionID);
      await client.query(
        `
          INSERT INTO turns (
            id,
            cli_session_id,
            task_workspace_id,
            backend,
            model,
            effort,
            fast_mode,
            prompt,
            sender_character_id,
            status,
            started_at,
            updated_at
          )
          VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', now(), now())
        `,
        [
          turnID,
          sessionID,
          workspace?.id ?? null,
          character.backend,
          character.model,
          character.effort,
          character.fastMode,
          prompt,
          senderCharacterID,
        ],
      );
      await client.query(
        `
          INSERT INTO messages (turn_id, role, text)
          VALUES
            ($1, 'user', $2),
            ($1, 'assistant', '')
        `,
        [turnID, prompt],
      );

      await snapshotLocalTurn(client, turnID, character);

      return {
        turnID,
        sessionID,
        conversationID: effectiveConversationID,
        externalSessionID,
        resumeExecutionWorkdir:
          active?.resumeExecutionWorkdir ?? null,
        character,
        prompt,
        workspace,
        workspaceID: workspace?.id ?? null,
        reusedSession,
        isolateGitWorkdir,
      };
    });
  }

  async execute(state) {
    if (state.character.localProfile) {
      if (!this.localProviders) throw new Error('Local provider service unavailable');
      if (state.externalSessionID) prepareClaudeSessionResume({sessionID:state.externalSessionID,workdir:state.workdir,previousWorkdir:state.resumeExecutionWorkdir});
      const spec = await this.localProviders.launch({character:state.character,mode:'gui',prompt:state.executionPrompt ?? state.prompt,previousSessionID:state.externalSessionID,workdir:state.workdir});
      try { await this.executeSingleProcess(state, spec); }
      finally { await spec.release(); }
      return;
    }
    if (state.character.backend === "claude") {
      await this.executeClaude(state);
      return;
    }
    await this.executeSingleProcess(state);
  }

  async executeSingleProcess(state, localSpec = null) {
    const executable = localSpec?.executable ?? locateExecutable(state.character);
    const cliArguments = localSpec?.args ?? buildArguments({
      character: state.character,
      prompt: state.executionPrompt ?? state.prompt,
      previousSessionID: state.externalSessionID,
      attachments: state.attachments,
      workdir: state.workdir,
    });
    const child = spawn(executable, cliArguments, {
      cwd: state.workdir,
      env: localSpec?.env ?? executionEnvironment(state.character, process.env, {
        workdir: state.workdir,
      }),
      stdio: ["ignore", "pipe", "pipe"],
      shell: false,
      detached: process.platform !== "win32",
    });
    state.process = child;
    if (state.character.backend === "codex") {
      this.startCodexRolloutMonitor(state);
    }
    if (state.character.backend === "antigravity") {
      this.startAntigravityReasoningMonitor(state);
    }

    const outputTask = this.consumeOutput(state, child.stdout);
    const errorTask = collectStream(child.stderr);
    let exitCode;
    try {
      [exitCode] = await once(child, "close");
      await outputTask;
    } finally {
      if (state.character.backend === "codex") {
        await this.stopCodexRolloutMonitor(state);
      }
      if (state.character.backend === "antigravity") {
        await this.stopAntigravityReasoningMonitor(state);
      }
    }
    const stderr = (await errorTask).trim();

    if (await this.settleCancelledOutput(state)) {
      return;
    }
    if (state.failure) {
      throw new Error(state.failure);
    }
    if (exitCode !== 0) {
      throw new Error(
        state.warning ||
        stderr ||
        `CLI가 종료 코드 ${exitCode}로 끝났습니다.`,
      );
    }

    const candidate = this.finalResponseCandidate(state);
    const decoded = this.decodeCompletedResponse(state, candidate);
    if (!decoded.text) {
      throw new Error("CLI 최종 메시지가 없습니다.");
    }
    await this.complete(state, decoded);
    await this.maybeAutoCompactAfterTurn(state);
  }

  async executeClaude(state) {
    if (state.externalSessionID) {
      prepareClaudeSessionResume({
        sessionID: state.externalSessionID,
        workdir: state.workdir,
        previousWorkdir: state.resumeExecutionWorkdir,
      });
    }
    const executable = locateExecutable(state.character);
    const worker = this.acquireClaudeWorker(state, executable);
    state.claudeWorker = worker;
    state.process = worker.child;
    try {
      await worker.runTurn({
        prompt: state.executionPrompt ?? state.prompt,
        onLine: async (line) => await this.consumeOutputLine(state, line),
      });
    } catch (error) {
      if (await this.settleCancelledOutput(state)) {
        return;
      }
      throw error;
    }

    if (await this.settleCancelledOutput(state)) {
      return;
    }
    if (state.failure) {
      throw new Error(state.failure);
    }
    const candidate = this.finalResponseCandidate(state);
    const decoded = this.decodeCompletedResponse(state, candidate);
    if (!decoded.text) {
      throw new Error("CLI 최종 메시지가 없습니다.");
    }
    await this.complete(state, decoded);
    await this.maybeAutoCompactAfterTurn(state);
  }

  acquireClaudeWorker(state, executable) {
    const characterID = state.character.id;
    const signature = claudePersistentWorkerSignature({
      character: state.character,
      executable,
      workdir: state.workdir,
    });
    const existing = this.claudeWorkers.get(characterID);
    if (existing?.matches({
      signature,
      sessionID: state.externalSessionID,
    })) {
      return existing;
    }
    existing?.close(
      new Error("Claude Code 설정 또는 작업 공간이 바뀌어 지속 세션을 교체합니다."),
    );

    let worker;
    worker = this.claudeWorkerFactory({
      executable,
      argumentsList: claudePersistentArguments(
        state.character,
        state.externalSessionID,
      ),
      cwd: state.workdir,
      env: executionEnvironment(state.character, process.env, {
        workdir: state.workdir,
      }),
      signature,
      sessionID: state.externalSessionID,
      onExit: (exitedWorker) => {
        if (this.claudeWorkers.get(characterID) === exitedWorker) {
          this.claudeWorkers.delete(characterID);
        }
      },
    });
    this.claudeWorkers.set(characterID, worker);
    return worker;
  }

  closeClaudeWorker(
    characterID,
    expected = null,
    reason = new Error("Claude Code 지속 세션을 종료했습니다."),
  ) {
    const worker = this.claudeWorkers.get(characterID);
    if (!worker || (expected && worker !== expected)) {
      return false;
    }
    this.claudeWorkers.delete(characterID);
    worker.close(reason);
    return true;
  }

  shutdown() {
    this.closeClaudeWorkers(
      new Error("OFFICESTRA 백엔드가 종료됩니다."),
    );
    this.embeddingService?.close?.();
  }

  closeClaudeWorkers(
    reason = new Error("Claude Code 지속 세션을 종료했습니다."),
  ) {
    for (const [characterID, worker] of this.claudeWorkers) {
      this.closeClaudeWorker(
        characterID,
        worker,
        reason,
      );
    }
  }

  async prepareTerminalLaunch(characterID) {
    const id = String(characterID ?? "").trim();
    if (this.draining) {
      throw new AgentDrainingError(
        "백엔드가 안전한 전환을 준비 중이라 터미널을 열 수 없습니다.",
      );
    }
    if (
      !id ||
      this.running.has(id) ||
      this.preparingCharacters.has(id) ||
      this.compactingCharacters.has(id)
    ) {
      throw new AgentBusyError(
        "이 직원의 현재 업무가 끝난 뒤 터미널을 여세요.",
      );
    }

    const launch = await this.withTransaction(async (client) => {
      await client.query(
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [`officestra:character:${id}`],
      );
      const characterResult = await client.query(
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
            identity_prompt AS "identityPrompt",
            config
          FROM characters
          WHERE id = $1
          FOR UPDATE
        `,
        [id],
      );
      if (characterResult.rowCount === 0) {
        throw new CharacterNotFoundError("캐릭터를 찾을 수 없습니다.");
      }
      const busy = await client.query(
        `
          SELECT turn.id
          FROM turns AS turn
          JOIN cli_sessions AS session
            ON session.id = turn.cli_session_id
          WHERE session.character_id = $1
            AND turn.status IN ('pending', 'running')
          LIMIT 1
        `,
        [id],
      );
      if (busy.rowCount > 0) {
        throw new AgentBusyError("이 직원은 이미 업무를 처리하고 있습니다.");
      }

      const activeResult = await client.query(
        `
          SELECT
            session.id,
            session.external_id AS "externalSessionID",
            session.conversation_id AS "conversationID",
            conversation.workdir AS "conversationWorkdir",
            session_scope.repository_root AS "sessionRepositoryRoot",
            resume_workspace.execution_workdir AS "resumeExecutionWorkdir"
          FROM active_cli_sessions AS active
          JOIN cli_sessions AS session
            ON session.id = active.cli_session_id
          JOIN conversations AS conversation
            ON conversation.id = session.conversation_id
          LEFT JOIN LATERAL (
            SELECT candidate.repository_root
            FROM task_workspaces AS candidate
            WHERE candidate.cli_session_id = session.id
              AND candidate.repository_root IS NOT NULL
            ORDER BY candidate.updated_at DESC, candidate.id DESC
            LIMIT 1
          ) AS session_scope ON true
          LEFT JOIN LATERAL (
            SELECT candidate.execution_workdir
            FROM turns AS completed_turn
            JOIN task_workspaces AS candidate
              ON candidate.id = completed_turn.task_workspace_id
            WHERE completed_turn.cli_session_id = session.id
              AND completed_turn.status = 'completed'
              AND candidate.execution_workdir IS NOT NULL
            ORDER BY completed_turn.ended_at DESC NULLS LAST
            LIMIT 1
          ) AS resume_workspace ON true
          WHERE active.character_id = $1
            AND session.ended_at IS NULL
          LIMIT 1
        `,
        [id],
      );
      const activeCandidate = activeResult.rows[0] ?? null;
      const active = activeSessionMatchesRuntime(activeCandidate, {
        workdir: this.workdir,
        repositoryRoot: this.repositoryRoot,
      }) ? activeCandidate : null;

      let sessionID = active?.id ?? null;
      let conversationID = active?.conversationID ?? null;
      if (!sessionID) {
        conversationID = randomUUID();
        await client.query(
          `
            INSERT INTO conversations (id, title, workdir)
            VALUES ($1, '터미널 대화', $2)
          `,
          [conversationID, this.workdir],
        );
        const previous = await client.query(
          `
            SELECT id
            FROM cli_sessions
            WHERE character_id = $1
            ORDER BY started_at DESC, id DESC
            LIMIT 1
          `,
          [id],
        );
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
          [conversationID, id, previous.rows[0]?.id ?? null],
        );
        sessionID = inserted.rows[0].id;
        await client.query(
          `
            INSERT INTO active_cli_sessions (character_id, cli_session_id)
            VALUES ($1, $2)
            ON CONFLICT (character_id) DO UPDATE
            SET cli_session_id = EXCLUDED.cli_session_id, updated_at = now()
          `,
          [id, sessionID],
        );
      }
      const terminalCharacter = await resolveLocalCharacter(client, characterResult.rows[0]);
      await validateLocalResume(client, sessionID, terminalCharacter, active?.externalSessionID);
      return {
        character: terminalCharacter,
        sessionID,
        conversationID,
        externalSessionID: active?.externalSessionID ?? null,
        resumeExecutionWorkdir: active?.resumeExecutionWorkdir ?? null,
        workdir: this.workdir,
      };
    });

    if (launch.character.backend === "claude") {
      this.closeClaudeWorker(
        id,
        null,
        new Error("터미널 모드가 같은 Claude Code 세션을 이어갑니다."),
      );
      if (launch.externalSessionID) {
        prepareClaudeSessionResume({
          sessionID: launch.externalSessionID,
          workdir: launch.workdir,
          previousWorkdir: launch.resumeExecutionWorkdir,
        });
      }
    }
    return launch;
  }

  async bindTerminalExternalSession({
    characterID,
    sessionID,
    externalSessionID,
  }) {
    const externalID = String(externalSessionID ?? "").trim();
    if (!externalID) throw new Error("CLI 세션 ID가 비어 있습니다.");
    await this.withTransaction(async (client) => {
      await client.query(
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [`officestra:character:${characterID}`],
      );
      const updated = await client.query(
        `
          UPDATE cli_sessions
          SET external_id = $3, ended_at = NULL
          WHERE id = $1
            AND character_id = $2
            AND (external_id IS NULL OR external_id = $3)
          RETURNING id
        `,
        [sessionID, characterID, externalID],
      );
      if (updated.rowCount === 0) {
        throw new AgentSessionNotFoundError(
          "터미널의 CLI 세션을 현재 직원에게 연결할 수 없습니다.",
        );
      }
      await client.query(
        `
          UPDATE cli_sessions
          SET ended_at = COALESCE(ended_at, now())
          WHERE character_id = $1 AND id <> $2 AND ended_at IS NULL
        `,
        [characterID, sessionID],
      );
      await client.query(
        `
          INSERT INTO active_cli_sessions (character_id, cli_session_id)
          VALUES ($1, $2)
          ON CONFLICT (character_id) DO UPDATE
          SET cli_session_id = EXCLUDED.cli_session_id, updated_at = now()
        `,
        [characterID, sessionID],
      );
    });
    this.broadcast({ type: "session.changed", characterId: characterID });
  }

  async beginTerminalTurn({
    characterID,
    sessionID,
    prompt,
    startedAt = new Date(),
    execution = null,
    senderCharacterID = null,
  }) {
    const cleanPrompt = String(prompt ?? "").trim();
    if (!cleanPrompt) throw new Error("터미널 프롬프트가 비어 있습니다.");
    const turnID = randomUUID();
    const result = await this.withTransaction(async (client) => {
      const session = await client.query(
        `
          SELECT
            session.id,
            character.backend,
            character.model,
            character.effort,
            character.fast_mode AS "fastMode"
          FROM cli_sessions AS session
          JOIN characters AS character ON character.id = session.character_id
          WHERE session.id = $1 AND session.character_id = $2
          FOR UPDATE OF session, character
        `,
        [sessionID, characterID],
      );
      if (session.rowCount === 0) {
        throw new AgentSessionNotFoundError("터미널 세션을 찾을 수 없습니다.");
      }
      const busy = await client.query(
        `
          SELECT turn.id
          FROM turns AS turn
          JOIN cli_sessions AS owner ON owner.id = turn.cli_session_id
          WHERE owner.character_id = $1
            AND turn.status IN ('pending', 'running')
          LIMIT 1
        `,
        [characterID],
      );
      if (busy.rowCount > 0) {
        throw new AgentBusyError("터미널의 이전 응답이 아직 끝나지 않았습니다.");
      }
      const settings = execution ?? session.rows[0];
      await client.query(
        `
          INSERT INTO turns (
            id, cli_session_id, backend, model, effort, fast_mode,
            origin, prompt, status, started_at, updated_at, sender_character_id
          )
          VALUES ($1, $2, $3, $4, $5, $6, 'terminal', $7, 'running', $8, now(), $9)
        `,
        [
          turnID,
          sessionID,
          settings.backend,
          settings.model,
          settings.effort,
          settings.fastMode,
          cleanPrompt,
          startedAt,
          senderCharacterID,
        ],
      );
      await client.query(
        `
          INSERT INTO messages (turn_id, role, text)
          VALUES ($1, 'user', $2), ($1, 'assistant', '')
        `,
        [turnID, cleanPrompt],
      );
      await snapshotLocalTurn(client, turnID, settings);
      return settings;
    });
    this.broadcast({ type: "feed.changed", turnId: turnID, characterId: characterID });
    return { turnID, prompt: cleanPrompt, settings: result };
  }

  async completeTerminalTurn({
    characterID,
    turnID,
    response,
    endedAt = new Date(),
    usage = null,
    initialGeneratedImages = new Set(),
    structured = null,
    reportedCostUsd = null,
    activities = [],
    refreshCompleted = false,
  }) {
    const result = await this.pool.query(
      `
        SELECT
          turn.id,
          turn.status,
          turn.cli_session_id AS "sessionID",
          turn.prompt,
          turn.backend AS "executionBackend",
          turn.model AS "executionModel",
          turn.effort AS "executionEffort",
          turn.fast_mode AS "executionFastMode",
          turn.provider_kind AS "providerKind",
          turn.provider_snapshot AS "providerSnapshot",
          turn.started_at AS "startedAt",
          session.conversation_id AS "conversationID",
          session.external_id AS "externalSessionID",
          character.id AS "characterID",
          character.name,
          character.seat,
          character.backend,
          character.model,
          character.effort,
          character.fast_mode AS "fastMode",
          character.auto_compact_percent AS "autoCompactPercent",
          character.permission,
          character.identity_prompt AS "identityPrompt",
          character.config
        FROM turns AS turn
        JOIN cli_sessions AS session ON session.id = turn.cli_session_id
        JOIN characters AS character ON character.id = session.character_id
        WHERE turn.id = $1
          AND session.character_id = $2
          AND turn.origin = 'terminal'
          AND (
            turn.status = 'running'
            OR ($3::boolean AND turn.backend = 'antigravity' AND turn.status = 'completed')
          )
      `,
      [turnID, characterID, refreshCompleted === true],
    );
    if (result.rowCount === 0) {
      throw new AgentJobNotFoundError("완료할 터미널 턴을 찾을 수 없습니다.");
    }
    const row = result.rows[0];
    if (refreshCompleted && row.executionBackend !== "antigravity") {
      throw new AgentJobNotFoundError("Antigravity 터미널 후속 응답만 갱신할 수 있습니다.");
    }
    const decoded = applyStructuredTurnResult(
      decodeAgentResponse(String(response ?? "")),
      structured,
    );
    if (!decoded.text) throw new Error("터미널 최종 메시지가 없습니다.");
    const state = {
      turnID,
      sessionID: row.sessionID,
      conversationID: row.conversationID,
      externalSessionID: row.externalSessionID,
      character: {
        id: row.characterID,
        name: row.name,
        seat: row.seat,
        backend: row.executionBackend,
        model: row.executionModel,
        effort: row.executionEffort,
        fastMode: row.executionFastMode,
        autoCompactPercent: row.autoCompactPercent,
        permission: row.permission,
        identityPrompt: row.identityPrompt,
        config: row.config,
        localProfile: row.providerKind === 'local' ? row.providerSnapshot : null,
      },
      prompt: row.prompt,
      recordPrompt: row.prompt,
      workdir: this.workdir,
      workspace: null,
      workspaceID: null,
      initialGeneratedImages,
      generatedImages: [],
      sequence: 0,
      lastActivity: null,
      activityRecords: new Map(),
      activityWritePromise: null,
      visibleAgentMessages: [],
      pendingInitialCodexReasoning: null,
      responseText: decoded.text,
      partialText: decoded.text,
      usage,
      endedAt,
      cancelRequested: false,
      structuredResultPath: null,
      refreshTerminalResult: refreshCompleted === true,
    };
    if (refreshCompleted) {
      const previous = await this.pool.query(
        `SELECT seq, kind, text, event_key AS "eventKey", status, collaboration
         FROM turn_activities WHERE turn_id = $1 ORDER BY seq`,
        [turnID],
      );
      for (const entry of previous.rows) {
        state.sequence = Math.max(state.sequence, Number(entry.seq));
        if (entry.eventKey) state.activityRecords.set(entry.eventKey, {
          sequence: Number(entry.seq), kind: entry.kind, text: entry.text,
          status: entry.status, collaboration: entry.collaboration ?? null,
        });
      }
    }
    state.usage ??= await this.terminalTurnUsage(state, row.startedAt);
    if (reportedCostUsd !== null) {
      state.usage = {
        ...(state.usage ?? emptyUsage()),
        reportedCostUsd,
      };
    }
    // 터미널에서도 GUI와 같은 활동 테이블/이벤트 경로를 사용한다.
    // 최종 답변은 별도로 보존하며 활동을 본문에 다시 이어 붙이지 않는다.
    for (const activity of activities) {
      await this.addActivity(state, activity);
    }
    await this.complete(state, decoded);
  }

  // Claude 상태줄 누적 비용의 마지막 갱신은 Stop 훅보다 늦게 도착한다. 이미
  // 완료한 터미널 턴의 비용에 그 차액을 더한다.
  async addTerminalTurnReportedCost({ characterID, turnID, costUsd }) {
    const amount = Number(costUsd);
    if (!turnID || !Number.isFinite(amount) || amount <= 0) return false;
    const result = await this.pool.query(
      `
        INSERT INTO usage_records (turn_id, cost_usd)
        SELECT turn.id, $3
        FROM turns AS turn
        JOIN cli_sessions AS session ON session.id = turn.cli_session_id
        WHERE turn.id = $1
          AND session.character_id = $2
          AND turn.origin = 'terminal'
          AND turn.backend = 'claude'
        ON CONFLICT (turn_id) DO UPDATE
        SET cost_usd = COALESCE(usage_records.cost_usd, 0) + EXCLUDED.cost_usd
        RETURNING turn_id
      `,
      [turnID, characterID, amount],
    );
    if (result.rowCount === 0) return false;
    this.broadcast({
      type: "feed.changed",
      turnId: turnID,
      characterId: characterID,
      costChanged: true,
    });
    return true;
  }

  // Claude와 Codex 터미널 턴은 CLI가 토큰 사용량을 알려주지 않는다. 두 CLI가
  // 디스크에 남긴 기록에서 이 턴 구간만 합산해 GUI 턴과 같은 값을 채운다.
  // Claude 비용만은 기록에 없고 상태줄이 알려준 값을 호출자가 따로 넣는다.
  async terminalTurnUsage(state, startedAt) {
    const backend = state.character?.backend;
    if (!state.externalSessionID || !startedAt) {
      return null;
    }
    const window = { startedAt, endedAt: state.endedAt };
    try {
      if (backend === "codex") {
        return await codexRolloutTurnUsage(
          findRolloutPath(state.externalSessionID),
          window,
        );
      }
      if (backend === "claude") {
        const current = claudeSessionPath(
          state.workdir,
          state.externalSessionID,
        );
        return await claudeTranscriptTurnUsage(
          claudeSessionFileMatches(current, state.externalSessionID)
            ? current
            : findClaudeSessionPath(state.externalSessionID),
          window,
        );
      }
    } catch (error) {
      console.warn(
        "터미널 턴 사용량을 읽지 못했습니다.",
        error instanceof Error ? error.message : String(error),
      );
    }
    return null;
  }

  async interruptTerminalTurn(characterID, turnID) {
    if (!turnID) return false;
    const result = await this.pool.query(
      `
        UPDATE turns AS turn
        SET
          status = 'interrupted',
          needs_input = false,
          error_message = '터미널 프로세스가 응답 완료 전에 종료됐습니다.',
          ended_at = now(),
          updated_at = now()
        FROM cli_sessions AS session
        WHERE turn.id = $1
          AND turn.cli_session_id = session.id
          AND session.character_id = $2
          AND turn.origin = 'terminal'
          AND turn.status = 'running'
        RETURNING turn.id
      `,
      [turnID, characterID],
    );
    if (result.rowCount > 0) {
      this.broadcast({ type: "feed.changed", turnId: turnID, characterId: characterID, costChanged: true });
      return true;
    }
    return false;
  }

  async compactionTarget(characterID) {
    const result = await this.pool.query(
      `
        SELECT
          character.id,
          character.name,
          character.seat,
          character.backend,
          character.model,
          character.effort,
          character.fast_mode AS "fastMode",
          character.auto_compact_percent AS "autoCompactPercent",
          character.permission,
          character.identity_prompt AS "identityPrompt",
          character.config,
          session.external_id AS "externalSessionID",
          conversation.workdir AS "conversationWorkdir",
          session_scope.repository_root AS "sessionRepositoryRoot",
          resume_workspace.execution_workdir AS "resumeExecutionWorkdir"
        FROM characters AS character
        LEFT JOIN active_cli_sessions AS active
          ON active.character_id = character.id
        LEFT JOIN cli_sessions AS session
          ON session.id = active.cli_session_id
          AND session.ended_at IS NULL
        LEFT JOIN conversations AS conversation
          ON conversation.id = session.conversation_id
        LEFT JOIN LATERAL (
          SELECT candidate.repository_root
          FROM task_workspaces AS candidate
          WHERE candidate.cli_session_id = session.id
            AND candidate.repository_root IS NOT NULL
          ORDER BY candidate.updated_at DESC, candidate.id DESC
          LIMIT 1
        ) AS session_scope ON true
        LEFT JOIN LATERAL (
          SELECT candidate.execution_workdir
          FROM turns AS completed_turn
          JOIN task_workspaces AS candidate
            ON candidate.id = completed_turn.task_workspace_id
          WHERE completed_turn.cli_session_id = session.id
            AND completed_turn.status = 'completed'
            AND candidate.execution_workdir IS NOT NULL
          ORDER BY
            completed_turn.ended_at DESC NULLS LAST,
            completed_turn.started_at DESC,
            completed_turn.id DESC
          LIMIT 1
        ) AS resume_workspace ON true
        WHERE character.id = $1
        LIMIT 1
      `,
      [characterID],
    );
    if (result.rowCount === 0) {
      throw new CharacterNotFoundError("캐릭터를 찾을 수 없습니다.");
    }
    const row = result.rows[0];
    if (
      !row.externalSessionID ||
      !activeSessionMatchesRuntime(row, {
        workdir: this.workdir,
        repositoryRoot: this.repositoryRoot,
      })
    ) {
      throw new AgentSessionNotFoundError(
        "압축할 활성 CLI 세션이 없습니다.",
      );
    }
    const previousWorkdir = String(row.resumeExecutionWorkdir ?? "").trim();
    const workdir = previousWorkdir && existsSync(previousWorkdir)
      ? previousWorkdir
      : this.workdir;
    return {
      character: {
        id: row.id,
        name: row.name,
        seat: row.seat,
        backend: row.backend,
        model: row.model,
        effort: row.effort,
        fastMode: row.fastMode,
        autoCompactPercent: normalizeAutoCompactPercent(
          row.autoCompactPercent,
        ),
        permission: row.permission,
        identityPrompt: row.identityPrompt,
        config: row.config ?? {},
      },
      externalSessionID: row.externalSessionID,
      resumeExecutionWorkdir: previousWorkdir || null,
      workdir,
    };
  }

  contextUsage(target, at = Date.now()) {
    return this.contextUsageReader({
      backend: target.character.backend,
      sessionID: target.externalSessionID,
      model: target.character.model,
      at,
    });
  }

  async compactContext(characterID, {
    automatic = false,
    expectedSessionID = null,
  } = {}) {
    if (this.draining) {
      throw new AgentDrainingError(
        "백엔드가 안전한 전환을 준비 중이라 컨텍스트를 압축하지 않습니다.",
      );
    }
    if (
      this.running.has(characterID) ||
      this.preparingCharacters.has(characterID) ||
      this.compactingCharacters.has(characterID)
    ) {
      throw new AgentBusyError(
        "이 직원의 현재 업무와 후속 처리가 끝난 뒤 압축하세요.",
      );
    }

    this.compactingCharacters.add(characterID);
    this.broadcast({
      type: "context.compaction.started",
      characterId: characterID,
      automatic,
    });
    try {
      const target = await this.compactionTarget(characterID);
      if (target.character.config?.localProfileId) {
        throw new Error('로컬 AI는 CLI 기본 자동 압축을 사용합니다. 수동 압축은 터미널에서 /compact를 사용하세요.');
      }
      if (
        expectedSessionID &&
        target.externalSessionID !== expectedSessionID
      ) {
        throw new AgentBusyError(
          "압축 기준을 확인한 뒤 활성 세션이 바뀌었습니다.",
        );
      }
      const before = this.contextUsage(target);
      let nativeResult = {};
      if (target.character.backend === "claude") {
        prepareClaudeSessionResume({
          sessionID: target.externalSessionID,
          workdir: target.workdir,
          previousWorkdir: target.resumeExecutionWorkdir,
        });
        const executable = locateExecutable(target.character);
        const worker = this.acquireClaudeWorker(target, executable);
        nativeResult = await worker.compact();
      } else if (target.character.backend === "codex") {
        nativeResult = await this.codexCompactor({
          executable: locateExecutable(target.character),
          threadID: target.externalSessionID,
          cwd: target.workdir,
          env: executionEnvironment(target.character),
        });
      } else {
        throw new Error(
          "Antigravity CLI는 수동 컨텍스트 압축을 지원하지 않습니다.",
        );
      }
      const after = this.contextUsage(target);
      const measuredPostTokens = after?.usedTokens != null &&
          (
            before?.usedTokens == null ||
            after.usedTokens < before.usedTokens
          )
        ? after.usedTokens
        : null;
      const payload = {
        ok: true,
        automatic,
        backend: target.character.backend,
        sessionId: target.externalSessionID,
        preTokens: nativeResult?.preTokens ?? before?.usedTokens ?? null,
        postTokens: nativeResult?.postTokens ?? measuredPostTokens,
        limitTokens: after?.limitTokens ?? before?.limitTokens ?? null,
      };
      this.compactingCharacters.delete(characterID);
      this.broadcast({
        type: "context.compacted",
        characterId: characterID,
        automatic,
        preTokens: payload.preTokens,
        postTokens: payload.postTokens,
        limitTokens: payload.limitTokens,
      });
      return payload;
    } catch (error) {
      this.compactingCharacters.delete(characterID);
      this.broadcast({
        type: "context.compaction.failed",
        characterId: characterID,
        automatic,
        errorMessage: error instanceof Error
          ? error.message
          : String(error),
      });
      throw error;
    }
  }

  async maybeAutoCompactAfterTurn(state) {
    if (state.character.localProfile) return null;
    // Codex는 CLI 자체의 기본 컨텍스트 창과 네이티브 자동 압축을 사용한다.
    // 직원별 퍼센트 기준은 Claude에만 적용한다.
    if (state.character.backend !== "claude") {
      return null;
    }
    const sessionID = String(state.externalSessionID ?? "").trim();
    if (!sessionID || this.draining) {
      return null;
    }
    const target = {
      character: state.character,
      externalSessionID: sessionID,
    };
    const usage = this.contextUsage(target);
    if (!usage?.limitTokens) {
      return null;
    }
    const threshold = normalizeAutoCompactPercent(
      state.character.autoCompactPercent,
    );
    if ((usage.usedTokens * 100) < (usage.limitTokens * threshold)) {
      return null;
    }
    try {
      return await this.compactContext(state.character.id, {
        automatic: true,
        expectedSessionID: sessionID,
      });
    } catch (error) {
      console.warn(
        `${state.character.name} 자동 컨텍스트 압축에 실패했습니다.`,
        error instanceof Error ? error.message : String(error),
      );
      return null;
    }
  }

  startCodexRolloutMonitor(state) {
    if (
      state.character.backend !== "codex" ||
      state.rolloutMonitorTimer
    ) {
      return;
    }
    state.rolloutMonitorTimer = setInterval(() => {
      void this.consumeCodexRolloutActivities(state).catch((error) => {
        console.warn(
          "Codex 협업 기록을 읽지 못했습니다.",
          error instanceof Error ? error.message : String(error),
        );
      });
    }, CODEX_ROLLOUT_MONITOR_INTERVAL_MS);
    state.rolloutMonitorTimer.unref?.();
  }

  async stopCodexRolloutMonitor(state) {
    if (state.rolloutMonitorTimer) {
      clearInterval(state.rolloutMonitorTimer);
      state.rolloutMonitorTimer = null;
    }
    if (state.rolloutPollPromise) {
      try {
        await state.rolloutPollPromise;
      } catch {
        // 마지막 직접 읽기에서 한 번 더 복구를 시도한다.
      }
    }
    for (let attempt = 0; attempt < 2; attempt += 1) {
      try {
        await this.consumeCodexRolloutActivities(state);
        break;
      } catch (error) {
        if (attempt === 1) {
          console.warn(
            "Codex 협업 기록의 마지막 갱신을 읽지 못했습니다.",
            error instanceof Error ? error.message : String(error),
          );
        }
      }
    }
  }

  // agy는 추론을 stdout으로 내보내지 않지만 대화 SQLite에는 진행 중에도 조금씩
  // 쌓는다. Codex 기록과 같은 주기로 읽어 추론 카드를 자라게 한다.
  startAntigravityReasoningMonitor(state) {
    if (
      state.character.backend !== "antigravity" ||
      state.reasoningMonitorTimer
    ) {
      return;
    }
    state.reasoningMonitorTimer = setInterval(() => {
      void this.consumeAntigravityReasoning(state).catch((error) => {
        console.warn(
          "Antigravity 추론 기록을 읽지 못했습니다.",
          error instanceof Error ? error.message : String(error),
        );
      });
    }, ANTIGRAVITY_REASONING_MONITOR_INTERVAL_MS);
    state.reasoningMonitorTimer.unref?.();
  }

  async stopAntigravityReasoningMonitor(state) {
    if (state.reasoningMonitorTimer) {
      clearInterval(state.reasoningMonitorTimer);
      state.reasoningMonitorTimer = null;
    }
    if (state.reasoningPollPromise) {
      try {
        await state.reasoningPollPromise;
      } catch {
        // 마지막 직접 읽기에서 한 번 더 복구를 시도한다.
      }
    }
    try {
      // 진행 중으로 남은 카드가 없도록 마지막 상태를 확정한다.
      await this.consumeAntigravityReasoning(state, { final: true });
    } catch (error) {
      console.warn(
        "Antigravity 추론 기록의 마지막 갱신을 읽지 못했습니다.",
        error instanceof Error ? error.message : String(error),
      );
    }
  }

  async consumeAntigravityReasoning(state, { final = false } = {}) {
    if (
      state.character.backend !== "antigravity" ||
      !state.externalSessionID
    ) {
      return;
    }
    if (state.reasoningPollPromise) {
      return state.reasoningPollPromise;
    }
    const conversationID = state.externalSessionID;
    const minStepIndex = Number.isInteger(state.antigravityReasoningBaseline)
      ? state.antigravityReasoningBaseline + 1
      : 0;
    const poll = (async () => {
      const reasonings = this.antigravityReasoningReader(conversationID, {
        minStepIndex,
      });
      const emitted = state.emittedReasoning ??= new Map();
      const activities = [];
      for (const { stepIndex, text, done } of reasonings) {
        const status = done || final ? "completed" : "running";
        const previous = emitted.get(stepIndex);
        if (previous?.text === text && previous?.status === status) {
          continue;
        }
        emitted.set(stepIndex, { text, status });
        activities.push({
          kind: "thinking",
          text,
          eventKey: `antigravity:${conversationID}:${stepIndex}:thinking`,
          status,
          preserveText: false,
          messageScoped: false,
        });
      }
      if (activities.length === 0) {
        return;
      }
      await this.promotePendingAgentMessage(state);
      for (const activity of activities) {
        await this.addParsedActivity(state, this.scopedActivity(state, activity));
      }
    })();
    state.reasoningPollPromise = poll;
    try {
      await poll;
    } finally {
      if (state.reasoningPollPromise === poll) {
        state.reasoningPollPromise = null;
      }
    }
  }

  async consumeCodexRolloutActivities(state) {
    if (state.character.backend !== "codex") {
      return;
    }
    if (!state.rolloutReader && state.externalSessionID) {
      state.rolloutReader = this.rolloutReaderFactory(
        state.externalSessionID,
        state.resumedCodexSession === true,
      );
    }
    if (!state.rolloutReader) {
      return;
    }
    if (state.rolloutPollPromise) {
      return state.rolloutPollPromise;
    }
    const poll = (async () => {
      const reader = state.rolloutReader;
      const activities = rolloutCollaborationActivities(
        reader,
        state.workdir,
      );
      if (activities.length === 0) {
        return;
      }
      await this.promotePendingAgentMessage(state);
      for (const activity of activities) {
        await this.addParsedActivity(state, activity);
        acknowledgeRolloutCollaborationActivity(
          reader,
          activity,
        );
      }
    })();
    state.rolloutPollPromise = poll;
    try {
      await poll;
    } finally {
      if (state.rolloutPollPromise === poll) {
        state.rolloutPollPromise = null;
      }
    }
  }

  async consumeOutput(state, stream) {
    const lines = createInterface({
      input: stream,
      crlfDelay: Infinity,
    });
    for await (const line of lines) {
      await this.consumeOutputLine(state, line);
    }
    await this.completePendingInitialCodexReasoning(state);
  }

  async consumeOutputLine(state, line) {
    const event = parseAgentEvent(
      line,
      state.character.backend,
      state.workdir,
    );
    if (!event) {
      return;
    }
    if (event.usage) {
      const usage = usageForTurn(state, event.usage);
      if (usage) {
        state.usage = event.usageIsDelta
          ? accumulatedUsage(state.usage, usage)
          : usage;
      }
    }
    if (event.usageFallback && !state.usage) {
      state.usage = usageForTurn(state, event.usageFallback);
    }
    this.enrichFileChangeEvent(state, event);
    if (
      event.sessionID &&
      event.sessionID !== state.externalSessionID
    ) {
      await this.activateSession(state, event.sessionID);
    }
    if (event.streamMessageID) {
      state.streamMessageID = event.streamMessageID;
      state.partialText = "";
      state.responseText = "";
      state.lastPartialPersistedAt = 0;
    }
    const activities = [
      ...(Array.isArray(event.activities) ? event.activities : []),
      ...(event.activity ? [event.activity] : []),
    ];
    const pendingReasoningBeforeEvent = activities.length > 0
      ? state.pendingInitialCodexReasoning
      : null;
    if (activities.length > 0) {
      await this.promotePendingAgentMessage(state);
      for (const activity of activities) {
        await this.addParsedActivity(
          state,
          this.scopedActivity(state, activity),
        );
      }
      if (pendingReasoningBeforeEvent) {
        await this.completePendingInitialCodexReasoning(
          state,
          pendingReasoningBeforeEvent,
        );
      }
    }
    if (event.agentMessage) {
      await this.recordAgentMessage(
        state,
        event.agentMessageKey ?? null,
        event.agentMessage,
      );
    }
    // Antigravity는 응답 단계마다 조각을 보낸다. 새 단계가 시작되면 앞
    // 단계의 조각을 비워 중간 메시지가 하나로 이어 붙지 않게 한다.
    // 조각 없이 끝나는 빈 단계도 앞 문장을 다시 기록하지 않아야 한다.
    if (
      event.responseStepKey &&
      event.responseStepKey !== state.responseStepKey
    ) {
      state.responseStepKey = event.responseStepKey;
      state.partialText = "";
    }
    if (event.responseDelta) {
      state.partialText += event.responseDelta;
      await this.persistPartialResponse(state);
    }
    if (event.responseStepDone && state.partialText.trim()) {
      // 단계가 끝난 중간 메시지는 Claude의 공개 메시지와 같은 길을 탄다.
      // 다음 활동이 오면 그 앞자리에 message 활동으로 남는다. 끝의 줄바꿈을
      // 떼어야 화면이 응답 초안에서 이미 보여 준 문장을 걷어낼 수 있다.
      await this.recordAgentMessage(
        state,
        event.responseStepKey ?? null,
        state.partialText.trim(),
      );
    }
    const responseText = event.responseText
      ?? (state.visibleAgentMessages?.length ? null : event.responseFallback);
    if (responseText) {
      this.rememberVisibleAgentMessage(
        state,
        null,
        responseText,
      );
      state.responseText = responseText;
      await this.persistResponseDraft(
        state,
        this.visibleResponseText(state),
      );
    }
    if (event.warning) {
      state.warning = event.warning;
    }
    if (event.failure) {
      state.failure = event.failure;
    }
  }

  scopedActivity(state, activity) {
    if (!activity.messageScoped || !state.streamMessageID) {
      return activity;
    }
    return {
      ...activity,
      eventKey: `${state.streamMessageID}:${activity.eventKey}`,
      messageScoped: false,
    };
  }

  enrichFileChangeEvent(state, event) {
    const fileChange = event.fileChange;
    if (!fileChange?.eventKey) {
      return;
    }
    state.fileChangeSnapshots ??= new Map();
    if (fileChange.phase === "item.started") {
      state.fileChangeSnapshots.set(
        fileChange.eventKey,
        captureFileSnapshots(state.workdir, fileChange.changes),
      );
      return;
    }
    if (fileChange.phase !== "item.completed") {
      return;
    }

    const snapshots = state.fileChangeSnapshots.get(fileChange.eventKey);
    state.fileChangeSnapshots.delete(fileChange.eventKey);
    const statistics = rolloutFileChangeStatistics(
      state.rolloutReader,
      state.workdir,
      fileChange.changes,
    ) ?? fileChangeStatistics(
      state.workdir,
      snapshots,
      fileChange.changes,
    );
    if (statistics && event.activity) {
      event.activity.text = fileChangeActivityText(
        fileChange.changes,
        statistics,
        state.workdir,
      );
    }
  }

  async addParsedActivity(state, activity) {
    const isInitialCodexReasoning =
      activity.isCodexReasoning === true &&
      state.hasSeenInitialCodexReasoning !== true;
    if (!isInitialCodexReasoning) {
      await this.addActivity(state, activity);
      return;
    }

    state.hasSeenInitialCodexReasoning = true;
    if (activity.status !== "completed" || !activity.eventKey) {
      await this.addActivity(state, activity);
      return;
    }

    await this.addActivity(state, {
      ...activity,
      status: "running",
    });
    state.pendingInitialCodexReasoning = {
      ...activity,
      status: "completed",
      preserveOccurredAt: true,
    };
  }

  async completePendingInitialCodexReasoning(state, expected = null) {
    const pending = state.pendingInitialCodexReasoning;
    if (
      !pending ||
      (expected && pending.eventKey !== expected.eventKey)
    ) {
      return;
    }

    state.pendingInitialCodexReasoning = null;
    try {
      await this.addActivity(state, pending);
    } catch (error) {
      state.pendingInitialCodexReasoning ??= pending;
      throw error;
    }
  }

  rememberVisibleAgentMessage(state, key, text) {
    const value = String(text ?? "").trim();
    if (!value) {
      return;
    }
    const messages = state.visibleAgentMessages ?? [];
    state.visibleAgentMessages = messages;
    if (key) {
      const existingIndex = messages.findIndex(
        (message) => message.key === key,
      );
      if (existingIndex >= 0) {
        messages[existingIndex] = { key, text: value };
        return;
      }
      messages.push({ key, text: value });
      return;
    }
    if (messages.at(-1)?.text !== value) {
      messages.push({ key, text: value });
    }
  }

  visibleResponseText(state, currentText = "") {
    const values = (state.visibleAgentMessages ?? [])
      .map((message) => message.text)
      .filter(Boolean);
    const current = String(currentText ?? "");
    if (current.trim() && values.at(-1) !== current.trim()) {
      values.push(current);
    }
    return values.join("\n\n");
  }

  finalResponseCandidate(state) {
    const candidates = [
      state.responseText,
      state.partialText,
      state.visibleAgentMessages?.at(-1)?.text,
    ];
    return candidates.find(
      (value) => String(value ?? "").trim().length > 0,
    ) ?? "";
  }

  decodeCompletedResponse(state, candidate) {
    const decoded = decodeAgentResponse(candidate);
    const structured = consumeStructuredTurnResult(
      state.structuredResultPath,
    );
    state.structuredResultPath = null;
    return applyStructuredTurnResult(decoded, structured);
  }

  completedResponseText(state, decoded) {
    // 한 턴이 여러 조각으로 나뉘면 앞 조각의 기계 블록(OFFICE_SOURCES 등)이
    // 이어붙인 본문 한가운데로 들어가 화면에 날것으로 노출된다.
    // 조각마다 기계 블록을 떼어낸 뒤 합친다.
    const values = (state.visibleAgentMessages ?? [])
      .map((message) => decodeAgentResponse(message.text).text)
      .filter(Boolean);
    if (values.at(-1) !== decoded.text) {
      values.push(decoded.text);
    }
    return values.join("\n\n");
  }

  /// 마지막 공개 메시지에는 [OFFICE_SOURCES] 같은 기계 블록과 [NEED_INPUT]
  /// 표식이 원문 그대로 남아 있지만 저장하는 응답은 그것을 떼어낸 본문이다.
  /// 둘이 어긋나면 화면이 같은 답을 활동과 응답으로 두 번 그리므로,
  /// 백엔드와 상관없이 활동 쪽을 정리된 본문으로 맞춘다.
  async normalizeCompletedMessageActivity(state, decoded) {
    const finalMessage = state.visibleAgentMessages?.at(-1);
    const eventKey = finalMessage?.key
      ? `message:${finalMessage.key}`
      : null;
    if (
      !eventKey ||
      !state.activityRecords.has(eventKey) ||
      finalMessage.text === decoded.text
    ) {
      return;
    }

    const originalText = finalMessage.text;
    await this.addActivity(state, {
      kind: "message",
      text: decoded.text,
      eventKey,
      status: "completed",
      preserveOccurredAt: true,
    });
    finalMessage.text = decoded.text;
    if (state.responseText === originalText) {
      state.responseText = decoded.text;
    }
    if (state.partialText === originalText) {
      state.partialText = decoded.text;
    }
  }

  async recordAgentMessage(state, key, text) {
    if (state.character.backend === "codex") {
      await this.completePendingInitialCodexReasoning(state);
      await this.addActivity(state, {
        kind: "message",
        text,
        eventKey: key ? `message:${key}` : null,
        status: "completed",
        preserveOccurredAt: true,
      });
    } else {
      if (
        state.pendingAgentMessage &&
        state.pendingAgentMessage.key !== key
      ) {
        await this.promotePendingAgentMessage(state);
      }
      state.pendingAgentMessage = { key, text };
    }
    this.rememberVisibleAgentMessage(state, key, text);
    state.responseText = text;
    state.partialText = text;
    await this.persistResponseDraft(
      state,
      this.visibleResponseText(state),
    );
  }

  async promotePendingAgentMessage(state) {
    const pending = state.pendingAgentMessage;
    if (!pending) {
      return;
    }
    state.pendingAgentMessage = null;
    await this.addActivity(state, {
      kind: "message",
      text: pending.text,
      eventKey: pending.key ? `message:${pending.key}` : null,
      status: "completed",
      preserveText: false,
    });
    if (state.responseText === pending.text) {
      state.responseText = "";
      state.partialText = "";
    }
  }

  async activateSession(state, externalSessionID) {
    const isNewSession = !state.externalSessionID;
    await this.withTransaction(async (client) => {
      await client.query(
        `
          UPDATE cli_sessions
          SET external_id = $2
          WHERE id = $1
            AND (external_id IS NULL OR external_id = $2)
        `,
        [state.sessionID, externalSessionID],
      );
      await client.query(
        `
          UPDATE cli_sessions
          SET ended_at = COALESCE(ended_at, now())
          WHERE character_id = $1
            AND id <> $2
            AND ended_at IS NULL
        `,
        [state.character.id, state.sessionID],
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
        [state.character.id, state.sessionID],
      );
    });
    state.externalSessionID = externalSessionID;
    state.rolloutReader = this.rolloutReaderFactory(
      externalSessionID,
      !isNewSession,
    );
    this.broadcast({ type: "session.changed", turnId: state.turnID });
  }

  async addActivity(state, activity) {
    const previous = state.activityWritePromise ?? Promise.resolve();
    const write = previous
      .catch(() => {})
      .then(() => this.persistActivity(state, activity));
    state.activityWritePromise = write;
    try {
      await write;
    } finally {
      if (state.activityWritePromise === write) {
        state.activityWritePromise = null;
      }
    }
  }

  async persistActivity(state, activity) {
    const eventKey = activity.eventKey ?? null;
    const existing = eventKey
      ? state.activityRecords.get(eventKey)
      : null;
    const text = activity.preserveText && existing
      ? existing.text
      : activity.text;
    const status = activity.status ?? "completed";
    const collaboration = activity.collaboration ?? null;
    const collaborationKey = JSON.stringify(collaboration);
    if (
      existing &&
      existing.kind === activity.kind &&
      existing.text === text &&
      existing.status === status &&
      JSON.stringify(existing.collaboration ?? null) === collaborationKey
    ) {
      return;
    }
    const duplicateKey = [
      activity.kind,
      text,
      status,
      collaborationKey,
    ].join("\n");
    if (!eventKey && state.lastActivity === duplicateKey) {
      return;
    }
    state.lastActivity = duplicateKey;

    if (existing) {
      await this.pool.query(
        `
          UPDATE turn_activities
          SET
            kind = $3,
            text = $4,
            status = $5,
            collaboration = $6
          WHERE turn_id = $1
            AND seq = $2
        `,
        [
          state.turnID,
          existing.sequence,
          activity.kind,
          text,
          status,
          collaboration,
        ],
      );
      state.activityRecords.set(eventKey, {
        sequence: existing.sequence,
        kind: activity.kind,
        text,
        status,
        collaboration,
      });
      await this.touchTurn(state.turnID);
      this.broadcast({
        type: "feed.changed",
        turnId: state.turnID,
        characterId: state.character.id,
      });
      return;
    }

    const sequence = state.sequence + 1;
    await this.pool.query(
      `
        INSERT INTO turn_activities (
          turn_id,
          seq,
          kind,
          text,
          event_key,
          status,
          collaboration
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (turn_id, seq) DO NOTHING
      `,
      [
        state.turnID,
        sequence,
        activity.kind,
        text,
        eventKey,
        status,
        collaboration,
      ],
    );
    state.sequence = sequence;
    if (eventKey) {
      state.activityRecords.set(eventKey, {
        sequence,
        kind: activity.kind,
        text,
        status,
        collaboration,
      });
    }
    await this.touchTurn(state.turnID);
    this.broadcast({
      type: "feed.changed",
      turnId: state.turnID,
      characterId: state.character.id,
    });
  }

  async persistPartialResponse(state) {
    const now = Date.now();
    if (now - state.lastPartialPersistedAt < 250) {
      return;
    }
    state.lastPartialPersistedAt = now;
    await this.persistResponseDraft(
      state,
      this.visibleResponseText(state, state.partialText),
    );
  }

  async persistResponseDraft(state, text) {
    const visibleText = decodeAgentResponse(text).text;
    await this.pool.query(
      `
        UPDATE messages
        SET text = $2, received_at = now()
        WHERE turn_id = $1
          AND role = 'assistant'
      `,
      [state.turnID, visibleText],
    );
    await this.touchTurn(state.turnID);
    this.broadcast({
      type: "feed.changed",
      turnId: state.turnID,
      characterId: state.character.id,
    });
  }

  async touchTurn(turnID) {
    await this.pool.query(
      `
        UPDATE turns
        SET updated_at = now()
        WHERE id = $1
      `,
      [turnID],
    );
  }

  async finalizeRunningActivities(state, status) {
    if (state.activityWritePromise) {
      try {
        await state.activityWritePromise;
      } catch {
        // 실패한 개별 저장은 호출자가 처리하고, 이미 저장된 실행 중 행은
        // 아래 일괄 전환으로 끝 상태를 맞춘다.
      }
    }
    const collaborationAgentStatus = status === "completed"
      ? "completed"
      : "errored";
    await this.pool.query(
      `
        UPDATE turn_activities
        SET
          status = $2,
          collaboration = CASE
            WHEN kind = 'collaboration'
              AND jsonb_typeof(collaboration) = 'object'
            THEN jsonb_set(
              collaboration,
              '{agentStatus}',
              to_jsonb($3::text),
              true
            )
            ELSE collaboration
          END
        WHERE turn_id = $1
          AND status = 'running'
      `,
      [state.turnID, status, collaborationAgentStatus],
    );
    if (state.activityRecords instanceof Map) {
      for (const [eventKey, activity] of state.activityRecords) {
        if (activity.status === "running") {
          const collaboration =
            activity.kind === "collaboration" &&
              activity.collaboration &&
              typeof activity.collaboration === "object"
              ? {
                ...activity.collaboration,
                agentStatus: collaborationAgentStatus,
              }
              : activity.collaboration;
          state.activityRecords.set(eventKey, {
            ...activity,
            status,
            collaboration,
          });
        }
      }
    }
  }

  async settleCancelledOutput(state) {
    if (!state.cancelRequested) {
      return false;
    }
    await this.finalizeRunningActivities(state, "failed");
    this.broadcast({
      type: "feed.changed",
      turnId: state.turnID,
      characterId: state.character.id,
    });
    return true;
  }

  async persistUsageRecord(client, state) {
    if (!state.usage) {
      return;
    }
    const usage = state.usage;
    const costUsd = state.character.localProfile ? null : estimateTokenCost({
      backend: state.character.backend,
      model: state.character.model,
      fastMode: state.character.fastMode,
      usage,
    });
    await client.query(
      `
        INSERT INTO usage_records (
          turn_id,
          input_tokens,
          output_tokens,
          cached_input_tokens,
          reasoning_output_tokens,
          cost_usd,
          cache_write_input_tokens,
          cache_write_5m_input_tokens,
          cache_write_1h_input_tokens
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (turn_id) DO UPDATE
        SET
          input_tokens = EXCLUDED.input_tokens,
          output_tokens = EXCLUDED.output_tokens,
          cached_input_tokens = EXCLUDED.cached_input_tokens,
          reasoning_output_tokens = EXCLUDED.reasoning_output_tokens,
          cost_usd = EXCLUDED.cost_usd,
          cache_write_input_tokens = EXCLUDED.cache_write_input_tokens,
          cache_write_5m_input_tokens = EXCLUDED.cache_write_5m_input_tokens,
          cache_write_1h_input_tokens = EXCLUDED.cache_write_1h_input_tokens
      `,
      [
        state.turnID,
        usage.inputTokens,
        usage.outputTokens,
        usage.cachedInputTokens,
        usage.reasoningOutputTokens,
        costUsd,
        usage.cacheWriteInputTokens,
        usage.cacheWrite5mInputTokens,
        usage.cacheWrite1hInputTokens,
      ],
    );
    if (state.character.localProfile) await client.query(
      "UPDATE usage_records SET cost_basis='local-not-applicable', reported_cost_audit=$2::jsonb WHERE turn_id=$1",
      [state.turnID, JSON.stringify({reportedCostUsd: usage.reportedCostUsd ?? null, reportedSonnet5CostUsd: usage.reportedSonnet5CostUsd ?? null})],
    );
  }

  async syncWorkRecordRAGBestEffort(workRecordID) {
    if (!workRecordID) {
      return null;
    }
    try {
      await this.withTransaction(async (client) => {
        await syncWorkRecordRAGDocuments(client, { workRecordID });
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.warn(
        "파생 RAG 동기화에 실패했습니다. 다음 기동 때 다시 색인합니다.",
        message,
      );
      return message;
    }
    if (!this.embeddingService) {
      return null;
    }
    let embedding;
    try {
      embedding = await embedRAGDocumentsBestEffort(
        this.pool,
        this.embeddingService,
        { workRecordIDs: [workRecordID] },
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.warn(
        `임베딩 비활성(사유): ${message}. ` +
          "작업 기록은 저장했고 로컬 임베딩은 나중에 백필합니다.",
      );
      return message;
    }
    if (!embedding.error) {
      return null;
    }
    console.warn(
      `임베딩 비활성(사유): ${embedding.error}. ` +
        "작업 기록은 저장했고 로컬 임베딩은 나중에 백필합니다.",
    );
    return embedding.error;
  }

  async transitionWorkRecordReviewBestEffort(options) {
    let workRecordID = null;
    try {
      await this.withTransaction(async (client) => {
        workRecordID = await transitionTurnWorkRecordReview(
          client,
          options,
        );
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.warn(
        "작업 기록의 검토 상태 동기화에 실패했습니다.",
        message,
      );
      return message;
    }
    return this.syncWorkRecordRAGBestEffort(workRecordID);
  }

  async complete(state, decoded) {
    if (state.cancelRequested) {
      return;
    }
    await this.completePendingInitialCodexReasoning(state);
    await this.finalizeRunningActivities(state, "completed");
    await this.normalizeCompletedMessageActivity(state, decoded);
    const generatedImages = listGeneratedImages(
      state.externalSessionID,
      generatedImageRoot(state.character.backend),
    ).filter((path) => !state.initialGeneratedImages.has(path));
    const responseText = appendLocalImagePreviews(
      this.completedResponseText(state, decoded),
      generatedImages,
    );
    const responseSources = portableResponseSources(
      decoded.sources ?? [],
      state.workdir,
    );
    const workspaceReview =
      state.workspace &&
        this.workspaceManager &&
        !decoded.needsInput
        ? await this.workspaceManager.prepareReview(state.workspace)
        : null;
    const reviewStatus = decoded.needsInput
      ? "needs_input"
      : workspaceReview?.hasChanges
      ? "awaiting_approval"
      : state.workspace
      ? "not_required"
      : "not_applicable";

    let completedWorkRecordID = null;
    await this.withTransaction(async (client) => {
      let sourceWarning = decoded.sourceError ?? null;
      let storedResponseSourceCount = 0;
      try {
        const storedSources = await replaceTurnResponseSources(
          client,
          state.turnID,
          responseSources,
        );
        storedResponseSourceCount = storedSources.length;
      } catch (error) {
        if (!(error instanceof ProvenanceValidationError)) {
          throw error;
        }
        sourceWarning = error.message;
        await replaceTurnResponseSources(client, state.turnID, []);
      }
      await client.query(
        `
          UPDATE messages
          SET text = $2, received_at = now()
          WHERE turn_id = $1
            AND role = 'assistant'
        `,
        [state.turnID, responseText],
      );
      await client.query(
        `
          UPDATE turns
          SET
            status = 'completed',
            needs_input = $2,
            response_source_warning = $3,
            wiki_proposal_warning = NULL,
            error_message = NULL,
            ended_at = COALESCE($4::timestamptz, now()),
            updated_at = now()
          WHERE id = $1
        `,
        [
          state.turnID,
          decoded.needsInput,
          sourceWarning,
          state.endedAt ?? null,
        ],
      );
      await this.persistUsageRecord(client, state);
      const storedRecord = await persistCompletedTurnWorkRecord(client, {
        repositoryRoot: state.workspace?.repositoryRoot ?? this.repositoryRoot,
        turnID: state.turnID,
        workspaceID: state.workspace?.id ?? null,
        characterID: state.character.id,
        prompt: state.recordPrompt ?? state.prompt,
        response: decoded.text,
        backend: state.character.backend,
        model: state.character.model,
        needsInput: decoded.needsInput,
        responseSourceCount: storedResponseSourceCount,
        responseSourceWarning: sourceWarning,
        reviewStatus,
        reviewTree: workspaceReview?.reviewTree ?? null,
        headCommit: workspaceReview?.headCommit ?? null,
        changedFiles: workspaceReview?.changedFiles ?? [],
        refreshTerminalResult: state.refreshTerminalResult === true,
      });
      completedWorkRecordID = storedRecord?.workRecordId ?? null;
      const wikiProposalWarning = await persistTurnWikiProposals(client, {
        repositoryRoot:
          state.workspace?.repositoryRoot ?? this.repositoryRoot,
        workRecordID: completedWorkRecordID,
        turnID: state.turnID,
        characterID: state.character.id,
        proposals: decoded.proposals ?? [],
        parserWarning: decoded.wikiProposalError ?? null,
        needsInput: decoded.needsInput,
      });
      if (wikiProposalWarning) {
        await client.query(
          `
            UPDATE turns
            SET wiki_proposal_warning = $2, updated_at = now()
            WHERE id = $1
          `,
          [state.turnID, wikiProposalWarning],
        );
      }
      if (workspaceReview) {
        const nextStatus = workspaceReview.hasChanges
          ? "awaiting_approval"
          : "active";
        await client.query(
          `
            UPDATE task_workspaces
            SET
              status = $2,
              review_turn_id = CASE WHEN $3 THEN $4::uuid ELSE NULL END,
              review_tree = CASE WHEN $3 THEN $5 ELSE NULL END,
              head_commit = $6,
              changed_files = $7::jsonb,
              error_message = NULL,
              review_requested_at = CASE WHEN $3 THEN now() ELSE NULL END,
              updated_at = now()
            WHERE id = $1
              AND status = 'active'
          `,
          [
            state.workspace.id,
            nextStatus,
            workspaceReview.hasChanges,
            state.turnID,
            workspaceReview.reviewTree,
            workspaceReview.headCommit,
            JSON.stringify(workspaceReview.changedFiles ?? []),
          ],
        );
      }
    });
    await this.syncWorkRecordRAGBestEffort(completedWorkRecordID);
    if (workspaceReview) {
      state.workspace = {
        ...state.workspace,
        status: workspaceReview.hasChanges
          ? "awaiting_approval"
          : "active",
        reviewTurnID: workspaceReview.hasChanges ? state.turnID : null,
        reviewTree: workspaceReview.hasChanges
          ? workspaceReview.reviewTree
          : null,
        headCommit: workspaceReview.headCommit,
        changedFiles: workspaceReview.changedFiles ?? [],
      };
      this.broadcast({
        type: "workspace.changed",
        turnId: state.turnID,
        characterId: state.character.id,
        status: state.workspace.status,
      });
    }
    if (workspaceReview?.hasChanges) {
      this.closeClaudeWorker(
        state.character.id,
        state.claudeWorker,
        new Error("변경된 작업 공간을 통합 대기로 보존합니다."),
      );
    }

    if (this.running.get(state.character.id) === state) {
      this.running.delete(state.character.id);
    }
    this.broadcast({
      type: "feed.changed",
      turnId: state.turnID,
      characterId: state.character.id,
      costChanged: true,
    });
  }

  async workspaceReviewRecord(client, turnID, lock = false) {
    const result = await client.query(
      `
        SELECT
          workspace.*,
          session.character_id AS "characterID",
          session.conversation_id AS "conversationID",
          character.name AS "characterName"
        FROM task_workspaces AS workspace
        JOIN cli_sessions AS session
          ON session.id = workspace.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE workspace.review_turn_id = $1
        ${lock ? "FOR UPDATE OF workspace, session" : ""}
      `,
      [turnID],
    );
    if (result.rowCount === 0) {
      throw new AgentJobNotFoundError(
        "검토할 작업 공간을 찾을 수 없습니다.",
      );
    }
    return {
      workspace: workspaceFromRow(result.rows[0]),
      characterID: result.rows[0].characterID,
      conversationID: result.rows[0].conversationID,
      characterName: result.rows[0].characterName,
    };
  }

  async inspectWorkspaceForSessionEnd(characterID, client = this.pool) {
    if (this.running.has(characterID)) {
      throw new AgentBusyError(
        "이 직원의 실행 중인 업무가 끝난 뒤 CLI를 변경하세요.",
      );
    }

    const busy = await client.query(
      `
        SELECT turn.id
        FROM turns AS turn
        JOIN cli_sessions AS session
          ON session.id = turn.cli_session_id
        WHERE session.character_id = $1
          AND turn.status IN ('pending', 'running')
        LIMIT 1
      `,
      [characterID],
    );
    if (busy.rowCount > 0) {
      throw new AgentBusyError(
        "이 직원의 실행 중인 업무가 끝난 뒤 CLI를 변경하세요.",
      );
    }

    const candidate = await client.query(
      `
        SELECT
          workspace.*,
          session.character_id AS "characterID",
          latest_turn.id AS "reviewCandidateTurnID"
        FROM task_workspaces AS workspace
        JOIN cli_sessions AS session
          ON session.id = workspace.cli_session_id
        LEFT JOIN LATERAL (
          SELECT turn.id
          FROM turns AS turn
          WHERE turn.task_workspace_id = workspace.id
          ORDER BY turn.started_at DESC, turn.id DESC
          LIMIT 1
        ) AS latest_turn ON true
        WHERE session.character_id = $1
          AND workspace.status IN (
            'active',
            'awaiting_approval',
            'merging',
            'conflict'
          )
        ORDER BY
          CASE workspace.status
            WHEN 'awaiting_approval' THEN 1
            WHEN 'merging' THEN 2
            WHEN 'conflict' THEN 3
            ELSE 4
          END,
          workspace.updated_at DESC
        LIMIT 1
      `,
      [characterID],
    );

    if (candidate.rowCount === 0) {
      return {
        characterID,
        workspace: null,
        reviewTurnID: null,
        review: null,
      };
    }

    const workspace = workspaceFromRow(candidate.rows[0]);
    if (BLOCKED_WORKSPACE_STATUSES.has(workspace.status)) {
      throw new AgentBusyError(
        "이 직원의 대기 중 변경사항을 먼저 통합하거나 폐기하세요.",
      );
    }
    if (!this.workspaceManager) {
      throw new AgentBusyError(
        "작업 공간 상태를 확인할 Git 실행기가 준비되지 않았습니다.",
      );
    }
    const reviewTurnID = candidate.rows[0].reviewCandidateTurnID;
    if (!reviewTurnID) {
      throw new AgentBusyError(
        "작업 공간에 연결된 업무 기록을 찾을 수 없습니다.",
      );
    }

    const review = await this.workspaceManager.prepareReview(workspace);
    return {
      characterID,
      workspace,
      reviewTurnID,
      review,
    };
  }

  async applyWorkspaceSessionEndPlan(client, plan) {
    if (plan.review?.hasChanges) {
      throw new AgentBusyError(
        "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 통합하거나 폐기하세요.",
      );
    }
    if (!plan.workspace) {
      const active = await client.query(
        `
          DELETE FROM active_cli_sessions
          WHERE character_id = $1
          RETURNING cli_session_id
        `,
        [plan.characterID],
      );
      await client.query(
        `
          UPDATE cli_sessions
          SET ended_at = COALESCE(ended_at, now())
          WHERE character_id = $1
            AND ended_at IS NULL
        `,
        [plan.characterID],
      );
      return { ended: active.rowCount > 0 };
    }

    const updated = await client.query(
      `
        UPDATE task_workspaces
        SET
          status = 'closed',
          review_tree = NULL,
          changed_files = '[]'::jsonb,
          error_message = NULL,
          updated_at = now()
        WHERE id = $1
          AND status = 'active'
        RETURNING id
      `,
      [plan.workspace.id],
    );
    if (updated.rowCount === 0) {
      throw new AgentBusyError(
        "작업 공간 상태가 바뀌었습니다. 다시 확인하세요.",
      );
    }
    await client.query(
      `
        DELETE FROM active_cli_sessions
        WHERE cli_session_id = $1
      `,
      [plan.workspace.cliSessionID],
    );
    await client.query(
      `
        UPDATE cli_sessions
        SET ended_at = COALESCE(ended_at, now())
        WHERE id = $1
      `,
      [plan.workspace.cliSessionID],
    );
    return { ended: true };
  }

  async finalizeWorkspaceSessionEndPlan(plan) {
    if (!plan.workspace) {
      return null;
    }
    const warnings = [];
    try {
      await this.workspaceManager.cleanup(plan.workspace);
    } catch (error) {
      warnings.push(
        `빈 작업 공간 정리에 실패했습니다. ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
    const transitionWarning = await this.transitionWorkRecordReviewBestEffort({
      turnID: plan.reviewTurnID,
      status: "not_required",
      reviewTree: plan.review.reviewTree,
      headCommit: plan.review.headCommit,
      changedFiles: [],
      actorType: "system",
    });
    if (transitionWarning) {
      warnings.push(transitionWarning);
    }
    this.broadcast({
      type: "workspace.changed",
      turnId: plan.reviewTurnID,
      characterId: plan.characterID,
      status: "closed",
    });
    return warnings.length > 0
      ? `${plan.characterID}: ${warnings.join(" ")}`
      : null;
  }

  async prepareWorkspaceForSessionEnd(characterID) {
    const plan = await this.inspectWorkspaceForSessionEnd(characterID);
    const { workspace, reviewTurnID, review } = plan;
    if (!workspace) {
      this.closeClaudeWorker(
        characterID,
        null,
        new Error("직원 CLI 세션을 종료합니다."),
      );
      return await this.withTransaction(async (client) =>
        await this.applyWorkspaceSessionEndPlan(client, plan)
      );
    }

    if (review.hasChanges) {
      this.closeClaudeWorker(
        characterID,
        null,
        new Error("작업 공간 검토 전에 Claude Code 세션을 닫습니다."),
      );
      const updated = await this.pool.query(
        `
          UPDATE task_workspaces
          SET
            status = 'awaiting_approval',
            review_turn_id = $2::uuid,
            review_tree = $3,
            head_commit = $4,
            changed_files = $5::jsonb,
            error_message = NULL,
            review_requested_at = now(),
            updated_at = now()
          WHERE id = $1
            AND status = 'active'
          RETURNING id
        `,
        [
          workspace.id,
          reviewTurnID,
          review.reviewTree,
          review.headCommit,
          JSON.stringify(review.changedFiles ?? []),
        ],
      );
      if (updated.rowCount === 0) {
        throw new AgentBusyError(
          "작업 공간 상태가 바뀌었습니다. 다시 확인하세요.",
        );
      }
      await this.transitionWorkRecordReviewBestEffort({
        turnID: reviewTurnID,
        status: "awaiting_approval",
        reviewTree: review.reviewTree,
        headCommit: review.headCommit,
        changedFiles: review.changedFiles ?? [],
        actorType: "system",
      });
      this.broadcast({
        type: "workspace.changed",
        turnId: reviewTurnID,
        characterId: characterID,
        status: "awaiting_approval",
      });
      this.broadcast({
        type: "feed.changed",
        turnId: reviewTurnID,
        characterId: characterID,
      });
      throw new AgentBusyError(
        "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 통합하거나 폐기하세요.",
      );
    }

    this.closeClaudeWorker(
      characterID,
      null,
      new Error("직원 CLI 세션을 종료합니다."),
    );
    await this.workspaceManager.cleanup(workspace);
    const closed = await this.withTransaction(async (client) =>
      await this.applyWorkspaceSessionEndPlan(client, plan)
    );
    await this.transitionWorkRecordReviewBestEffort({
      turnID: reviewTurnID,
      status: "not_required",
      reviewTree: review.reviewTree,
      headCommit: review.headCommit,
      changedFiles: [],
      actorType: "system",
    });
    this.broadcast({
      type: "workspace.changed",
      turnId: reviewTurnID,
      characterId: characterID,
      status: "closed",
    });
    return closed;
  }

  async fetchWorkspaceReview(turnID) {
    if (!this.workspaceManager) {
      throw new AgentJobNotFoundError(
        "검토할 작업 공간을 찾을 수 없습니다.",
      );
    }
    let record = await this.workspaceReviewRecord(
      this.pool,
      turnID,
    );
    if (record.workspace.status === "awaiting_approval") {
      const current = await this.workspaceManager.prepareReview(
        record.workspace,
      );
      if (current.reviewTree !== record.workspace.reviewTree) {
        const refreshed = current.hasChanges
          ? await this.pool.query(
            `
              UPDATE task_workspaces
              SET
                review_tree = $2,
                head_commit = $3,
                changed_files = $4::jsonb,
                error_message = NULL,
                review_requested_at = now(),
                updated_at = now()
              WHERE id = $1
                AND status = 'awaiting_approval'
                AND review_tree = $5
              RETURNING *
            `,
            [
              record.workspace.id,
              current.reviewTree,
              current.headCommit,
              JSON.stringify(current.changedFiles ?? []),
              record.workspace.reviewTree,
            ],
          )
          : await this.pool.query(
            `
              UPDATE task_workspaces
              SET
                status = 'active',
                review_turn_id = NULL,
                review_tree = NULL,
                head_commit = $2,
                changed_files = '[]'::jsonb,
                error_message = NULL,
                review_requested_at = NULL,
                updated_at = now()
              WHERE id = $1
                AND status = 'awaiting_approval'
                AND review_tree = $3
              RETURNING *
            `,
            [
              record.workspace.id,
              current.headCommit,
              record.workspace.reviewTree,
            ],
          );
        if (refreshed.rowCount === 0) {
          throw new AgentBusyError(
            "검토 상태가 바뀌었습니다. 다시 불러오세요.",
          );
        }
        record.workspace = workspaceFromRow(refreshed.rows[0]);
        await this.transitionWorkRecordReviewBestEffort({
          turnID,
          status: current.hasChanges
            ? "awaiting_approval"
            : "not_required",
          reviewTree: current.reviewTree,
          headCommit: current.headCommit,
          changedFiles: current.changedFiles ?? [],
          actorType: "system",
        });
        this.broadcast({
          type: "workspace.changed",
          turnId: turnID,
          characterId: record.characterID,
          status: record.workspace.status,
        });
      }
    }
    const diff = await this.workspaceManager.diff(record.workspace, {
      maxBytes: MAX_WORKSPACE_DIFF_BYTES,
    });
    return {
      workspace: workspaceReviewPayload(record.workspace, diff),
    };
  }

  async approveWorkspace(
    turnID,
    expectedReviewTree,
    { actorType = "user" } = {},
  ) {
    if (actorType !== "system" && this.draining) {
      throw new AgentDrainingError(
        "백엔드가 안전한 전환을 준비 중이라 새 통합을 받지 않습니다.",
      );
    }
    return await this.withPostProcessing(
      `approve-workspace:${turnID}`,
      async () =>
        await this.approveWorkspaceAccepted(
          turnID,
          expectedReviewTree,
          { actorType },
        ),
    );
  }

  async approveWorkspaceAccepted(
    turnID,
    expectedReviewTree,
    { actorType = "user" } = {},
  ) {
    if (!this.workspaceManager) {
      throw new AgentJobNotFoundError(
        "통합할 작업 공간을 찾을 수 없습니다.",
      );
    }
    const reviewedTree = String(expectedReviewTree ?? "").trim();
    if (!reviewedTree) {
      throw new AgentBusyError(
        "검토한 변경 버전을 확인할 수 없습니다. diff를 다시 불러오세요.",
      );
    }

    let merged;
    try {
      merged = await this.withTransaction(async (client) => {
        const record = await this.workspaceReviewRecord(
          client,
          turnID,
          true,
        );
        if (![
          "awaiting_approval",
          "conflict",
        ].includes(record.workspace.status)) {
          throw new AgentBusyError(
            "현재 상태에서는 이 변경사항을 승인할 수 없습니다.",
          );
        }
        if (record.workspace.reviewTree !== reviewedTree) {
          throw new AgentBusyError(
            "검토 뒤 변경사항이 갱신됐습니다. diff를 다시 확인하세요.",
          );
        }

        await client.query(
          "SELECT pg_advisory_xact_lock(hashtext($1))",
          [record.workspace.repositoryRoot],
        );
        const transitioned = await client.query(
          `
            UPDATE task_workspaces
            SET
              status = 'merging',
              error_message = NULL,
              updated_at = now()
            WHERE id = $1
              AND status IN ('awaiting_approval', 'conflict')
            RETURNING id
          `,
          [record.workspace.id],
        );
        if (transitioned.rowCount === 0) {
          throw new AgentBusyError(
            "다른 통합 작업이 이미 이 변경사항을 처리하고 있습니다.",
          );
        }

        const approved = await this.workspaceManager.approve(
          record.workspace,
          {
            expectedReviewTree: reviewedTree,
            commitMessage: workspaceCommitMessage(
              record.characterName,
              turnID,
            ),
          },
        );
        await client.query(
          `
            UPDATE task_workspaces
            SET
              status = 'merged',
              task_commit = $2,
              merged_commit = $3,
              error_message = NULL,
              merged_at = now(),
              updated_at = now()
            WHERE id = $1
          `,
          [
            record.workspace.id,
            approved.taskCommit,
            approved.mergedCommit,
          ],
        );
        return {
          characterID: record.characterID,
          workspace: {
            ...record.workspace,
            status: "merged",
            taskCommit: approved.taskCommit,
            mergedCommit: approved.mergedCommit,
            errorMessage: null,
            mergedAt: new Date().toISOString(),
          },
        };
      });
    } catch (error) {
      try {
        await this.recordWorkspaceApprovalError(turnID, error);
      } catch {
        // 원래 Git 안전 오류를 호출자에게 그대로 전달한다.
      }
      throw error;
    }

    await this.transitionWorkRecordReviewBestEffort({
      turnID,
      status: "merged",
      taskCommit: merged.workspace.taskCommit,
      mergedCommit: merged.workspace.mergedCommit,
      reviewTree: reviewedTree,
      headCommit: merged.workspace.headCommit,
      changedFiles: merged.workspace.changedFiles,
      actorType: actorType === "system" ? "system" : "user",
    });
    let cleanupWarning = null;
    try {
      await this.workspaceManager.cleanup(merged.workspace);
    } catch (error) {
      cleanupWarning =
        `통합은 완료됐지만 작업 공간 정리에 실패했습니다. ${
          error instanceof Error ? error.message : String(error)
        }`;
      await this.pool.query(
        `
          UPDATE task_workspaces
          SET error_message = $2, updated_at = now()
          WHERE id = $1
            AND status = 'merged'
        `,
        [merged.workspace.id, cleanupWarning],
      );
      merged.workspace.errorMessage = cleanupWarning;
    }
    this.broadcast({
      type: "workspace.changed",
      turnId: turnID,
      characterId: merged.characterID,
      status: "merged",
    });
    return {
      workspace: workspaceReviewPayload(merged.workspace, {
        diff: "",
        diffTruncated: false,
      }),
      cleanupWarning,
    };
  }

  async recordWorkspaceApprovalError(turnID, error) {
    if (!error?.code) {
      return;
    }
    const message = error instanceof Error ? error.message : String(error);
    if (error.code === "changed-after-review") {
      const record = await this.workspaceReviewRecord(this.pool, turnID);
      const review = await this.workspaceManager.prepareReview(
        record.workspace,
      );
      const updated = await this.pool.query(
        `
          UPDATE task_workspaces
          SET
            status = CASE WHEN $7 THEN 'awaiting_approval' ELSE 'active' END,
            review_turn_id = CASE WHEN $7 THEN $6::uuid ELSE NULL END,
            review_tree = CASE WHEN $7 THEN $2 ELSE NULL END,
            head_commit = $3,
            changed_files = $4::jsonb,
            error_message = CASE WHEN $7 THEN $5 ELSE NULL END,
            review_requested_at = CASE WHEN $7 THEN now() ELSE NULL END,
            updated_at = now()
          WHERE id = $1
            AND review_turn_id = $6::uuid
            AND status IN ('awaiting_approval', 'conflict')
        `,
        [
          record.workspace.id,
          review.reviewTree,
          review.headCommit,
          JSON.stringify(review.changedFiles ?? []),
          message,
          turnID,
          review.hasChanges,
        ],
      );
      if (updated.rowCount > 0) {
        const status = review.hasChanges ? "awaiting_approval" : "active";
        await this.transitionWorkRecordReviewBestEffort({
          turnID,
          status: review.hasChanges
            ? "awaiting_approval"
            : "not_required",
          reviewTree: review.reviewTree,
          headCommit: review.headCommit,
          changedFiles: review.changedFiles ?? [],
          errorMessage: review.hasChanges ? message : null,
          actorType: "system",
        });
        this.broadcast({
          type: "workspace.changed",
          turnId: turnID,
          characterId: record.characterID,
          status,
        });
      }
      return;
    }

    const status = error.code === "conflict"
      ? "conflict"
      : "awaiting_approval";
    const result = await this.pool.query(
      `
        UPDATE task_workspaces AS workspace
        SET
          status = CASE
            WHEN workspace.status = 'conflict' THEN 'conflict'
            ELSE $2
          END,
          error_message = $3,
          updated_at = now()
        FROM cli_sessions AS session
        WHERE workspace.review_turn_id = $1
          AND session.id = workspace.cli_session_id
          AND workspace.status IN ('awaiting_approval', 'conflict')
        RETURNING
          session.character_id AS "characterID",
          workspace.status
      `,
      [turnID, status, message],
    );
    if (result.rowCount > 0) {
      await this.transitionWorkRecordReviewBestEffort({
        turnID,
        status: result.rows[0].status ?? status,
        errorMessage: message,
        actorType: "system",
      });
      this.broadcast({
        type: "workspace.changed",
        turnId: turnID,
        characterId: result.rows[0].characterID,
        status: result.rows[0].status ?? status,
      });
    }
  }

  async rejectWorkspace(turnID) {
    if (this.draining) {
      throw new AgentDrainingError(
        "백엔드가 안전한 전환을 준비 중이라 새 거절을 받지 않습니다.",
      );
    }
    return await this.withPostProcessing(
      `reject-workspace:${turnID}`,
      async () => await this.rejectWorkspaceAccepted(turnID),
    );
  }

  async rejectWorkspaceAccepted(turnID) {
    const rejected = await this.withTransaction(async (client) => {
      const record = await this.workspaceReviewRecord(
        client,
        turnID,
        true,
      );
      if (!["awaiting_approval", "conflict"].includes(
        record.workspace.status,
      )) {
        throw new AgentBusyError(
          "현재 상태에서는 이 변경사항을 거절할 수 없습니다.",
        );
      }
      const result = await client.query(
        `
          UPDATE task_workspaces
          SET
            status = 'rejected',
            error_message = NULL,
            rejected_at = now(),
            updated_at = now()
          WHERE id = $1
            AND status IN ('awaiting_approval', 'conflict')
          RETURNING id
        `,
        [record.workspace.id],
      );
      if (result.rowCount === 0) {
        throw new AgentBusyError(
          "다른 작업이 이미 이 변경사항을 처리했습니다.",
        );
      }
      const workRecordID = await transitionTurnWorkRecordReview(client, {
        turnID,
        status: "rejected",
        reviewTree: record.workspace.reviewTree,
        headCommit: record.workspace.headCommit,
        changedFiles: record.workspace.changedFiles,
      });
      return {
        characterID: record.characterID,
        workRecordID,
        workspace: {
          ...record.workspace,
          status: "rejected",
          errorMessage: null,
          rejectedAt: new Date().toISOString(),
        },
      };
    });
    await this.syncWorkRecordRAGBestEffort(rejected.workRecordID);
    this.broadcast({
      type: "workspace.changed",
      turnId: turnID,
      characterId: rejected.characterID,
      status: "rejected",
    });
    return {
      workspace: workspaceReviewPayload(rejected.workspace, {
        diff: "",
        diffTruncated: false,
      }),
    };
  }

  async fail(state, error) {
    if (this.running.get(state.character.id) !== state) {
      return;
    }
    discardStructuredTurnResult(state.structuredResultPath);
    state.structuredResultPath = null;
    await this.completePendingInitialCodexReasoning(state);
    await this.finalizeRunningActivities(state, "failed");
    const message =
      error instanceof Error ? error.message : String(error);
    await this.withTransaction(async (client) => {
      await client.query(
        `
          UPDATE turns
          SET
            status = 'failed',
            error_message = $2,
            ended_at = now(),
            updated_at = now()
          WHERE id = $1
        `,
        [state.turnID, message],
      );
      await this.persistUsageRecord(client, state);
      if (!state.externalSessionID) {
        await client.query(
          `
            UPDATE cli_sessions
            SET ended_at = COALESCE(ended_at, now())
            WHERE id = $1
          `,
          [state.sessionID],
        );
        if (state.workspace) {
          await client.query(
            `
              UPDATE task_workspaces
              SET
                status = 'failed',
                error_message = $2,
                updated_at = now()
              WHERE id = $1
                AND status = 'active'
            `,
            [state.workspace.id, message],
          );
        }
      }
    });
    if (this.running.get(state.character.id) === state) {
      this.running.delete(state.character.id);
    }
    this.broadcast({
      type: "feed.changed",
      turnId: state.turnID,
      characterId: state.character.id,
      costChanged: true,
    });
  }
}

function workspaceFromRow(row) {
  if (!row) {
    return null;
  }
  return {
    id: row.id ?? row.workspaceID ?? row.workspace_id,
    cliSessionID: row.cliSessionID ?? row.cli_session_id,
    status: row.status,
    repositoryRoot: row.repositoryRoot ?? row.repository_root,
    sourceWorkdir: row.sourceWorkdir ?? row.source_workdir,
    worktreePath: row.worktreePath ?? row.worktree_path,
    executionWorkdir: row.executionWorkdir ?? row.execution_workdir,
    branchName: row.branchName ?? row.branch_name,
    baseBranch: row.baseBranch ?? row.base_branch,
    baseCommit: row.baseCommit ?? row.base_commit,
    reviewTurnID: row.reviewTurnID ?? row.review_turn_id ?? null,
    reviewTree: row.reviewTree ?? row.review_tree ?? null,
    headCommit: row.headCommit ?? row.head_commit ?? null,
    changedFiles: row.changedFiles ?? row.changed_files ?? [],
    taskCommit: row.taskCommit ?? row.task_commit ?? null,
    mergedCommit: row.mergedCommit ?? row.merged_commit ?? null,
    errorMessage: row.errorMessage ?? row.error_message ?? null,
    createdAt: row.createdAt ?? row.created_at ?? null,
    updatedAt: row.updatedAt ?? row.updated_at ?? null,
    reviewRequestedAt:
      row.reviewRequestedAt ?? row.review_requested_at ?? null,
    mergedAt: row.mergedAt ?? row.merged_at ?? null,
    rejectedAt: row.rejectedAt ?? row.rejected_at ?? null,
  };
}

function workspaceFromActiveRow(row) {
  if (!row?.workspaceStatus) {
    return null;
  }
  return workspaceFromRow({
    id: row.workspaceID,
    cliSessionID: row.id,
    status: row.workspaceStatus,
    repositoryRoot: row.workspaceRepositoryRoot,
    sourceWorkdir: row.workspaceSourceWorkdir,
    worktreePath: row.workspaceWorktreePath,
    executionWorkdir: row.workspaceExecutionWorkdir,
    branchName: row.workspaceBranchName,
    baseBranch: row.workspaceBaseBranch,
    baseCommit: row.workspaceBaseCommit,
    reviewTurnID: row.workspaceReviewTurnID,
    reviewTree: row.workspaceReviewTree,
    headCommit: row.workspaceHeadCommit,
    changedFiles: row.workspaceChangedFiles,
    taskCommit: row.workspaceTaskCommit,
    mergedCommit: row.workspaceMergedCommit,
    errorMessage: row.workspaceErrorMessage,
    createdAt: row.workspaceCreatedAt,
    updatedAt: row.workspaceUpdatedAt,
    reviewRequestedAt: row.workspaceReviewRequestedAt,
    mergedAt: row.workspaceMergedAt,
    rejectedAt: row.workspaceRejectedAt,
  });
}

function activeSessionMatchesRuntime(row, { workdir, repositoryRoot }) {
  if (!row) {
    return false;
  }
  const currentWorkdir = canonicalRuntimePath(workdir);
  const currentRepositoryRoot = canonicalRuntimePath(repositoryRoot);
  const sessionRepositoryRoot = canonicalRuntimePath(
    row.sessionRepositoryRoot ?? row.workspaceRepositoryRoot,
  );
  if (sessionRepositoryRoot) {
    return sessionRepositoryRoot === currentRepositoryRoot;
  }
  const conversationWorkdir = canonicalRuntimePath(row.conversationWorkdir);
  return Boolean(
    conversationWorkdir &&
      (
        conversationWorkdir === currentWorkdir ||
        conversationWorkdir === currentRepositoryRoot
      ),
  );
}

function canonicalRuntimePath(value) {
  const candidate = String(value ?? "").trim();
  if (!candidate) {
    return null;
  }
  const absolute = resolve(candidate);
  try {
    return realpathSync(absolute);
  } catch {
    return absolute;
  }
}

export function normalizeAutoCompactPercent(value) {
  const percent = Number(value);
  if (!Number.isFinite(percent)) {
    return 90;
  }
  return Math.min(95, Math.max(20, Math.round(percent)));
}

function workspaceReviewPayload(workspace, diff) {
  return {
    status: workspace.status,
    repositoryRoot: workspace.repositoryRoot,
    worktreePath: workspace.worktreePath,
    executionWorkdir: ["merged", "closed"].includes(workspace.status)
      ? workspace.sourceWorkdir
      : workspace.executionWorkdir,
    branchName: workspace.branchName,
    baseBranch: workspace.baseBranch,
    baseCommit: workspace.baseCommit,
    reviewTurnId: workspace.reviewTurnID,
    reviewTree: workspace.reviewTree,
    headCommit: workspace.headCommit,
    changedFiles: workspace.changedFiles ?? [],
    taskCommit: workspace.taskCommit,
    mergedCommit: workspace.mergedCommit,
    errorMessage: workspace.errorMessage,
    reviewRequestedAt: workspace.reviewRequestedAt,
    mergedAt: workspace.mergedAt,
    rejectedAt: workspace.rejectedAt,
    diff: diff.diff,
    diffTruncated: diff.diffTruncated,
  };
}

function workspaceCommitMessage(characterName, turnID) {
  const taskID = String(turnID).slice(0, 8);
  return `OFFICESTRA: ${characterName} 업무 ${taskID}`;
}

function isTaskWorkspaceExecutionConflict(error) {
  return (
    error != null &&
    typeof error === "object" &&
    error.code === "23505" &&
    error.constraint === TASK_WORKSPACE_EXECUTION_CONSTRAINT
  );
}

function captureFileSnapshots(workdir, changes) {
  const snapshots = new Map();
  let remainingBytes = MAX_TURN_SNAPSHOT_BYTES;
  for (const change of changes ?? []) {
    const path = resolvedChangePath(workdir, change?.path);
    if (!path || snapshots.has(path)) {
      continue;
    }
    const snapshot = readFileSnapshot(path, remainingBytes);
    if (!snapshot) {
      continue;
    }
    remainingBytes -= snapshot.length;
    snapshots.set(path, snapshot);
  }
  return snapshots;
}

function fileChangeStatistics(workdir, snapshots, changes) {
  if (!(snapshots instanceof Map)) {
    return null;
  }
  const directory = mkdtempSync(join(tmpdir(), "office-file-diff-"));
  try {
    const beforeDirectory = join(directory, "before");
    const afterDirectory = join(directory, "after");
    mkdirSync(beforeDirectory);
    mkdirSync(afterDirectory);
    const uniquePaths = normalizedChangePaths(workdir, changes);
    if (uniquePaths.length === 0) {
      return null;
    }

    let remainingAfterBytes = MAX_TURN_SNAPSHOT_BYTES;
    for (const [index, path] of uniquePaths.entries()) {
      const before = snapshots.get(path);
      const after = readFileSnapshot(path, remainingAfterBytes);
      if (!Buffer.isBuffer(before) || !Buffer.isBuffer(after)) {
        return null;
      }
      remainingAfterBytes -= after.length;
      const name = String(index).padStart(5, "0");
      writeFileSync(join(beforeDirectory, name), before);
      writeFileSync(join(afterDirectory, name), after);
    }

    const result = spawnSync(
      "/usr/bin/git",
      [
        "diff",
        "--no-index",
        "--numstat",
        "--no-ext-diff",
        "--no-textconv",
        "--",
        beforeDirectory,
        afterDirectory,
      ],
      { encoding: "utf8", maxBuffer: 1_000_000 },
    );
    if (result.error || ![0, 1].includes(result.status)) {
      return null;
    }
    const lines = String(result.stdout ?? "")
      .trim()
      .split("\n")
      .filter(Boolean);
    if (lines.length !== uniquePaths.length) {
      return null;
    }

    let additions = 0;
    let deletions = 0;
    for (const line of lines) {
      const [added, deleted] = line.split("\t");
      const addedCount = Number.parseInt(added, 10);
      const deletedCount = Number.parseInt(deleted, 10);
      if (!Number.isFinite(addedCount) || !Number.isFinite(deletedCount)) {
        return null;
      }
      additions += addedCount;
      deletions += deletedCount;
    }
    return { additions, deletions };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function createRolloutReader(sessionID, startAtEnd) {
  const path = findRolloutPath(sessionID);
  if (!path) {
    return null;
  }
  return {
    path,
    offset: startAtEnd ? statSync(path).size : 0,
    remainder: "",
    pending: [],
    collaborationPending: [],
    collaborationTracker: new CodexRolloutCollaborationTracker(),
  };
}

function findRolloutPath(sessionID) {
  const id = String(sessionID ?? "").trim();
  if (!id) {
    return null;
  }
  const cached = rolloutPathCache.get(id);
  if (cached && existsSync(cached)) {
    return cached;
  }
  const root = join(homedir(), ".codex", "sessions");
  if (!existsSync(root)) {
    return null;
  }
  let entries;
  try {
    entries = readdirSync(root, { recursive: true });
  } catch {
    return null;
  }
  const relative = entries.find((entry) => {
    const value = String(entry);
    return value.endsWith(".jsonl") && value.includes(id);
  });
  if (!relative) {
    return null;
  }
  const path = join(root, String(relative));
  rolloutPathCache.set(id, path);
  return path;
}

export function latestCodexUsageFromRollout(path) {
  return latestUsageFromTail(path, codexRolloutUsage);
}

// 중단된 Claude 턴은 결과 이벤트를 받지 못하므로 세션 기록 끝에서
// 마지막 사용량을 읽어 복구한다.
export function latestClaudeUsageFromSession(path) {
  return latestUsageFromTail(path, claudeSessionUsage);
}

function latestUsageFromTail(path, extractUsage) {
  if (!path) {
    return null;
  }
  let descriptor;
  try {
    const size = statSync(path).size;
    descriptor = openSync(path, "r");
    let end = size;
    let leadingFragment = "";
    while (end > 0) {
      const start = Math.max(0, end - ROLLOUT_TAIL_CHUNK_BYTES);
      const length = end - start;
      const buffer = Buffer.alloc(length);
      const bytesRead = readSync(
        descriptor,
        buffer,
        0,
        length,
        start,
      );
      const lines = (
        buffer.subarray(0, bytesRead).toString("utf8") + leadingFragment
      ).split("\n");
      leadingFragment = lines.shift() ?? "";
      for (let index = lines.length - 1; index >= 0; index -= 1) {
        const usage = extractUsage(lines[index]);
        if (usage) {
          return usage;
        }
      }
      end = start;
    }
    return extractUsage(leadingFragment);
  } catch {
    return null;
  } finally {
    if (descriptor !== undefined) {
      closeSync(descriptor);
    }
  }
}

export function codexUsageDelta(usage, baseline) {
  if (!usage || !baseline) {
    return null;
  }
  const result = { ...usage };
  for (const field of [
    "inputTokens",
    "outputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "cacheWrite5mInputTokens",
    "cacheWrite1hInputTokens",
    "reasoningOutputTokens",
  ]) {
    const current = usage[field];
    const previous = baseline[field];
    if (current == null) {
      result[field] = null;
      continue;
    }
    if (previous == null) {
      result[field] = current;
      continue;
    }
    if (current < previous) {
      return null;
    }
    result[field] = current - previous;
  }
  return result;
}

// 중단된 턴은 결과 이벤트를 받기 전에 프로세스가 끝나 사용량이 비어 있다.
// CLI가 디스크에 남긴 세션 기록에서 마지막 사용량을 되살린다.
export function recoverInterruptedUsage(state) {
  if (state.usage || !state.externalSessionID) {
    return null;
  }
  if (![
    "codex",
    "claude",
    "antigravity",
  ].includes(state.character?.backend)) {
    return null;
  }
  const currentClaudePath = claudeSessionPath(
    state.workdir,
    state.externalSessionID,
  );
  let recorded;
  if (state.character?.backend === "codex") {
    recorded = latestCodexUsageFromRollout(
      findRolloutPath(state.externalSessionID),
    );
  } else if (state.character?.backend === "antigravity") {
    recorded = (
      state.antigravityUsageReader ?? antigravitySessionUsage
    )(state.externalSessionID);
  } else {
    recorded = latestClaudeUsageFromSession(
      claudeSessionFileMatches(
        currentClaudePath,
        state.externalSessionID,
      )
        ? currentClaudePath
        : findClaudeSessionPath(state.externalSessionID),
    );
  }
  if (!recorded) {
    return null;
  }
  const usage = usageForTurn(state, recorded);
  if (usage) {
    state.usage = usage;
  }
  return usage ?? null;
}

export function accumulatedUsage(current, delta) {
  if (!current) {
    return delta ? { ...delta } : null;
  }
  if (!delta) {
    return { ...current };
  }
  const result = { ...current };
  for (const field of [
    "inputTokens",
    "outputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "cacheWrite5mInputTokens",
    "cacheWrite1hInputTokens",
    "reasoningOutputTokens",
    "reportedCostUsd",
    "reportedSonnet5CostUsd",
  ]) {
    const left = current[field];
    const right = delta[field];
    result[field] = left == null && right == null
      ? null
      : (left ?? 0) + (right ?? 0);
  }
  for (const field of ["serviceTier", "speed", "inferenceGeo"]) {
    result[field] = delta[field] ?? current[field] ?? null;
  }
  return result;
}

function usageForTurn(state, usage) {
  const resumedCumulativeSession =
    (state.character?.backend === "codex" && state.resumedCodexSession) ||
    (
      state.character?.backend === "antigravity" &&
      state.resumedAntigravitySession
    );
  if (!resumedCumulativeSession) {
    return usage;
  }
  if (!state.usageBaseline) {
    return null;
  }
  return codexUsageDelta(usage, state.usageBaseline) ?? usage;
}

function codexRolloutUsage(line) {
  let record;
  try {
    record = JSON.parse(line);
  } catch {
    return null;
  }
  if (record?.payload?.type !== "token_count") {
    return null;
  }
  const usage = record.payload.info?.total_token_usage;
  const inputTokens = nonnegativeTokenCount(usage?.input_tokens);
  const outputTokens = nonnegativeTokenCount(usage?.output_tokens);
  if (inputTokens === null || outputTokens === null) {
    return null;
  }
  return {
    inputTokens,
    outputTokens,
    cachedInputTokens: nonnegativeTokenCount(
      usage.cached_input_tokens,
    ),
    cacheWriteInputTokens: nonnegativeTokenCount(
      usage.cache_write_input_tokens,
    ),
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: nonnegativeTokenCount(
      usage.reasoning_output_tokens,
    ),
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
}

function nonnegativeTokenCount(value) {
  return typeof value === "number" &&
      Number.isFinite(value) &&
      value >= 0
    ? value
    : null;
}

function emptyUsage() {
  return {
    inputTokens: null,
    outputTokens: null,
    cachedInputTokens: null,
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: null,
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
}

function rolloutFileChangeStatistics(reader, workdir, changes) {
  if (!reader || !readRolloutEvents(reader, workdir)) {
    return null;
  }
  const expectedPaths = normalizedChangePaths(workdir, changes);
  const matchIndex = reader.pending.findIndex((event) =>
    sameStringArrays(event.paths, expectedPaths)
  );
  if (matchIndex < 0) {
    return null;
  }
  const [event] = reader.pending.splice(matchIndex, 1);
  return event.statistics;
}

function rolloutCollaborationActivities(reader, workdir) {
  if (!reader || !readRolloutEvents(reader, workdir)) {
    return [];
  }
  reader.collaborationPending ??= [];
  return [...reader.collaborationPending];
}

function acknowledgeRolloutCollaborationActivity(reader, activity) {
  const pending = reader?.collaborationPending;
  if (!Array.isArray(pending)) {
    return;
  }
  const index = pending.indexOf(activity);
  if (index >= 0) {
    pending.splice(index, 1);
  }
}

function readRolloutEvents(reader, workdir) {
  let size;
  try {
    size = statSync(reader.path).size;
  } catch {
    return false;
  }
  if (size < reader.offset) {
    reader.offset = 0;
    reader.remainder = "";
    reader.pending = [];
    reader.collaborationPending = [];
    reader.collaborationTracker = new CodexRolloutCollaborationTracker();
  }
  if (size === reader.offset) {
    return true;
  }

  const length = size - reader.offset;
  const buffer = Buffer.alloc(length);
  let descriptor;
  let bytesRead = 0;
  try {
    descriptor = openSync(reader.path, "r");
    while (bytesRead < length) {
      const count = readSync(
        descriptor,
        buffer,
        bytesRead,
        length - bytesRead,
        reader.offset + bytesRead,
      );
      if (count <= 0) {
        break;
      }
      bytesRead += count;
    }
  } catch {
    return false;
  } finally {
    if (descriptor !== undefined) {
      closeSync(descriptor);
    }
  }
  if (bytesRead === 0) {
    return false;
  }
  reader.offset += bytesRead;
  const lines = (
    reader.remainder + buffer.subarray(0, bytesRead).toString("utf8")
  ).split("\n");
  reader.remainder = lines.pop() ?? "";
  reader.collaborationPending ??= [];
  reader.collaborationTracker ??=
    new CodexRolloutCollaborationTracker();
  for (const line of lines) {
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    const payload = record?.payload;
    reader.collaborationPending.push(
      ...reader.collaborationTracker.consume(record),
    );
    if (
      payload?.type !== "patch_apply_end" ||
      payload.status !== "completed" ||
      payload.success === false ||
      !payload.changes ||
      typeof payload.changes !== "object"
    ) {
      continue;
    }
    const paths = normalizedChangePaths(
      workdir,
      Object.keys(payload.changes).map((path) => ({ path })),
    );
    let additions = 0;
    let deletions = 0;
    let isComplete = paths.length > 0;
    for (const change of Object.values(payload.changes)) {
      const statistics = unifiedDiffStatistics(change?.unified_diff);
      if (!statistics) {
        isComplete = false;
        break;
      }
      additions += statistics.additions;
      deletions += statistics.deletions;
    }
    if (isComplete) {
      reader.pending.push({
        paths,
        statistics: { additions, deletions },
      });
    }
  }
  return true;
}

function unifiedDiffStatistics(value) {
  if (typeof value !== "string" || !value) {
    return null;
  }
  let additions = 0;
  let deletions = 0;
  let hasHunk = false;
  let isInsideHunk = false;
  for (const line of value.replaceAll("\r\n", "\n").split("\n")) {
    if (line.startsWith("@@")) {
      hasHunk = true;
      isInsideHunk = true;
      continue;
    }
    if (!isInsideHunk) {
      continue;
    }
    if (line.startsWith("+")) {
      additions += 1;
    } else if (line.startsWith("-")) {
      deletions += 1;
    }
  }
  return hasHunk ? { additions, deletions } : null;
}

function normalizedChangePaths(workdir, changes) {
  return [...new Set(
    (changes ?? [])
      .map((change) => resolvedChangePath(workdir, change?.path))
      .filter(Boolean),
  )].sort();
}

function sameStringArrays(left, right) {
  return left.length === right.length &&
    left.every((value, index) => value === right[index]);
}

function readFileSnapshot(path, byteLimit) {
  try {
    const metadata = statSync(path);
    if (
      !metadata.isFile() ||
      metadata.size > MAX_FILE_SNAPSHOT_BYTES ||
      metadata.size > byteLimit
    ) {
      return null;
    }
    return readFileSync(path);
  } catch (error) {
    return error?.code === "ENOENT" ? Buffer.alloc(0) : null;
  }
}

function resolvedChangePath(workdir, value) {
  const path = String(value ?? "").trim();
  if (!path) {
    return null;
  }
  return isAbsolute(path) ? path : resolve(workdir, path);
}

export function claudeSessionPath(workdir, sessionID) {
  const directory = String(workdir ?? "").trim();
  const id = String(sessionID ?? "").trim();
  if (!directory || !id) {
    return null;
  }
  let resolved;
  try {
    resolved = realpathSync(directory);
  } catch {
    resolved = directory;
  }
  return join(
    homedir(),
    ".claude",
    "projects",
    resolved.replace(/[/.]/g, "-"),
    `${id}.jsonl`,
  );
}

export function claudeSessionResumable(workdir, sessionID) {
  const path = claudeSessionPath(workdir, sessionID);
  return path === null ? false : existsSync(path);
}

export function findClaudeSessionPath(sessionID) {
  return claudeSessionCandidates(sessionID).at(0)?.path ?? null;
}

function claudeSessionCandidates(sessionID) {
  const id = String(sessionID ?? "").trim();
  if (!id) {
    return [];
  }
  const root = join(homedir(), ".claude", "projects");
  if (!existsSync(root)) {
    return [];
  }
  let directories;
  try {
    directories = readdirSync(root);
  } catch {
    return [];
  }
  const candidates = [];
  for (const directory of directories) {
    const candidate = join(root, String(directory), `${id}.jsonl`);
    try {
      const metadata = statSync(candidate);
      if (!metadata.isFile()) {
        continue;
      }
      candidates.push({
        path: candidate,
        size: metadata.size,
        modifiedAt: metadata.mtimeMs,
        messageAt: latestClaudeMessageTimestamp(candidate),
      });
    } catch {
      // 다른 작업 공간이 동시에 정리한 후보는 건너뛴다.
    }
  }
  return candidates.sort((left, right) =>
    (right.messageAt ?? -Infinity) - (left.messageAt ?? -Infinity) ||
    right.modifiedAt - left.modifiedAt ||
    right.size - left.size ||
    left.path.localeCompare(right.path)
  );
}

export function prepareClaudeSessionResume({
  sessionID,
  workdir,
  previousWorkdir = null,
}) {
  const target = claudeSessionPath(workdir, sessionID);
  if (!target) {
    return null;
  }
  const preferred = claudeSessionPath(previousWorkdir, sessionID);
  const source = claudeSessionFileMatches(preferred, sessionID)
    ? preferred
    : findClaudeSessionPath(sessionID);
  if (!source) {
    throw new Error(
      "저장된 Claude Code 세션 기록을 찾을 수 없습니다. " +
      "대화 이력을 버리고 새 세션으로 바꾸지 않았습니다.",
    );
  }
  if (source === target) {
    return source;
  }

  if (existsSync(target)) {
    if (!sameFileIdentity(source, target)) {
      throw new Error(
        "현재 작업 공간에 서로 다른 Claude Code 세션 기록이 이미 있습니다. " +
        "기존 기록을 덮어쓰거나 숨기지 않았습니다.",
      );
    }
  }

  const sidecar = prepareClaudeSessionSidecar(source, target, sessionID);
  if (existsSync(target)) {
    return target;
  }
  mkdirSync(dirname(target), { recursive: true });
  try {
    linkSync(source, target);
  } catch (error) {
    if (error?.code === "EEXIST" && sameFileIdentity(source, target)) {
      return target;
    }
    if (sidecar.created) {
      try {
        unlinkSync(sidecar.path);
      } catch {
        // 생성한 alias만 최선으로 되돌리고 원래 파일은 건드리지 않는다.
      }
    }
    throw error;
  }
  return target;
}

function prepareClaudeSessionSidecar(source, target, sessionID) {
  const sourcePath = join(dirname(source), sessionID);
  if (!pathEntryExists(sourcePath)) {
    return { created: false, path: null };
  }
  const sourceMetadata = statSync(sourcePath);
  if (!sourceMetadata.isDirectory()) {
    throw new Error("Claude Code 세션 부속 기록 경로가 디렉토리가 아닙니다.");
  }

  const targetPath = join(dirname(target), sessionID);
  if (pathEntryExists(targetPath)) {
    if (sameRealPath(targetPath, sourcePath)) {
      return { created: false, path: targetPath };
    }
    throw new Error(
      "현재 작업 공간에 서로 다른 Claude Code 세션 부속 기록이 있습니다. " +
      "기존 기록을 덮어쓰거나 숨기지 않았습니다.",
    );
  }

  mkdirSync(dirname(targetPath), { recursive: true });
  const resolvedSource = realpathSync(sourcePath);
  try {
    symlinkSync(resolvedSource, targetPath, "dir");
  } catch (error) {
    if (
      error?.code === "EEXIST" &&
      sameRealPath(targetPath, resolvedSource)
    ) {
      return { created: false, path: targetPath };
    }
    throw error;
  }
  return { created: true, path: targetPath };
}

function pathEntryExists(path) {
  try {
    lstatSync(path);
    return true;
  } catch {
    return false;
  }
}

function sameRealPath(left, right) {
  try {
    return realpathSync(left) === realpathSync(right);
  } catch {
    return false;
  }
}

function latestClaudeMessageTimestamp(path) {
  const maxBytes = 4 * 1024 * 1024;
  let descriptor;
  try {
    const size = statSync(path).size;
    const offset = Math.max(0, size - maxBytes);
    const length = size - offset;
    descriptor = openSync(path, "r");
    const buffer = Buffer.alloc(length);
    const bytesRead = readSync(descriptor, buffer, 0, length, offset);
    const lines = buffer.subarray(0, bytesRead).toString("utf8").split("\n");
    if (offset > 0) {
      lines.shift();
    }
    for (let index = lines.length - 1; index >= 0; index -= 1) {
      const line = lines[index];
      if (
        !line.includes('"timestamp"') ||
        (!line.includes('"type":"assistant"') &&
          !line.includes('"type":"user"'))
      ) {
        continue;
      }
      let event;
      try {
        event = JSON.parse(line);
      } catch {
        continue;
      }
      if (event.isSidechain === true) {
        continue;
      }
      const at = Date.parse(event.timestamp);
      if (Number.isFinite(at)) {
        return at;
      }
    }
  } catch {
    return null;
  } finally {
    if (descriptor !== undefined) {
      closeSync(descriptor);
    }
  }
  return null;
}

function claudeSessionFileMatches(path, sessionID) {
  if (!path) {
    return false;
  }
  let descriptor;
  try {
    const metadata = statSync(path);
    if (!metadata.isFile()) {
      return false;
    }
    const length = Math.min(metadata.size, 64 * 1024);
    descriptor = openSync(path, "r");
    const buffer = Buffer.alloc(length);
    const bytesRead = readSync(descriptor, buffer, 0, length, 0);
    const prefix = buffer.subarray(0, bytesRead).toString("utf8");
    return prefix.includes(`"sessionId":"${sessionID}"`) ||
      prefix.includes(`"session_id":"${sessionID}"`);
  } catch {
    return false;
  } finally {
    if (descriptor !== undefined) {
      closeSync(descriptor);
    }
  }
}

function sameFileIdentity(left, right) {
  try {
    const leftMetadata = statSync(left);
    const rightMetadata = statSync(right);
    return leftMetadata.dev === rightMetadata.dev &&
      leftMetadata.ino === rightMetadata.ino;
  } catch {
    return false;
  }
}

export function buildArguments({
  character,
  prompt,
  previousSessionID,
  attachments = [],
  workdir = null,
}) {
  switch (character.backend) {
    case "codex":
      return codexArguments(
        character,
        prompt,
        previousSessionID,
        attachments,
      );
    case "claude":
      return claudeArguments(
        character,
        prompt,
        previousSessionID,
      );
    case "antigravity":
      return antigravityArguments(
        character,
        prompt,
        previousSessionID,
        workdir,
      );
    default:
      throw new Error(`지원하지 않는 직원 백엔드입니다: ${character.backend}`);
  }
}

export function executionEnvironment(
  character,
  baseEnvironment = process.env,
  { workdir = null } = {},
) {
  let environment;
  if (character.backend === "antigravity") {
    environment = antigravityPlaywrightEnvironment(baseEnvironment);
  } else if (character.backend === "claude") {
    environment = { ...baseEnvironment };
    delete environment.CLAUDE_CODE_AUTO_COMPACT_WINDOW;
    environment.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE = String(
      normalizeAutoCompactPercent(character.autoCompactPercent),
    );
    // CLI 갱신은 OFFICESTRA의 명시적 업데이트 화면에서만 수행한다.
    // 업무 사이에 Claude가 바뀌면 긴 재개 세션의 프롬프트 캐시가 한 번
    // 전부 무효화될 수 있으므로 자식 프로세스의 자동 갱신은 끈다.
    environment.DISABLE_AUTOUPDATER = "1";
  } else {
    environment = baseEnvironment;
  }
  if (!workdir) {
    return environment;
  }
  environment = { ...environment };
  environment[STRUCTURED_RESULT_ENV] = structuredTurnResultPath({
    workdir,
    characterID: character.id,
  });
  environment.PATH = prependPathEntries(environment.PATH, [
    structuredResultToolDirectory,
    dirname(process.execPath),
  ]);
  return environment;
}

function prependPathEntries(value, entries) {
  const paths = [
    ...entries,
    ...String(value ?? "").split(delimiter),
  ].filter(Boolean);
  return [...new Set(paths)].join(delimiter);
}

export function configuredExecutableForCharacter(character) {
  const configured = character.config?.executablePath;
  if (!configured || !existsSync(configured)) {
    return null;
  }
  const configuredName = basename(configured).toLowerCase();
  const expectedName = backendExecutableName(character.backend).toLowerCase();
  if (configuredName !== expectedName) {
    return null;
  }
  try {
    if (!statSync(configured).isFile()) {
      return null;
    }
    accessSync(configured, constants.X_OK);
    return configured;
  } catch {
    return null;
  }
}

export function locateExecutable(character) {
  const configured = configuredExecutableForCharacter(character);
  if (configured) return configured;

  const home = homedir();
  const executableName = backendExecutableName(character.backend);
  const candidates = [
    join(home, ".local", "bin", executableName),
    join("/opt/homebrew/bin", executableName),
    join("/usr/local/bin", executableName),
  ];
  if (character.backend === "claude") {
    const versionsDirectory = join(home, ".nvm", "versions", "node");
    if (existsSync(versionsDirectory)) {
      const versionCandidates = readdirSync(versionsDirectory)
        .sort()
        .reverse()
        .map((version) =>
          join(versionsDirectory, version, "bin", "claude")
        );
      candidates.unshift(...versionCandidates);
    }
  }

  return candidates.find(existsSync) ?? executableName;
}

function codexArguments(
  character,
  prompt,
  previousSessionID,
  attachments,
) {
  const argumentsList = ["exec"];
  if (previousSessionID) {
    argumentsList.push("resume", previousSessionID, "--json");
  } else {
    argumentsList.push("--json", "--skip-git-repo-check");
  }
  if (character.model) {
    argumentsList.push("-c", `model="${character.model}"`);
  }
  argumentsList.push(
    "-c",
    `model_reasoning_effort="${character.effort}"`,
    "-c",
    "features.fast_mode=true",
    "-c",
    `service_tier="${character.fastMode ? "fast" : "default"}"`,
    "-c",
    'model_reasoning_summary="detailed"',
    "-c",
    "show_raw_agent_reasoning=true",
    "-c",
    `developer_instructions=${JSON.stringify(configuredIdentityPrompt(character))}`,
  );
  if (previousSessionID) {
    argumentsList.push(
      "-c",
      `sandbox_mode="${character.permission}"`,
    );
  } else {
    argumentsList.push("-s", character.permission);
  }
  for (const attachment of attachments) {
    if (attachment.isCodexImage) {
      argumentsList.push("-i", attachment.path);
    }
  }
  argumentsList.push(prompt);
  return argumentsList;
}

function claudeArguments(
  character,
  prompt,
  previousSessionID,
) {
  const argumentsList = [
    "-p",
    prompt,
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--prompt-suggestions",
    "true",
    "--settings",
    JSON.stringify({ fastMode: character.fastMode === true }),
    "--effort",
    character.effort,
    "--permission-mode",
    character.permission,
  ];
  if (character.model) {
    argumentsList.push("--model", character.model);
  }
  argumentsList.push(
    "--append-system-prompt",
    configuredIdentityPrompt(character),
  );
  // 실행 전에 같은 transcript inode를 현재 worktree에 연결한다. 과거처럼
  // JSONL을 복제하지 않으므로 세션 분기와 캐시 접두부 재생성을 막는다.
  if (previousSessionID) {
    argumentsList.push("--resume", previousSessionID);
  }
  return argumentsList;
}

function antigravityArguments(
  character,
  prompt,
  previousSessionID,
  workdir,
) {
  const argumentsList = [
    "-p",
    antigravityExecutionPrompt(character, prompt, workdir),
    "--output-format",
    "stream-json",
    "--print-timeout",
    "24h",
    "--model",
    character.model,
    "--effort",
    character.effort,
  ];
  if (previousSessionID) {
    argumentsList.push("--conversation", previousSessionID);
  }
  // Headless 모드는 --add-dir로 등록된 폴더만 읽기 권한을 자동 승인한다.
  // cwd만 맞추면 plan 모드의 view_file도 승인 대기 상태로 끝날 수 있다.
  if (workdir) {
    argumentsList.push("--add-dir", workdir);
  }
  switch (character.permission) {
    case "plan":
      argumentsList.push("--mode", "plan");
      break;
    case "accept-edits":
      argumentsList.push(
        "--mode",
        "accept-edits",
        "--sandbox",
        "--dangerously-skip-permissions",
      );
      break;
    case "dangerously-skip-permissions":
      argumentsList.push(
        "--mode",
        "accept-edits",
        "--dangerously-skip-permissions",
      );
      break;
    default:
      throw new Error(
        `지원하지 않는 Antigravity 권한입니다: ${character.permission}`,
      );
  }
  return argumentsList;
}

function antigravityExecutionPrompt(character, prompt, workdir) {
  const resolvedWorkdir = String(workdir ?? "").trim();
  return [
    "<officestra_identity>",
    configuredIdentityPrompt(character),
    "</officestra_identity>",
    "<officestra_execution>",
    resolvedWorkdir
      ? `업무 폴더는 ${JSON.stringify(resolvedWorkdir)}입니다.`
      : "현재 업무 폴더 안에서만 파일 작업을 수행합니다.",
    resolvedWorkdir
      ? `명령을 실행할 때는 반드시 ${JSON.stringify(resolvedWorkdir)}로 이동한 뒤 실행합니다.`
      : null,
    "사용자 요청 범위를 벗어난 파일이나 서비스를 변경하지 않습니다.",
    "</officestra_execution>",
    "<user_request>",
    String(prompt ?? ""),
    "</user_request>",
  ].filter((line) => line !== null).join("\n");
}

export function claudePersistentArguments(character, previousSessionID) {
  const argumentsList = [
    "-p",
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--prompt-suggestions",
    "true",
    "--settings",
    JSON.stringify({ fastMode: character.fastMode === true }),
    "--effort",
    character.effort,
    "--permission-mode",
    character.permission,
  ];
  if (character.model) {
    argumentsList.push("--model", character.model);
  }
  argumentsList.push(
    "--append-system-prompt",
    configuredIdentityPrompt(character),
  );
  if (previousSessionID) {
    argumentsList.push("--resume", previousSessionID);
  }
  return argumentsList;
}

export function claudePersistentWorkerSignature({
  character,
  executable,
  workdir,
}) {
  return JSON.stringify({
    executable: String(executable ?? ""),
    workdir: String(workdir ?? ""),
    model: String(character.model ?? ""),
    effort: String(character.effort ?? ""),
    fastMode: character.fastMode === true,
    permission: String(character.permission ?? ""),
    autoCompactPercent: normalizeAutoCompactPercent(
      character.autoCompactPercent,
    ),
    identityPrompt: configuredIdentityPrompt(character),
  });
}

function configuredIdentityPrompt(character) {
  return identityPromptWithStructuredResult(character.identityPrompt, character.id);
}

export function promptWithAttachments(prompt, attachments) {
  if (attachments.length === 0) {
    return prompt;
  }
  const references = attachments.map(
    (attachment) =>
      `- ${JSON.stringify(attachment.name)}: ` +
      JSON.stringify(attachment.path),
  );
  return [
    prompt,
    "",
    "첨부 파일",
    "다음 로컬 파일을 업무 자료로 사용하세요.",
    ...references,
  ].join("\n");
}

export function stageAttachments({
  attachmentPaths,
  workdir,
}) {
  if (!Array.isArray(attachmentPaths)) {
    throw new Error("첨부 파일 목록이 올바르지 않습니다.");
  }
  const uniquePaths = [
    ...new Set(
      attachmentPaths.map((path) => String(path ?? "").trim()),
    ),
  ].filter(Boolean);
  if (uniquePaths.length > 20) {
    throw new Error("첨부 파일은 한 번에 20개까지 선택할 수 있습니다.");
  }
  if (uniquePaths.length === 0) {
    return [];
  }

  const directory = join(workdir, ".office-attachments", randomUUID());
  mkdirSync(directory, { recursive: true });

  try {
    return uniquePaths.map((path, index) => {
      const sourcePath = realpathSync(path);
      if (!statSync(sourcePath).isFile()) {
        throw new Error(`파일만 첨부할 수 있습니다. ${path}`);
      }
      const name = basename(sourcePath);
      const stagedPath = join(
        directory,
        `${String(index + 1).padStart(2, "0")}-${name}`,
      );
      copyFileSync(sourcePath, stagedPath);
      return {
        name,
        path: stagedPath,
        directory,
        isCodexImage: [".png", ".jpg", ".jpeg"].includes(
          extname(name).toLowerCase(),
        ),
      };
    });
  } catch (error) {
    rmSync(directory, { recursive: true, force: true });
    throw error;
  }
}

function removeStagedAttachments(attachments) {
  const directory = attachments[0]?.directory;
  if (directory) {
    rmSync(directory, { recursive: true, force: true });
  }
}

function terminateProcessGroup(child) {
  if (!child || child.exitCode !== null || !child.pid) {
    return;
  }
  const processID =
    process.platform === "win32" ? child.pid : -child.pid;
  try {
    if (process.platform === "win32") {
      child.kill("SIGTERM");
    } else {
      process.kill(processID, "SIGTERM");
    }
  } catch {
    return;
  }

  const forceTermination = setTimeout(() => {
    if (child.exitCode !== null) {
      return;
    }
    try {
      if (process.platform === "win32") {
        child.kill("SIGKILL");
      } else {
        process.kill(processID, "SIGKILL");
      }
    } catch {
      // 이미 종료된 프로세스는 추가 조치가 필요 없다.
    }
  }, 1_500);
  forceTermination.unref();
}

async function collectStream(stream) {
  let output = "";
  for await (const chunk of stream) {
    output += chunk.toString("utf8");
    if (output.length > 200_000) {
      output = output.slice(-200_000);
    }
  }
  return output;
}
