// 공통 프로젝트·티켓 데이터 계층. 배정은 실행 요청과 독립적이다.

export class WorkBoardError extends Error {
  constructor(message, status = 400) {
    super(message);
    this.name = "WorkBoardError";
    this.status = status;
  }
}

const states = new Set(["open", "in_progress", "review", "done", "canceled", "deferred"]);
const ticketTypes = new Set(["general", "planning", "implementation", "pretest",
  "verification", "research", "content", "operations"]);
const userReviews = new Set(["none", "approved", "changes_requested"]);
const verificationVerdicts = new Set(["pass", "fail"]);
const operationImpacts = new Set(["internal", "external"]);
const commitPattern = /^[0-9a-f]{40}$/;
const activityKinds = new Set(["progress", "decision", "verification"]);
const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const keyPattern = /^[a-z0-9-]{2,64}$/;

function object(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new WorkBoardError("JSON 객체가 필요합니다.");
  }
  return value;
}

function allowedFields(value, allowed) {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new WorkBoardError(`알 수 없는 필드: ${key}`);
  }
}

function text(value, field, max, required = false) {
  if (typeof value !== "string" || value.length > max || (required && !value.trim())) {
    throw new WorkBoardError(`${field} 값이 올바르지 않습니다.`);
  }
  return value.trim();
}

function uuid(value, field) {
  if (typeof value !== "string" || !uuidPattern.test(value)) {
    throw new WorkBoardError(`${field}는 UUID여야 합니다.`);
  }
  return value;
}

function dueDate(value) {
  if (value === null) return null;
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(value)) {
    throw new WorkBoardError("dueDate는 YYYY-MM-DD 또는 null이어야 합니다.");
  }
  const date = new Date(`${value}T00:00:00Z`);
  if (!Number.isFinite(date.getTime()) || date.toISOString().slice(0, 10) !== value) {
    throw new WorkBoardError("dueDate가 유효한 날짜가 아닙니다.");
  }
  return value;
}

function uuidList(value, field) {
  if (!Array.isArray(value) || value.length > 100) {
    throw new WorkBoardError(`${field}는 UUID 목록이어야 합니다.`);
  }
  const ids = value.map((item) => uuid(item, field));
  if (new Set(ids).size !== ids.length) throw new WorkBoardError(`${field}에 중복이 있습니다.`);
  return ids;
}

function decimalText(value, field, { positive = false } = {}) {
  if (value === null) return null;
  if (typeof value !== "string" || !/^\d{1,16}(?:\.\d{1,4})?$/.test(value)) {
    throw new WorkBoardError(`${field}는 십진수 문자열 또는 null이어야 합니다.`);
  }
  if (positive && /^0+(?:\.0+)?$/.test(value)) {
    throw new WorkBoardError(`${field}는 0보다 커야 합니다.`);
  }
  return value;
}

export function normalizeProjectInput(body, { patch = false } = {}) {
  const value = object(body);
  allowedFields(value, patch ? ["title", "description"] : ["projectKey", "title", "description"]);
  if (patch && Object.keys(value).length === 0) throw new WorkBoardError("수정할 필드가 없습니다.");
  const result = {};
  if (!patch) {
    if (typeof value.projectKey !== "string" || !keyPattern.test(value.projectKey)) {
      throw new WorkBoardError("projectKey는 소문자·숫자·하이픈 2~64자여야 합니다.");
    }
    result.projectKey = value.projectKey;
  }
  if (!patch || Object.hasOwn(value, "title")) result.title = text(value.title, "title", 120, true);
  if (!patch || Object.hasOwn(value, "description")) result.description = text(value.description ?? "", "description", 4000);
  return result;
}

export function normalizeTicketInput(body, { patch = false } = {}) {
  const value = object(body);
  const fields = ["title", "description", "assigneeId", "completedById", "verifiedById", "state", "dueDate",
    "completionCriteria", "parentTicketId", "predecessorIds", "goalIds", "workRecordIds",
    "decisionPending", "reportedActorId", "pushedCommitSha", "ticketType", "completionEvidence",
    "userReview", "userReviewNote", "targetTicketId", "verificationVerdict", "operationImpact",
    "resolutionReason"];
  allowedFields(value, patch ? fields : ["projectId", ...fields]);
  if (patch && Object.keys(value).length === 0) throw new WorkBoardError("수정할 필드가 없습니다.");
  const result = {};
  if (!patch) result.projectId = uuid(value.projectId, "projectId");
  if (!patch || Object.hasOwn(value, "title")) result.title = text(value.title, "title", 160, true);
  if (!patch || Object.hasOwn(value, "description")) result.description = text(value.description ?? "", "description", 10000);
  if (!patch || Object.hasOwn(value, "completionCriteria")) result.completionCriteria = text(value.completionCriteria ?? "", "completionCriteria", 4000);
  if (!patch || Object.hasOwn(value, "ticketType")) {
    const ticketType = value.ticketType ?? (!patch ? "general" : null);
    if (!ticketTypes.has(ticketType)) throw new WorkBoardError("ticketType이 올바르지 않습니다.");
    result.ticketType = ticketType;
  }
  for (const field of ["completionEvidence", "userReviewNote", "resolutionReason"]) {
    if (!patch || Object.hasOwn(value, field)) {
      result[field] = text(value[field] ?? "", field, field === "completionEvidence" ? 10000 : 4000);
    }
  }
  if (!patch || Object.hasOwn(value, "userReview")) {
    const review = value.userReview ?? (!patch ? "none" : null);
    if (!userReviews.has(review)) throw new WorkBoardError("userReview가 올바르지 않습니다.");
    result.userReview = review;
  }
  if (!patch || Object.hasOwn(value, "targetTicketId")) {
    result.targetTicketId = value.targetTicketId === null || value.targetTicketId === undefined
      ? null : uuid(value.targetTicketId, "targetTicketId");
  }
  if (!patch || Object.hasOwn(value, "verificationVerdict")) {
    const verdict = value.verificationVerdict ?? null;
    if (verdict !== null && !verificationVerdicts.has(verdict)) {
      throw new WorkBoardError("verificationVerdict가 올바르지 않습니다.");
    }
    result.verificationVerdict = verdict;
  }
  if (!patch || Object.hasOwn(value, "operationImpact")) {
    const impact = value.operationImpact ?? null;
    if (impact !== null && !operationImpacts.has(impact)) {
      throw new WorkBoardError("operationImpact가 올바르지 않습니다.");
    }
    result.operationImpact = impact;
  }
  if (!patch || Object.hasOwn(value, "assigneeId")) {
    result.assigneeId = value.assigneeId === null || value.assigneeId === undefined
      ? null : text(value.assigneeId, "assigneeId", 100, true);
  }
  for (const field of ["completedById", "verifiedById"]) {
    if (!patch || Object.hasOwn(value, field)) {
      result[field] = value[field] === null || value[field] === undefined
        ? null : text(value[field], field, 100, true);
    }
  }
  if (!patch || Object.hasOwn(value, "state")) {
    const state = !patch && value.state === undefined ? "open" : value.state;
    if (!states.has(state)) throw new WorkBoardError("state가 올바르지 않습니다.");
    result.state = state;
  }
  if (!patch || Object.hasOwn(value, "dueDate")) result.dueDate = dueDate(value.dueDate ?? null);
  if (!patch || Object.hasOwn(value, "pushedCommitSha")) {
    const sha = value.pushedCommitSha ?? null;
    if (sha !== null && (typeof sha !== "string" || !commitPattern.test(sha))) {
      throw new WorkBoardError("pushedCommitSha는 원격에 푸시된 40자리 소문자 커밋 SHA여야 합니다.");
    }
    result.pushedCommitSha = sha;
  }
  if (!patch || Object.hasOwn(value, "decisionPending")) {
    const pending = value.decisionPending ?? (!patch ? false : null);
    if (typeof pending !== "boolean") throw new WorkBoardError("decisionPending은 불리언이어야 합니다.");
    result.decisionPending = pending;
  }
  if (Object.hasOwn(value, "reportedActorId")) {
    result.reportedActorId = value.reportedActorId === null ? null
      : text(value.reportedActorId, "reportedActorId", 100, true);
  }
  if (!patch || Object.hasOwn(value, "parentTicketId")) {
    result.parentTicketId = value.parentTicketId === null || value.parentTicketId === undefined
      ? null : uuid(value.parentTicketId, "parentTicketId");
  }
  if (!patch || Object.hasOwn(value, "predecessorIds")) {
    result.predecessorIds = uuidList(
      value.predecessorIds === undefined && !patch ? [] : value.predecessorIds,
      "predecessorIds",
    );
  }
  if (!patch || Object.hasOwn(value, "goalIds")) {
    result.goalIds = uuidList(
      value.goalIds === undefined && !patch ? [] : value.goalIds,
      "goalIds",
    );
  }
  if (!patch || Object.hasOwn(value, "workRecordIds")) {
    result.workRecordIds = uuidList(
      value.workRecordIds === undefined && !patch ? [] : value.workRecordIds,
      "workRecordIds",
    );
  }
  return result;
}

export function normalizeActivityInput(body) {
  const value = object(body);
  allowedFields(value, ["kind", "body", "reportedActorId"]);
  if (!activityKinds.has(value.kind)) throw new WorkBoardError("kind가 올바르지 않습니다.");
  return {
    kind: value.kind,
    body: text(value.body, "body", 4000, true),
    reportedActorId: value.reportedActorId === undefined || value.reportedActorId === null
      ? null : text(value.reportedActorId, "reportedActorId", 100, true),
  };
}

export function normalizeGoalInput(body, { patch = false } = {}) {
  const value = object(body);
  const fields = ["horizon", "periodStart", "title", "description",
    "metricName", "metricUnit", "targetValue", "actualValue"];
  allowedFields(value, patch ? fields : ["projectId", ...fields]);
  if (patch && Object.keys(value).length === 0) throw new WorkBoardError("수정할 필드가 없습니다.");
  const result = {};
  if (!patch) result.projectId = uuid(value.projectId, "projectId");
  if (!patch || Object.hasOwn(value, "horizon")) {
    if (!["weekly", "monthly"].includes(value.horizon)) {
      throw new WorkBoardError("horizon은 weekly 또는 monthly여야 합니다.");
    }
    result.horizon = value.horizon;
  }
  if (!patch || Object.hasOwn(value, "periodStart")) {
    if (value.periodStart === null || value.periodStart === undefined) {
      throw new WorkBoardError("periodStart가 필요합니다.");
    }
    result.periodStart = dueDate(value.periodStart);
  }
  if (!patch || Object.hasOwn(value, "title")) result.title = text(value.title, "title", 160, true);
  if (!patch || Object.hasOwn(value, "description")) {
    result.description = text(value.description ?? "", "description", 10000);
  }
  for (const [key, max] of [["metricName", 120], ["metricUnit", 40]]) {
    if (!patch || Object.hasOwn(value, key)) {
      result[key] = value[key] === null || value[key] === undefined
        ? null : text(value[key], key, max, true);
    }
  }
  if (!patch || Object.hasOwn(value, "targetValue")) {
    result.targetValue = decimalText(value.targetValue ?? null, "targetValue", { positive: true });
  }
  if (!patch || Object.hasOwn(value, "actualValue")) {
    result.actualValue = decimalText(value.actualValue ?? null, "actualValue");
  }
  if (!patch) validateGoalPeriod(result.horizon, result.periodStart);
  if (!patch && (result.targetValue !== null || result.actualValue !== null)
      && !result.metricName) {
    throw new WorkBoardError("수치 목표에는 metricName이 필요합니다.");
  }
  return result;
}

function validateGoalPeriod(horizon, periodStart) {
  const day = new Date(`${periodStart}T00:00:00Z`).getUTCDay();
  if ((horizon === "weekly" && day !== 1)
      || (horizon === "monthly" && !periodStart.endsWith("-01"))) {
    throw new WorkBoardError("주간 시작은 월요일, 월간 시작은 매월 1일이어야 합니다.");
  }
}

const projectColumns = `id, project_key AS "projectKey", title, description,
  created_at AS "createdAt", updated_at AS "updatedAt"`;
const ticketColumns = `id, project_id AS "projectId", title, description,
  assignee_id AS "assigneeId", completed_by_id AS "completedById",
  verified_by_id AS "verifiedById", state, pushed_commit_sha AS "pushedCommitSha",
  ticket_type AS "ticketType", completion_evidence AS "completionEvidence",
  user_review AS "userReview", user_review_note AS "userReviewNote",
  user_reviewed_at AS "userReviewedAt", target_ticket_id AS "targetTicketId",
  verification_verdict AS "verificationVerdict", operation_impact AS "operationImpact",
  resolution_reason AS "resolutionReason",
  due_date::text AS "dueDate",
  completion_criteria AS "completionCriteria", decision_pending AS "decisionPending",
  parent_ticket_id AS "parentTicketId",
  created_at AS "createdAt", updated_at AS "updatedAt"`;
const goalColumns = `id, project_id AS "projectId", horizon,
  period_start::text AS "periodStart", title, description,
  metric_name AS "metricName", metric_unit AS "metricUnit",
  target_value::text AS "targetValue", actual_value::text AS "actualValue",
  created_at AS "createdAt", updated_at AS "updatedAt"`;

export async function listBoard(pool) {
  const projects = await pool.query(
    `SELECT ${projectColumns} FROM work_board_projects ORDER BY created_at, project_key`,
  );
  const tickets = await pool.query(
    `SELECT ${ticketColumns} FROM work_board_tickets ORDER BY created_at, id`,
  );
  const goals = await pool.query(
    `SELECT ${goalColumns} FROM work_board_goals ORDER BY period_start DESC, created_at, id`,
  );
  const dependencies = await pool.query(
    `SELECT ticket_id AS "ticketId", predecessor_id AS "predecessorId"
     FROM work_board_ticket_dependencies ORDER BY ticket_id, predecessor_id`,
  );
  const goalLinks = await pool.query(
    `SELECT ticket_id AS "ticketId", goal_id AS "goalId"
     FROM work_board_goal_tickets ORDER BY ticket_id, goal_id`,
  );
  const recordLinks = await pool.query(
    `SELECT link.ticket_id AS "ticketId", record.id,
       record.title, record.record_type AS "recordType",
       record.character_id AS "characterId", record.source_turn_id AS "sourceTurnId",
       record.recorded_at AS "recordedAt",
       left(CASE WHEN strpos(record.body, E'결과\n') > 0
         THEN substr(record.body, strpos(record.body, E'결과\n') + char_length(E'결과\n'))
         ELSE record.body END, 220) AS "bodyPreview"
     FROM work_board_ticket_records AS link
     JOIN work_records AS record ON record.id = link.work_record_id
     ORDER BY record.recorded_at DESC, record.id`,
  );
  const predecessorIds = new Map();
  const goalIds = new Map();
  const workRecordIds = new Map();
  for (const row of dependencies.rows) {
    if (!predecessorIds.has(row.ticketId)) predecessorIds.set(row.ticketId, []);
    predecessorIds.get(row.ticketId).push(row.predecessorId);
  }
  for (const row of goalLinks.rows) {
    if (!goalIds.has(row.ticketId)) goalIds.set(row.ticketId, []);
    goalIds.get(row.ticketId).push(row.goalId);
  }
  for (const row of recordLinks.rows) {
    if (!workRecordIds.has(row.ticketId)) workRecordIds.set(row.ticketId, []);
    workRecordIds.get(row.ticketId).push(row.id);
  }
  return {
    projects: projects.rows,
    goals: goals.rows,
    workRecords: recordLinks.rows,
    tickets: tickets.rows.map((row) => ({
      ...row,
      predecessorIds: predecessorIds.get(row.id) ?? [],
      goalIds: goalIds.get(row.id) ?? [],
      workRecordIds: workRecordIds.get(row.id) ?? [],
    })),
  };
}

export async function createProject(pool, body) {
  const value = normalizeProjectInput(body);
  try {
    const result = await pool.query(
      `INSERT INTO work_board_projects (project_key, title, description)
       VALUES ($1, $2, $3) RETURNING ${projectColumns}`,
      [value.projectKey, value.title, value.description],
    );
    return result.rows[0];
  } catch (error) {
    if (error.code === "23505") throw new WorkBoardError("이미 있는 프로젝트 키입니다.", 409);
    throw error;
  }
}

export async function updateProject(pool, id, body) {
  uuid(id, "projectId");
  const value = normalizeProjectInput(body, { patch: true });
  const columns = { title: "title", description: "description" };
  const keys = Object.keys(value);
  const assignments = keys.map((key, index) => `${columns[key]} = $${index + 2}`);
  const result = await pool.query(
    `UPDATE work_board_projects SET ${assignments.join(", ")}, updated_at = now()
     WHERE id = $1 RETURNING ${projectColumns}`,
    [id, ...keys.map((key) => value[key])],
  );
  if (!result.rows[0]) throw new WorkBoardError("프로젝트가 없습니다.", 404);
  return result.rows[0];
}

async function lockProject(client, projectId) {
  const result = await client.query(
    'SELECT id, project_key AS "projectKey" FROM work_board_projects WHERE id = $1 FOR UPDATE', [projectId],
  );
  if (!result.rows[0]) throw new WorkBoardError("프로젝트가 없습니다.", 404);
  return result.rows[0];
}

const typedTransitions = {
  open: new Set(["in_progress", "deferred", "canceled"]),
  in_progress: new Set(["review", "deferred", "canceled"]),
  review: new Set(["in_progress", "done", "deferred", "canceled"]),
  deferred: new Set(["open", "canceled"]),
  canceled: new Set(),
  done: new Set(),
};

function requireDeliveryEvidence(project, ticket) {
  if (project.projectKey !== "toss-trading" && ticket.state === "done"
      && ["general", "implementation"].includes(ticket.ticketType) && !ticket.pushedCommitSha) {
    throw new WorkBoardError("구현·미분류 티켓 완료에는 원격 푸시를 확인한 커밋 SHA가 필요합니다.");
  }
}

async function validateLifecycle(client, project, before, next, ticketId = null) {
  const typed = next.ticketType !== "general";
  if (before?.state === "done" && before.ticketType !== "general"
      && Object.keys(next).some((field) => field !== "reportedActorId"
        && JSON.stringify(next[field]) !== JSON.stringify(before[field]))) {
    throw new WorkBoardError("완료된 유형 티켓은 변경할 수 없습니다. 후속 티켓을 만드세요.");
  }
  if (!before && typed && next.state !== "open") {
    throw new WorkBoardError("유형을 지정한 새 티켓은 오픈 상태로 만듭니다.");
  }
  if (before && next.ticketType !== before.ticketType
      && !["open", "in_progress"].includes(before.state)) {
    throw new WorkBoardError(["done", "canceled"].includes(before.state)
      ? "완료·취소된 티켓은 유형을 바꿀 수 없습니다. 후속 티켓을 만드세요."
      : "오픈·진행 상태에서만 유형을 바꿀 수 있습니다. 진행으로 돌린 뒤 수정하세요.");
  }
  if (before && (typed || before.ticketType !== "general") && next.state !== before.state
      && !typedTransitions[before.state].has(next.state)) {
    throw new WorkBoardError("이 유형의 상태 전이가 허용되지 않습니다.");
  }
  if (typed && ["review", "done"].includes(next.state)) {
    if (!next.completionCriteria || !next.completionEvidence || !next.completedById) {
      throw new WorkBoardError("검토·완료에는 완료 조건·결과 근거·완료자가 필요합니다.");
    }
  }
  if (typed && ["deferred", "canceled"].includes(next.state) && !next.resolutionReason) {
    throw new WorkBoardError("대기·취소에는 사유가 필요합니다.");
  }
  if (next.userReview !== "none") {
    const allowedStates = next.userReview === "changes_requested"
      ? ["review", "in_progress"] : ["review", "done"];
    if (!allowedStates.includes(next.state) || !next.userReviewNote) {
      throw new WorkBoardError("사용자 검토 기록에는 검토 상태와 메모가 필요합니다.");
    }
    if (next.userReview !== (before?.userReview ?? "none")) {
      if (!next.reportedActorId) {
        throw new WorkBoardError("사용자 검토 결과 변경에는 기록자 신고값이 필요합니다.");
      }
      if (next.reportedActorId !== "user") {
        const actor = await client.query("SELECT id FROM characters WHERE id = $1", [next.reportedActorId]);
        if (!actor.rows[0]) throw new WorkBoardError("검토 기록자는 사용자 또는 등록된 직원이어야 합니다.");
      }
    }
  }
  if (next.ticketType !== "operations" && next.operationImpact !== null) {
    throw new WorkBoardError("operationImpact는 운영 유형에서만 사용합니다.");
  }
  if (next.ticketType === "operations" && ["review", "done"].includes(next.state)
      && next.operationImpact === null) {
    throw new WorkBoardError("운영 티켓은 내부·외부 영향 범위를 정해야 합니다.");
  }
  if (next.ticketType !== "verification"
      && (next.targetTicketId !== null || next.verificationVerdict !== null)) {
    throw new WorkBoardError("검증 대상과 판정은 독립검증 유형에서만 사용합니다.");
  }
  let target = null;
  if (next.ticketType === "verification" && next.targetTicketId !== null) {
    if (next.targetTicketId === ticketId) throw new WorkBoardError("자기 티켓은 검증 대상이 될 수 없습니다.");
    const result = await client.query(
      `SELECT id, project_id AS "projectId", assignee_id AS "assigneeId",
              completed_by_id AS "completedById", state
       FROM work_board_tickets WHERE id = $1 FOR UPDATE`, [next.targetTicketId],
    );
    target = result.rows[0];
    if (!target || target.projectId !== project.id) {
      throw new WorkBoardError("검증 대상은 같은 프로젝트의 티켓이어야 합니다.");
    }
  }
  if (next.ticketType === "verification" && ["review", "done"].includes(next.state)
      && !target) {
    throw new WorkBoardError("독립검증 티켓에는 검증 대상이 필요합니다.");
  }
  if (next.state === "done" && typed) {
    if (before?.state !== "review" && before?.state !== "done") {
      throw new WorkBoardError("완료 전에 검토 상태를 거쳐야 합니다.");
    }
    if (next.decisionPending) throw new WorkBoardError("사용자 결정 대기 중에는 완료할 수 없습니다.");
    if ((["planning", "content"].includes(next.ticketType)
        || (next.ticketType === "operations" && next.operationImpact === "external"))
        && next.userReview !== "approved") {
      throw new WorkBoardError("이 유형의 완료에는 사용자 검토 승인 기록이 필요합니다.");
    }
    if (next.ticketType === "verification") {
      if (!next.verificationVerdict) throw new WorkBoardError("독립검증 완료에는 합격·불합격 판정이 필요합니다.");
      if (!["review", "done"].includes(target.state)) {
        throw new WorkBoardError("검증 대상은 검토 또는 완료 상태여야 합니다.");
      }
      if ([target.assigneeId, target.completedById].includes(next.completedById)) {
        throw new WorkBoardError("검증 완료자는 대상 티켓의 담당자·완료자와 달라야 합니다.");
      }
    }
  }
  requireDeliveryEvidence(project, next);
  return target;
}

async function requireProjectTickets(client, projectId, ids, field) {
  if (ids.length === 0) return;
  const result = await client.query(
    "SELECT id FROM work_board_tickets WHERE project_id = $1 AND id = ANY($2::uuid[])",
    [projectId, ids],
  );
  if (result.rows.length !== ids.length) {
    throw new WorkBoardError(`${field}는 같은 프로젝트의 티켓이어야 합니다.`);
  }
}

async function requireProjectGoals(client, projectId, ids) {
  if (ids.length === 0) return;
  const result = await client.query(
    "SELECT id FROM work_board_goals WHERE project_id = $1 AND id = ANY($2::uuid[])",
    [projectId, ids],
  );
  if (result.rows.length !== ids.length) {
    throw new WorkBoardError("goalIds는 같은 프로젝트의 목표여야 합니다.");
  }
}

async function validateTicketPlan(client, projectId, ticketId, value) {
  if (Object.hasOwn(value, "parentTicketId") && value.parentTicketId !== null) {
    if (value.parentTicketId === ticketId) throw new WorkBoardError("티켓은 자신의 부모가 될 수 없습니다.");
    await requireProjectTickets(client, projectId, [value.parentTicketId], "parentTicketId");
    const result = await client.query(
      "SELECT id, parent_ticket_id AS \"parentTicketId\" FROM work_board_tickets WHERE project_id = $1",
      [projectId],
    );
    const parents = new Map(result.rows.map((row) => [row.id, row.parentTicketId]));
    parents.set(ticketId, value.parentTicketId);
    const visited = new Set();
    let cursor = value.parentTicketId;
    while (cursor !== null) {
      if (cursor === ticketId || visited.has(cursor)) {
        throw new WorkBoardError("WBS 부모 관계에 순환이 생깁니다.");
      }
      visited.add(cursor);
      cursor = parents.get(cursor) ?? null;
    }
  }
  if (Object.hasOwn(value, "predecessorIds")) {
    if (value.predecessorIds.includes(ticketId)) {
      throw new WorkBoardError("티켓은 자신의 선행 작업이 될 수 없습니다.");
    }
    await requireProjectTickets(client, projectId, value.predecessorIds, "predecessorIds");
    const result = await client.query(
      `SELECT ticket_id AS "ticketId", predecessor_id AS "predecessorId"
       FROM work_board_ticket_dependencies WHERE project_id = $1`,
      [projectId],
    );
    const graph = new Map();
    for (const row of result.rows) {
      if (!graph.has(row.ticketId)) graph.set(row.ticketId, []);
      graph.get(row.ticketId).push(row.predecessorId);
    }
    graph.set(ticketId, value.predecessorIds);
    const seen = new Set();
    const pending = [...value.predecessorIds];
    while (pending.length) {
      const current = pending.pop();
      if (current === ticketId) {
        throw new WorkBoardError("선행 관계에 순환이 생깁니다.");
      }
      if (seen.has(current)) continue;
      seen.add(current);
      pending.push(...(graph.get(current) ?? []));
    }
  }
  if (Object.hasOwn(value, "goalIds")) {
    await requireProjectGoals(client, projectId, value.goalIds);
  }
}

async function replaceTicketLinks(client, projectId, ticketId, value) {
  if (Object.hasOwn(value, "predecessorIds")) {
    await client.query("DELETE FROM work_board_ticket_dependencies WHERE ticket_id = $1", [ticketId]);
    for (const predecessorId of value.predecessorIds) {
      await client.query(
        `INSERT INTO work_board_ticket_dependencies (ticket_id, predecessor_id, project_id)
         VALUES ($1, $2, $3)`, [ticketId, predecessorId, projectId],
      );
    }
  }
  if (Object.hasOwn(value, "goalIds")) {
    await client.query("DELETE FROM work_board_goal_tickets WHERE ticket_id = $1", [ticketId]);
    for (const goalId of value.goalIds) {
      await client.query(
        `INSERT INTO work_board_goal_tickets (ticket_id, goal_id, project_id)
         VALUES ($1, $2, $3)`, [ticketId, goalId, projectId],
      );
    }
  }
  if (Object.hasOwn(value, "workRecordIds")) {
    const ids = value.workRecordIds;
    if (ids.length) {
      const found = await client.query(
        "SELECT id FROM work_records WHERE id = ANY($1::uuid[])", [ids],
      );
      if (found.rows.length !== ids.length) {
        throw new WorkBoardError("workRecordIds에 없는 업무 기록이 있습니다.");
      }
    }
    await client.query("DELETE FROM work_board_ticket_records WHERE ticket_id = $1", [ticketId]);
    for (const recordId of ids) {
      await client.query(
        `INSERT INTO work_board_ticket_records (ticket_id, work_record_id)
         VALUES ($1, $2)`, [ticketId, recordId],
      );
    }
  }
}

async function readTicket(client, id) {
  const result = await client.query(
    `SELECT ${ticketColumns} FROM work_board_tickets WHERE id = $1`, [id],
  );
  const row = result.rows[0];
  if (!row) throw new WorkBoardError("티켓이 없습니다.", 404);
  const dependencies = await client.query(
    "SELECT predecessor_id AS id FROM work_board_ticket_dependencies WHERE ticket_id = $1 ORDER BY id", [id],
  );
  const goals = await client.query(
    "SELECT goal_id AS id FROM work_board_goal_tickets WHERE ticket_id = $1 ORDER BY id", [id],
  );
  const records = await client.query(
    `SELECT work_record_id AS id FROM work_board_ticket_records
     WHERE ticket_id = $1 ORDER BY work_record_id`, [id],
  );
  return {
    ...row,
    predecessorIds: dependencies.rows.map((item) => item.id),
    goalIds: goals.rows.map((item) => item.id),
    workRecordIds: records.rows.map((item) => item.id),
  };
}

export async function readBoardWorkRecord(pool, id) {
  uuid(id, "workRecordId");
  const result = await pool.query(
    `SELECT id, title, body, record_type AS "recordType",
       character_id AS "characterId", source_turn_id AS "sourceTurnId",
       recorded_at AS "recordedAt"
     FROM work_records WHERE id = $1`, [id],
  );
  if (!result.rows[0]) throw new WorkBoardError("업무 기록이 없습니다.", 404);
  return result.rows[0];
}

export async function listTicketActivity(pool, id) {
  uuid(id, "ticketId");
  const ticket = await pool.query("SELECT id FROM work_board_tickets WHERE id = $1", [id]);
  if (!ticket.rows[0]) throw new WorkBoardError("티켓이 없습니다.", 404);
  const result = await pool.query(
    `SELECT id, ticket_id AS "ticketId", kind, body, changes,
       reported_actor_id AS "reportedActorId", created_at AS "createdAt"
     FROM work_board_ticket_activity WHERE ticket_id = $1
     ORDER BY created_at DESC, id DESC LIMIT 100`, [id],
  );
  const labels = {
    title: "제목", description: "설명", assigneeId: "담당자",
    completedById: "완료자", verifiedById: "검증자", state: "상태",
    ticketType: "유형", completionEvidence: "완료 근거", userReview: "사용자 검토",
    userReviewNote: "사용자 검토 메모", userReviewedAt: "사용자 검토 시각",
    targetTicketId: "검증 대상", verificationVerdict: "검증 판정",
    operationImpact: "운영 영향", resolutionReason: "대기·취소 사유",
    dueDate: "기한", completionCriteria: "완료 조건", decisionPending: "사용자 결정 대기",
    parentTicketId: "상위 작업", predecessorIds: "선행 작업", goalIds: "목표 연결",
    workRecordIds: "업무 기록 연결", pushedCommitSha: "푸시된 커밋 SHA",
  };
  const display = (value) => {
    const serialized = JSON.stringify(value);
    return serialized.length > 180 ? `${serialized.slice(0, 180)}…` : serialized;
  };
  return result.rows.map((row) => ({
    ...row,
    summary: row.kind === "changed"
      ? Object.entries(row.changes).map(([field, change]) =>
        `${labels[field] ?? field}: ${display(change.before)} → ${display(change.after)}`).join(" · ")
      : row.kind === "created" ? "티켓 생성" : row.body,
  }));
}

export async function addTicketActivity(client, id, body) {
  uuid(id, "ticketId");
  const value = normalizeActivityInput(body);
  const result = await client.query(
    `INSERT INTO work_board_ticket_activity (ticket_id, kind, body, reported_actor_id)
     SELECT id, $2, $3, $4 FROM work_board_tickets WHERE id = $1
     RETURNING id, ticket_id AS "ticketId", kind, body, changes,
       reported_actor_id AS "reportedActorId", created_at AS "createdAt"`,
    [id, value.kind, value.body, value.reportedActorId],
  );
  if (!result.rows[0]) throw new WorkBoardError("티켓이 없습니다.", 404);
  return { ...result.rows[0], summary: result.rows[0].body };
}

async function writeChangeActivity(client, id, kind, changes, reportedActorId = null) {
  await client.query(
    `INSERT INTO work_board_ticket_activity
       (ticket_id, kind, changes, reported_actor_id) VALUES ($1, $2, $3::jsonb, $4)`,
    [id, kind, JSON.stringify(changes), reportedActorId],
  );
}

export async function createTicket(pool, body) {
  const value = normalizeTicketInput(body);
  const project = await lockProject(pool, value.projectId);
  await validateLifecycle(pool, project, null, value);
  try {
    const result = await pool.query(
      `INSERT INTO work_board_tickets
       (project_id, title, description, assignee_id, completed_by_id, verified_by_id, state, due_date,
        completion_criteria, parent_ticket_id, decision_pending, pushed_commit_sha,
        ticket_type, completion_evidence, user_review, user_review_note, user_reviewed_at,
        target_ticket_id, verification_verdict, operation_impact, resolution_reason)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
               $13, $14, $15, $16, $17, $18, $19, $20, $21) RETURNING id`,
      [value.projectId, value.title, value.description, value.assigneeId,
        value.completedById, value.verifiedById, value.state, value.dueDate,
        value.completionCriteria, value.parentTicketId, value.decisionPending, value.pushedCommitSha,
        value.ticketType, value.completionEvidence, value.userReview, value.userReviewNote,
        value.userReview === "none" ? null : new Date(), value.targetTicketId,
        value.verificationVerdict, value.operationImpact, value.resolutionReason],
    );
    const id = result.rows[0].id;
    await validateTicketPlan(pool, value.projectId, id, value);
    await replaceTicketLinks(pool, value.projectId, id, value);
    await writeChangeActivity(pool, id, "created", {}, value.reportedActorId ?? null);
    return await readTicket(pool, id);
  } catch (error) {
    if (error.code === "23503") throw new WorkBoardError("프로젝트, 담당자 또는 연결 대상이 없습니다.", 400);
    throw error;
  }
}

export async function updateTicket(pool, id, body) {
  uuid(id, "ticketId");
  const value = normalizeTicketInput(body, { patch: true });
  const existing = await pool.query(
    "SELECT project_id AS \"projectId\" FROM work_board_tickets WHERE id = $1", [id],
  );
  if (!existing.rows[0]) throw new WorkBoardError("티켓이 없습니다.", 404);
  const projectId = existing.rows[0].projectId;
  const project = await lockProject(pool, projectId);
  const before = await readTicket(pool, id);
  const reviewMaterialChanged = ["description", "completionCriteria", "completionEvidence",
    "ticketType", "operationImpact", "targetTicketId"].some((field) =>
    Object.hasOwn(value, field) && JSON.stringify(value[field]) !== JSON.stringify(before[field]));
  if ((before.state === "review" && value.state === "in_progress"
       && value.userReview !== "changes_requested")
      || (before.userReview === "approved" && reviewMaterialChanged
          && value.userReview !== "changes_requested")) {
    value.userReview = "none";
    value.userReviewNote = "";
  }
  if (value.userReview === "none") value.userReviewNote = "";
  const next = { ...before, ...value };
  const target = await validateLifecycle(pool, project, before, next, id);
  await validateTicketPlan(pool, projectId, id, value);
  const columns = {
    title: "title", description: "description", assigneeId: "assignee_id",
    completedById: "completed_by_id", verifiedById: "verified_by_id",
    state: "state", dueDate: "due_date", completionCriteria: "completion_criteria",
    parentTicketId: "parent_ticket_id", decisionPending: "decision_pending",
    pushedCommitSha: "pushed_commit_sha", ticketType: "ticket_type",
    completionEvidence: "completion_evidence", userReview: "user_review",
    userReviewNote: "user_review_note", targetTicketId: "target_ticket_id",
    verificationVerdict: "verification_verdict", operationImpact: "operation_impact",
    resolutionReason: "resolution_reason",
  };
  const keys = Object.keys(value).filter((key) => columns[key]);
  const assignments = keys.map((key, index) => `${columns[key]} = $${index + 2}`);
  let reviewedAt = before.userReviewedAt;
  if (Object.hasOwn(value, "userReview") && value.userReview !== before.userReview) {
    reviewedAt = value.userReview === "none" ? null : new Date();
    assignments.push(`user_reviewed_at = $${keys.length + 2}`);
  }
  try {
    const result = await pool.query(
      `UPDATE work_board_tickets SET ${assignments.length ? `${assignments.join(", ")}, ` : ""}updated_at = now()
       WHERE id = $1 RETURNING id`,
      [id, ...keys.map((key) => value[key]),
        ...(Object.hasOwn(value, "userReview") && value.userReview !== before.userReview ? [reviewedAt] : [])],
    );
    if (!result.rows[0]) throw new WorkBoardError("티켓이 없습니다.", 404);
    await replaceTicketLinks(pool, projectId, id, value);
    if (before.state !== "done" && next.state === "done"
        && next.ticketType === "verification") {
      const targetBefore = await readTicket(pool, target.id);
      if (next.verificationVerdict === "fail") {
        await pool.query(
          `UPDATE work_board_tickets SET state = 'in_progress', completed_by_id = NULL,
           verified_by_id = NULL, completion_evidence = '', pushed_commit_sha = NULL,
           user_review = 'none', user_review_note = '', user_reviewed_at = NULL,
           decision_pending = false, updated_at = now() WHERE id = $1`, [target.id],
        );
      } else {
        await pool.query(
          `UPDATE work_board_tickets SET verified_by_id = $2, updated_at = now()
           WHERE id = $1`, [target.id, next.completedById],
        );
      }
      const targetAfter = await readTicket(pool, target.id);
      const targetChanges = {};
      for (const field of ["state", "completedById", "verifiedById", "completionEvidence",
        "pushedCommitSha", "userReview", "userReviewNote", "userReviewedAt", "decisionPending"]) {
        if (JSON.stringify(targetBefore[field]) !== JSON.stringify(targetAfter[field])) {
          targetChanges[field] = { before: targetBefore[field], after: targetAfter[field] };
        }
      }
      if (Object.keys(targetChanges).length) {
        await writeChangeActivity(pool, target.id, "changed", targetChanges, value.reportedActorId ?? null);
      }
    }
    const after = await readTicket(pool, id);
    const changes = {};
    for (const key of Object.keys(value)) {
      if (key === "reportedActorId") continue;
      if (JSON.stringify(before[key]) !== JSON.stringify(after[key])) {
        changes[key] = { before: before[key], after: after[key] };
      }
    }
    if (Object.keys(changes).length) {
      await writeChangeActivity(pool, id, "changed", changes, value.reportedActorId ?? null);
    }
    return after;
  } catch (error) {
    if (error.code === "23503") throw new WorkBoardError("담당자가 없습니다.", 400);
    throw error;
  }
}

export async function createGoal(client, body) {
  const value = normalizeGoalInput(body);
  await lockProject(client, value.projectId);
  const result = await client.query(
    `INSERT INTO work_board_goals
     (project_id, horizon, period_start, title, description, metric_name,
      metric_unit, target_value, actual_value)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
     RETURNING ${goalColumns}`,
    [value.projectId, value.horizon, value.periodStart, value.title,
      value.description, value.metricName, value.metricUnit,
      value.targetValue, value.actualValue],
  );
  return result.rows[0];
}

export async function updateGoal(client, id, body) {
  uuid(id, "goalId");
  const value = normalizeGoalInput(body, { patch: true });
  const existing = await client.query(
    `SELECT project_id AS "projectId", horizon,
      period_start::text AS "periodStart", metric_name AS "metricName",
      target_value::text AS "targetValue", actual_value::text AS "actualValue"
     FROM work_board_goals WHERE id = $1`, [id],
  );
  if (!existing.rows[0]) throw new WorkBoardError("목표가 없습니다.", 404);
  await lockProject(client, existing.rows[0].projectId);
  const merged = { ...existing.rows[0], ...value };
  validateGoalPeriod(merged.horizon, merged.periodStart);
  if ((merged.targetValue !== null || merged.actualValue !== null) && !merged.metricName) {
    throw new WorkBoardError("수치 목표에는 metricName이 필요합니다.");
  }
  const columns = {
    horizon: "horizon", periodStart: "period_start", title: "title",
    description: "description", metricName: "metric_name", metricUnit: "metric_unit",
    targetValue: "target_value", actualValue: "actual_value",
  };
  const keys = Object.keys(value);
  const assignments = keys.map((key, index) => `${columns[key]} = $${index + 2}`);
  const result = await client.query(
    `UPDATE work_board_goals SET ${assignments.join(", ")}, updated_at = now()
     WHERE id = $1 RETURNING ${goalColumns}`,
    [id, ...keys.map((key) => value[key])],
  );
  return result.rows[0];
}
