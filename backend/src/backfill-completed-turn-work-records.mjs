// 이 파일은 전환 공백의 완료 턴을 작업 기록으로 복원하고 파생 RAG를 재색인한다.

import { fileURLToPath } from "node:url";

import { pool, withTransaction } from "./db.mjs";
import {
  persistCompletedTurnWorkRecord,
  syncWorkRecordRAGDocuments,
  transitionTurnWorkRecordReview,
} from "./work-record-memory.mjs";

export const BACKFILL_COMPLETED_TURNS_USAGE = [
  "사용법",
  "node src/backfill-completed-turn-work-records.mjs --dry-run|--apply",
  "  --repository-root <절대경로> --from <포함 ISO 시각>",
  "  --before <미포함 ISO 시각> --expected-count <건수>",
].join("\n");

const MODE_ARGUMENTS = new Map([
  ["--dry-run", "dry-run"],
  ["--apply", "apply"],
]);

function requiredOption(options, name, description) {
  const value = String(options[name] ?? "").trim();
  if (!value) {
    throw new Error(`${description} 옵션이 필요합니다.`);
  }
  return value;
}

function validateTimestamp(value, description) {
  if (!value.includes("T") || Number.isNaN(Date.parse(value))) {
    throw new Error(`${description}은 ISO 시각이어야 합니다.`);
  }
  return value;
}

export function parseBackfillCompletedTurnArguments(argumentsList) {
  if (
    argumentsList.length === 1 &&
    (argumentsList[0] === "--help" || argumentsList[0] === "-h")
  ) {
    return { help: true };
  }
  if (argumentsList.includes("--help") || argumentsList.includes("-h")) {
    throw new Error("도움말은 다른 옵션과 함께 사용할 수 없습니다.");
  }

  let mode = null;
  const options = {};
  for (let index = 0; index < argumentsList.length; index += 1) {
    const argument = argumentsList[index];
    if (MODE_ARGUMENTS.has(argument)) {
      if (mode !== null) {
        throw new Error("--dry-run과 --apply 중 하나만 지정하세요.");
      }
      mode = MODE_ARGUMENTS.get(argument);
      continue;
    }
    const optionNames = new Map([
      ["--repository-root", "repositoryRoot"],
      ["--from", "from"],
      ["--before", "before"],
      ["--expected-count", "expectedCount"],
    ]);
    const optionName = optionNames.get(argument);
    if (!optionName) {
      throw new Error(`알 수 없는 옵션입니다. ${argument}`);
    }
    if (Object.hasOwn(options, optionName)) {
      throw new Error(`${argument} 옵션을 중복 지정할 수 없습니다.`);
    }
    const value = argumentsList[index + 1];
    if (!value || value.startsWith("--")) {
      throw new Error(`${argument} 뒤에 값을 입력하세요.`);
    }
    options[optionName] = value;
    index += 1;
  }

  if (!mode) {
    throw new Error("--dry-run 또는 --apply 확인이 필요합니다.");
  }
  const repositoryRoot = requiredOption(
    options,
    "repositoryRoot",
    "--repository-root",
  );
  if (!repositoryRoot.startsWith("/")) {
    throw new Error("--repository-root는 절대경로여야 합니다.");
  }
  const from = validateTimestamp(
    requiredOption(options, "from", "--from"),
    "--from",
  );
  const before = validateTimestamp(
    requiredOption(options, "before", "--before"),
    "--before",
  );
  if (Date.parse(from) >= Date.parse(before)) {
    throw new Error("--from은 --before보다 앞선 시각이어야 합니다.");
  }
  const expectedText = requiredOption(
    options,
    "expectedCount",
    "--expected-count",
  );
  const expectedCount = Number(expectedText);
  if (!Number.isSafeInteger(expectedCount) || expectedCount < 1) {
    throw new Error("--expected-count는 1 이상의 정수여야 합니다.");
  }
  return {
    help: false,
    mode,
    repositoryRoot,
    from,
    before,
    expectedCount,
  };
}

export function restoredReviewStatus(candidate) {
  if (candidate.needsInput === true) {
    return "needs_input";
  }
  if (!candidate.workspaceID) {
    return "not_applicable";
  }
  if (candidate.reviewTurnID !== candidate.turnID) {
    return "not_required";
  }
  const changedFiles = Array.isArray(candidate.changedFiles)
    ? candidate.changedFiles
    : [];
  if (
    changedFiles.length > 0 ||
    [
      "awaiting_approval",
      "merging",
      "merged",
      "rejected",
      "conflict",
    ].includes(candidate.workspaceStatus)
  ) {
    return "awaiting_approval";
  }
  return "not_required";
}

export async function listMissingCompletedTurnWorkRecords(client, {
  repositoryRoot,
  from,
  before,
}) {
  const result = await client.query(
    `
      SELECT
        turn.id::text AS "turnID",
        turn.prompt,
        turn.ended_at::text AS "recordedAt",
        turn.backend,
        COALESCE(turn.resolved_model, turn.model) AS model,
        turn.needs_input AS "needsInput",
        turn.response_source_warning AS "responseSourceWarning",
        session.character_id AS "characterID",
        workspace.id::text AS "workspaceID",
        workspace.status AS "workspaceStatus",
        workspace.review_turn_id::text AS "reviewTurnID",
        workspace.review_tree AS "reviewTree",
        workspace.head_commit AS "headCommit",
        workspace.changed_files AS "changedFiles",
        workspace.task_commit AS "taskCommit",
        workspace.merged_commit AS "mergedCommit",
        workspace.merged_at::text AS "mergedAt",
        workspace.rejected_at::text AS "rejectedAt",
        final_message.text AS response,
        COALESCE(response_sources.count, 0)::integer
          AS "responseSourceCount"
      FROM turns AS turn
      JOIN cli_sessions AS session ON session.id = turn.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = session.conversation_id
      LEFT JOIN task_workspaces AS workspace
        ON workspace.id = turn.task_workspace_id
      LEFT JOIN LATERAL (
        SELECT activity.text
        FROM turn_activities AS activity
        WHERE activity.turn_id = turn.id
          AND activity.kind = 'message'
          AND activity.status = 'completed'
        ORDER BY activity.seq DESC, activity.id DESC
        LIMIT 1
      ) AS final_message ON true
      LEFT JOIN LATERAL (
        SELECT count(*) AS count
        FROM turn_response_sources AS source
        WHERE source.turn_id = turn.id
      ) AS response_sources ON true
      WHERE turn.status = 'completed'
        AND turn.ended_at >= $2::timestamptz
        AND turn.ended_at < $3::timestamptz
        AND COALESCE(workspace.repository_root, conversation.workdir) = $1
        AND NOT EXISTS (
          SELECT 1
          FROM work_records AS record
          WHERE record.source_turn_id = turn.id
            AND record.record_type = 'result'
            AND record.import_id IS NULL
        )
      ORDER BY turn.ended_at, turn.id
    `,
    [repositoryRoot, from, before],
  );
  return result.rows ?? [];
}

function validateCandidate(candidate) {
  const requiredValues = [
    [candidate.turnID, "턴 ID"],
    [candidate.characterID, "직원 ID"],
    [candidate.prompt, "사용자 요청"],
    [candidate.recordedAt, "완료 시각"],
    [candidate.response, "최종 공개 메시지"],
  ];
  for (const [value, description] of requiredValues) {
    if (!String(value ?? "").trim()) {
      throw new Error(
        `${candidate.turnID ?? "알 수 없는 턴"}의 ${description} 값이 없습니다.`,
      );
    }
  }
}

export async function backfillCompletedTurnWorkRecords(client, {
  repositoryRoot,
  from,
  before,
  expectedCount,
}) {
  await client.query(
    "SELECT pg_advisory_xact_lock(hashtext($1))",
    [`officestra:completed-turn-backfill:${repositoryRoot}:${from}:${before}`],
  );
  const candidates = await listMissingCompletedTurnWorkRecords(client, {
    repositoryRoot,
    from,
    before,
  });
  if (candidates.length !== expectedCount && candidates.length !== 0) {
    throw new Error(
      `백필 후보가 예상 ${expectedCount}개와 다릅니다. 실제 ${candidates.length}개`,
    );
  }
  for (const candidate of candidates) {
    validateCandidate(candidate);
  }

  const workRecordIDs = [];
  let transitioned = 0;
  for (const candidate of candidates) {
    const isReviewTurn = candidate.reviewTurnID === candidate.turnID;
    const changedFiles = isReviewTurn && Array.isArray(candidate.changedFiles)
      ? candidate.changedFiles
      : [];
    const stored = await persistCompletedTurnWorkRecord(client, {
      repositoryRoot,
      turnID: candidate.turnID,
      workspaceID: candidate.workspaceID,
      characterID: candidate.characterID,
      prompt: candidate.prompt,
      response: candidate.response,
      backend: candidate.backend,
      model: candidate.model,
      needsInput: candidate.needsInput,
      responseSourceCount: candidate.responseSourceCount,
      responseSourceWarning: candidate.responseSourceWarning,
      reviewStatus: restoredReviewStatus(candidate),
      reviewTree: isReviewTurn ? candidate.reviewTree : null,
      headCommit: isReviewTurn ? candidate.headCommit : null,
      changedFiles,
      recordedAt: candidate.recordedAt,
    });
    if (!stored?.workRecordId) {
      throw new Error(`${candidate.turnID}의 작업 기록을 저장하지 못했습니다.`);
    }
    workRecordIDs.push(stored.workRecordId);

    if (
      isReviewTurn &&
      ["merged", "rejected", "conflict"].includes(
        candidate.workspaceStatus,
      )
    ) {
      const occurredAt = candidate.workspaceStatus === "merged"
        ? candidate.mergedAt
        : candidate.workspaceStatus === "rejected"
        ? candidate.rejectedAt
        : null;
      const transitionedID = await transitionTurnWorkRecordReview(client, {
        turnID: candidate.turnID,
        status: candidate.workspaceStatus,
        taskCommit: candidate.taskCommit,
        mergedCommit: candidate.mergedCommit,
        reviewTree: candidate.reviewTree,
        headCommit: candidate.headCommit,
        changedFiles,
        actorType: "system",
        occurredAt,
      });
      if (!transitionedID) {
        throw new Error(`${candidate.turnID}의 검토 상태를 복원하지 못했습니다.`);
      }
      transitioned += 1;
    }
  }
  return {
    backfilled: candidates.length,
    transitioned,
    turnIDs: candidates.map((candidate) => candidate.turnID),
    workRecordIDs,
  };
}

export async function auditCompletedTurnWorkRecords(client, {
  repositoryRoot,
}) {
  const result = await client.query(
    `
      WITH scoped_records AS (
        SELECT record.id, record.source_turn_id
        FROM work_records AS record
        JOIN projects AS project ON project.id = record.project_id
        WHERE project.repository_root = $1
      ), scoped_searchable AS (
        SELECT scoped.id
        FROM scoped_records AS scoped
        JOIN searchable_work_record_ids AS searchable
          ON searchable.id = scoped.id
      ), scoped_documents AS (
        SELECT document.id, document.work_record_id
        FROM rag_documents AS document
        JOIN scoped_records AS scoped
          ON scoped.id = document.work_record_id
      )
      SELECT
        (SELECT count(*)::integer FROM scoped_records)
          AS "workRecordCount",
        (SELECT count(*)::integer FROM scoped_searchable)
          AS "searchableRecordCount",
        (SELECT count(*)::integer FROM scoped_documents)
          AS "ragDocumentCount",
        (SELECT count(*)::integer
         FROM scoped_searchable AS searchable
         WHERE NOT EXISTS (
           SELECT 1 FROM scoped_documents AS document
           WHERE document.work_record_id = searchable.id
         )) AS "missingRagCount",
        (SELECT count(*)::integer
         FROM scoped_documents AS document
         WHERE NOT EXISTS (
           SELECT 1 FROM scoped_searchable AS searchable
           WHERE searchable.id = document.work_record_id
         )) AS "staleRagCount",
        (SELECT count(*)::integer
         FROM (
           SELECT source_turn_id
           FROM scoped_records
           WHERE source_turn_id IS NOT NULL
           GROUP BY source_turn_id
           HAVING count(*) > 1
         ) AS duplicate) AS "duplicateTurnCount"
    `,
    [repositoryRoot],
  );
  return result.rows?.[0] ?? null;
}

async function runCLI() {
  const options = parseBackfillCompletedTurnArguments(process.argv.slice(2));
  if (options.help) {
    console.log(BACKFILL_COMPLETED_TURNS_USAGE);
    return;
  }
  if (options.mode === "dry-run") {
    const candidates = await listMissingCompletedTurnWorkRecords(pool, options);
    if (
      candidates.length !== options.expectedCount &&
      candidates.length !== 0
    ) {
      throw new Error(
        `백필 후보가 예상 ${options.expectedCount}개와 다릅니다. 실제 ${candidates.length}개`,
      );
    }
    console.log(JSON.stringify({
      mode: options.mode,
      candidates: candidates.length,
      turnIDs: candidates.map((candidate) => candidate.turnID),
    }));
    return;
  }

  const backfill = await withTransaction((client) =>
    backfillCompletedTurnWorkRecords(client, options));
  const rag = await withTransaction((client) =>
    syncWorkRecordRAGDocuments(client, {
      repositoryRoot: options.repositoryRoot,
    }));
  const remaining = await listMissingCompletedTurnWorkRecords(pool, options);
  const audit = await auditCompletedTurnWorkRecords(pool, options);
  if (
    remaining.length > 0 ||
    audit?.missingRagCount !== 0 ||
    audit?.staleRagCount !== 0 ||
    audit?.duplicateTurnCount !== 0
  ) {
    throw new Error("백필 후 무결성 검증을 통과하지 못했습니다.");
  }
  console.log(JSON.stringify({
    mode: options.mode,
    ...backfill,
    ragChanged: rag.changed,
    ragDeleted: rag.deleted,
    remainingMissing: remaining.length,
    audit,
  }));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    await runCLI();
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    console.error(BACKFILL_COMPLETED_TURNS_USAGE);
    process.exitCode = 1;
  } finally {
    await pool.end();
  }
}
