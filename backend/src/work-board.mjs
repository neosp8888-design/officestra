// 공통 프로젝트·티켓 데이터 계층. 배정은 실행 요청과 독립적이다.

export class WorkBoardError extends Error {
  constructor(message, status = 400) {
    super(message);
    this.name = "WorkBoardError";
    this.status = status;
  }
}

const states = new Set(["open", "in_progress", "review", "done", "canceled", "deferred"]);
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
    "decisionPending", "reportedActorId", "pushedCommitSha"];
  allowedFields(value, patch ? fields : ["projectId", ...fields]);
  if (patch && Object.keys(value).length === 0) throw new WorkBoardError("수정할 필드가 없습니다.");
  const result = {};
  if (!patch) result.projectId = uuid(value.projectId, "projectId");
  if (!patch || Object.hasOwn(value, "title")) result.title = text(value.title, "title", 160, true);
  if (!patch || Object.hasOwn(value, "description")) result.description = text(value.description ?? "", "description", 10000);
  if (!patch || Object.hasOwn(value, "completionCriteria")) result.completionCriteria = text(value.completionCriteria ?? "", "completionCriteria", 4000);
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

function requireDeliveryEvidence(project, state, pushedCommitSha) {
  if (project.projectKey !== "toss-trading" && state === "done" && !pushedCommitSha) {
    throw new WorkBoardError("오피스 티켓 완료에는 원격 푸시를 확인한 커밋 SHA가 필요합니다.");
  }
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
  requireDeliveryEvidence(project, value.state, value.pushedCommitSha);
  try {
    const result = await pool.query(
      `INSERT INTO work_board_tickets
       (project_id, title, description, assignee_id, completed_by_id, verified_by_id, state, due_date,
        completion_criteria, parent_ticket_id, decision_pending, pushed_commit_sha)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id`,
      [value.projectId, value.title, value.description, value.assigneeId,
        value.completedById, value.verifiedById, value.state, value.dueDate,
        value.completionCriteria, value.parentTicketId, value.decisionPending, value.pushedCommitSha],
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
  requireDeliveryEvidence(project, value.state ?? before.state,
    Object.hasOwn(value, "pushedCommitSha") ? value.pushedCommitSha : before.pushedCommitSha);
  await validateTicketPlan(pool, projectId, id, value);
  const columns = {
    title: "title", description: "description", assigneeId: "assignee_id",
    completedById: "completed_by_id", verifiedById: "verified_by_id",
    state: "state", dueDate: "due_date", completionCriteria: "completion_criteria",
    parentTicketId: "parent_ticket_id", decisionPending: "decision_pending",
    pushedCommitSha: "pushed_commit_sha",
  };
  const keys = Object.keys(value).filter((key) => columns[key]);
  const assignments = keys.map((key, index) => `${columns[key]} = $${index + 2}`);
  try {
    const result = await pool.query(
      `UPDATE work_board_tickets SET ${assignments.length ? `${assignments.join(", ")}, ` : ""}updated_at = now()
       WHERE id = $1 RETURNING id`,
      [id, ...keys.map((key) => value[key])],
    );
    if (!result.rows[0]) throw new WorkBoardError("티켓이 없습니다.", 404);
    await replaceTicketLinks(pool, projectId, id, value);
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
