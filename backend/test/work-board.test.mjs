import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";
import pg from "pg";
import {
  WorkBoardError,
  addTicketActivity,
  createProject,
  createTicket,
  createGoal,
  listBoard,
  listTicketActivity,
  normalizeActivityInput,
  normalizeGoalInput,
  normalizeProjectInput,
  normalizeTicketInput,
  readBoardWorkRecord,
  updateGoal,
  updateTicket,
} from "../src/work-board.mjs";

test("프로젝트·티켓 입력은 상태, 날짜, 필드와 담당자를 검증한다", () => {
  assert.deepEqual(normalizeProjectInput({ projectKey: "toss-trading", title: " Toss " }), {
    projectKey: "toss-trading", title: "Toss", description: "",
  });
  assert.throws(() => normalizeProjectInput({ projectKey: "TOSS", title: "Toss" }), WorkBoardError);
  assert.throws(() => normalizeProjectInput({ projectKey: "toss-trading", title: " " }), WorkBoardError);
  assert.throws(() => normalizeProjectInput({ title: "X", extra: true }, { patch: true }), WorkBoardError);
  const projectId = randomUUID();
  const ticket = normalizeTicketInput({ projectId, title: " 확인 " });
  assert.equal(ticket.title, "확인");
  assert.equal(ticket.state, "open");
  assert.equal(ticket.pushedCommitSha, null);
  assert.equal(ticket.dueDate, null);
  assert.equal(ticket.assigneeId, null);
  assert.equal(ticket.completedById, null);
  assert.equal(ticket.verifiedById, null);
  assert.deepEqual(normalizeTicketInput({ completedById: "left-man", verifiedById: "left-woman" }, { patch: true }), {
    completedById: "left-man", verifiedById: "left-woman",
  });
  assert.deepEqual(normalizeTicketInput({ completedById: null, verifiedById: null }, { patch: true }), {
    completedById: null, verifiedById: null,
  });
  assert.throws(() => normalizeTicketInput({ completedById: 5 }, { patch: true }), WorkBoardError);
  for (const value of ["2026-02-30", "2026-13-01", "내일", 20260925]) {
    assert.throws(() => normalizeTicketInput({ dueDate: value }, { patch: true }), WorkBoardError);
  }
  assert.deepEqual(normalizeTicketInput({ dueDate: null, assigneeId: null }, { patch: true }), {
    dueDate: null, assigneeId: null,
  });
  assert.throws(() => normalizeTicketInput({ state: "active" }, { patch: true }), WorkBoardError);
  assert.throws(() => normalizeTicketInput({ state: "todo" }, { patch: true }), WorkBoardError);
  assert.throws(() => normalizeTicketInput({ state: "blocked" }, { patch: true }), WorkBoardError);
  for (const state of ["open", "in_progress", "review", "done", "canceled", "deferred"]) {
    assert.equal(normalizeTicketInput({ state }, { patch: true }).state, state);
  }
  assert.deepEqual(normalizeTicketInput({ pushedCommitSha: "a".repeat(40) }, { patch: true }), {
    pushedCommitSha: "a".repeat(40),
  });
  for (const sha of ["abc", "A".repeat(40), "g".repeat(40), 42]) {
    assert.throws(() => normalizeTicketInput({ pushedCommitSha: sha }, { patch: true }), WorkBoardError);
  }
  assert.throws(() => normalizeTicketInput({ state: null }, { patch: true }), WorkBoardError);
  assert.throws(() => normalizeTicketInput({ projectId }, { patch: true }), WorkBoardError);
  assert.equal(ticket.decisionPending, false);
  assert.deepEqual(normalizeTicketInput({ decisionPending: true }, { patch: true }), {
    decisionPending: true,
  });
  assert.throws(() => normalizeTicketInput({ decisionPending: "true" }, { patch: true }), WorkBoardError);
});

test("진행 기록은 본문과 신고된 작성자만 받는다", () => {
  assert.deepEqual(normalizeActivityInput({ kind: "progress", body: " 진행 ", reportedActorId: "left-woman" }), {
    kind: "progress", body: "진행", reportedActorId: "left-woman",
  });
  for (const bad of [
    { kind: "changed", body: "수정" },
    { kind: "progress", body: " " },
    { kind: "decision", body: "x", extra: true },
  ]) assert.throws(() => normalizeActivityInput(bad), WorkBoardError);
});

test("오피스 완료에는 푸시 커밋 근거가 필요하고 Toss는 별도 정책을 쓴다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  try {
    await client.query("BEGIN");
    const office = await createProject(query, {
      projectKey: `delivery-${randomUUID().slice(0, 12)}`, title: "오피스 반영",
    });
    await assert.rejects(createTicket(query, {
      projectId: office.id, title: "근거 없는 완료", state: "done",
    }), (error) => error instanceof WorkBoardError && /커밋 SHA/.test(error.message));
    const ticket = await createTicket(query, { projectId: office.id, title: "검토" });
    await updateTicket(query, ticket.id, { state: "review" });
    await assert.rejects(updateTicket(query, ticket.id, { state: "done" }), WorkBoardError);
    const pushedCommitSha = "a".repeat(40);
    const completed = await updateTicket(query, ticket.id, { state: "done", pushedCommitSha });
    assert.equal(completed.state, "done");
    assert.equal(completed.pushedCommitSha, pushedCommitSha);
    await assert.rejects(updateTicket(query, ticket.id, { pushedCommitSha: null }), WorkBoardError);
    const toss = await query.query("SELECT id FROM work_board_projects WHERE project_key = 'toss-trading'");
    assert.equal((await createTicket(query, {
      projectId: toss.rows[0].id, title: "별도 저장소", state: "done",
    })).pushedCommitSha, null);
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});

test("진행 메모와 티켓 수정 이력을 누적하고 결정 대기를 상태와 분리한다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  try {
    await client.query("BEGIN");
    const project = await createProject(query, {
      projectKey: `activity-${randomUUID().slice(0, 12)}`, title: "이력 검증",
    });
    const ticket = await createTicket(query, { projectId: project.id, title: "TTS", state: "open" });
    const first = await addTicketActivity(query, ticket.id, {
      kind: "progress", body: "첫 측정 2.1초", reportedActorId: "left-woman",
    });
    await addTicketActivity(query, ticket.id, { kind: "verification", body: "두 번째 측정 1.8초" });
    await updateTicket(query, ticket.id, { state: "deferred", decisionPending: true });
    await updateTicket(query, ticket.id, { state: "deferred" });
    const events = await listTicketActivity(query, ticket.id);
    assert.equal(events.length, 4); // 생성, 메모 2건, 실제 변경 1건
    assert.equal(events.find((item) => item.id === first.id)?.reportedActorId, "left-woman");
    const change = events.find((item) => item.kind === "changed");
    assert.deepEqual(change.changes.state, { before: "open", after: "deferred" });
    assert.deepEqual(change.changes.decisionPending, { before: false, after: true });
    assert.equal((await listBoard(query)).tickets.find((item) => item.id === ticket.id)?.decisionPending, true);
    await assert.rejects(listTicketActivity(query, randomUUID()),
      (error) => error instanceof WorkBoardError && error.status === 404);
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});

test("주간·월간 목표는 명시한 기간과 문자열 수치만 받는다", () => {
  const projectId = randomUUID();
  const weekly = normalizeGoalInput({
    projectId, horizon: "weekly", periodStart: "2026-09-21", title: "검증 완료",
  });
  assert.equal(weekly.targetValue, null);
  assert.equal(weekly.actualValue, null);
  assert.throws(() => normalizeGoalInput({
    projectId, horizon: "weekly", periodStart: "2026-09-22", title: "잘못된 시작",
  }), WorkBoardError);
  assert.throws(() => normalizeGoalInput({
    projectId, horizon: "monthly", periodStart: "2026-09-02", title: "잘못된 시작",
  }), WorkBoardError);
  assert.throws(() => normalizeGoalInput({
    projectId, horizon: "monthly", periodStart: "2026-10-01", title: "수치",
    metricName: "처리 건수", targetValue: 10,
  }), WorkBoardError);
  assert.throws(() => normalizeGoalInput({
    projectId, horizon: "monthly", periodStart: "2026-10-01", title: "수치",
    targetValue: "10",
  }), WorkBoardError);
  assert.deepEqual(normalizeTicketInput({
    parentTicketId: null, predecessorIds: [], goalIds: [],
  }, { patch: true }), {
    parentTicketId: null, predecessorIds: [], goalIds: [],
  });
  assert.throws(() => normalizeTicketInput({
    predecessorIds: [projectId, projectId],
  }, { patch: true }), WorkBoardError);
  assert.throws(() => normalizeTicketInput({ goalIds: null }, { patch: true }), WorkBoardError);
  assert.throws(() => normalizeTicketInput({ workRecordIds: [projectId, projectId] }, { patch: true }), WorkBoardError);
  assert.deepEqual(normalizeTicketInput({ workRecordIds: [] }, { patch: true }), { workRecordIds: [] });
});

test("티켓과 실제 업무 기록을 연결하고 해제하되 본문은 목록에서 제외한다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  try {
    await client.query("BEGIN");
    const repository = await query.query("SELECT id FROM projects LIMIT 1");
    assert.ok(repository.rows[0]);
    const record = await query.query(
      `INSERT INTO work_records (project_id, record_type, title, body)
       VALUES ($1, 'result', 'TTS 확인 결과', '상세 본문') RETURNING id`,
      [repository.rows[0].id],
    );
    const project = await createProject(query, {
      projectKey: `record-${randomUUID().slice(0, 12)}`, title: "연결 검증",
    });
    const ticket = await createTicket(query, {
      projectId: project.id, title: "음성 확인", workRecordIds: [record.rows[0].id],
    });
    assert.deepEqual(ticket.workRecordIds, [record.rows[0].id]);
    const snapshot = await listBoard(query);
    const linked = snapshot.workRecords.find((item) => item.id === record.rows[0].id);
    assert.equal(linked.ticketId, ticket.id);
    assert.equal(linked.title, "TTS 확인 결과");
    assert.equal(Object.hasOwn(linked, "body"), false);
    assert.equal(linked.bodyPreview, "상세 본문");
    const detail = await readBoardWorkRecord(query, record.rows[0].id);
    assert.equal(detail.body, "상세 본문");
    await assert.rejects(readBoardWorkRecord(query, randomUUID()),
      (error) => error instanceof WorkBoardError && error.status === 404);
    await assert.rejects(updateTicket(query, ticket.id, { workRecordIds: [randomUUID()] }),
      (error) => error instanceof WorkBoardError && /없는 업무 기록/.test(error.message));
    assert.deepEqual((await listBoard(query)).tickets.find((item) => item.id === ticket.id).workRecordIds,
      [record.rows[0].id]);
    await updateTicket(query, ticket.id, { workRecordIds: [] });
    assert.deepEqual((await listBoard(query)).tickets.find((item) => item.id === ticket.id).workRecordIds, []);
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});

test("프로젝트 키 중복을 거부한다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  const key = `board-test-${randomUUID().slice(0, 12)}`;
  try {
    await client.query("BEGIN");
    await createProject(query, { projectKey: key, title: "검증 프로젝트" });
    await assert.rejects(
      createProject(query, { projectKey: key, title: "중복" }),
      (error) => error instanceof WorkBoardError && error.status === 409,
    );
  } catch (error) {
    // 중복 INSERT는 PostgreSQL 트랜잭션을 실패 상태로 만든다.
    await client.query("ROLLBACK");
    throw error;
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});

test("프로젝트·티켓의 실제 저장, 상태 변경, 담당 미정을 조회한다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  try {
    await client.query("BEGIN");
    const project = await createProject(query, {
      projectKey: `board-test-${randomUUID().slice(0, 12)}`,
      title: "검증 프로젝트",
    });
    const ticket = await createTicket(query, {
      projectId: project.id,
      title: "완료 조건 확인",
      completionCriteria: "사용자가 직접 확인",
      dueDate: null,
    });
    assert.equal(ticket.state, "open");
    assert.equal(ticket.assigneeId, null);
    const updated = await updateTicket(query, ticket.id, {
      state: "deferred", dueDate: "2026-10-01", description: "외부 결정 대기",
    });
    assert.equal(updated.state, "deferred");
    assert.equal(updated.dueDate, "2026-10-01");
    const snapshot = await listBoard(query);
    assert.equal(snapshot.tickets.find((item) => item.id === ticket.id)?.description, "외부 결정 대기");
    assert.equal(snapshot.projects.find((item) => item.id === project.id)?.title, "검증 프로젝트");
    await assert.rejects(updateTicket(query, randomUUID(), { state: "done" }),
      (error) => error instanceof WorkBoardError && error.status === 404);
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});

test("WBS 부모·선행 순환과 다른 프로젝트 연결은 거부하고 목표 연결은 보존한다", {
  skip: !process.env.OFFICE_TEST_DATABASE_URL,
}, async () => {
  const pool = new pg.Pool({ connectionString: process.env.OFFICE_TEST_DATABASE_URL });
  const client = await pool.connect();
  const query = { query: client.query.bind(client) };
  try {
    await client.query("BEGIN");
    const project = await createProject(query, {
      projectKey: `wbs-${randomUUID().slice(0, 12)}`, title: "WBS 검증",
    });
    const other = await createProject(query, {
      projectKey: `wbs-${randomUUID().slice(0, 12)}`, title: "다른 프로젝트",
    });
    const goal = await createGoal(query, {
      projectId: project.id, horizon: "weekly", periodStart: "2026-09-21",
      title: "한 주 목표", metricName: "완료 건수", targetValue: "5",
    });
    assert.equal(goal.actualValue, null);
    const root = await createTicket(query, { projectId: project.id, title: "상위" });
    const child = await createTicket(query, {
      projectId: project.id, title: "하위", parentTicketId: root.id,
      predecessorIds: [root.id], goalIds: [goal.id],
    });
    assert.equal(child.parentTicketId, root.id);
    assert.deepEqual(child.predecessorIds, [root.id]);
    assert.deepEqual(child.goalIds, [goal.id]);
    await assert.rejects(updateTicket(query, root.id, { parentTicketId: child.id }),
      (error) => error instanceof WorkBoardError && /순환/.test(error.message));
    await assert.rejects(updateTicket(query, root.id, { predecessorIds: [child.id] }),
      (error) => error instanceof WorkBoardError && /순환/.test(error.message));
    const foreign = await createTicket(query, { projectId: other.id, title: "다른 티켓" });
    await assert.rejects(updateTicket(query, root.id, { parentTicketId: foreign.id }), WorkBoardError);
    await assert.rejects(updateTicket(query, root.id, { predecessorIds: [foreign.id] }), WorkBoardError);
    await assert.rejects(updateTicket(query, foreign.id, { goalIds: [goal.id] }), WorkBoardError);
    const revised = await updateGoal(query, goal.id, { actualValue: "2" });
    assert.equal(revised.targetValue, "5.0000");
    assert.equal(revised.actualValue, "2.0000");
    const snapshot = await listBoard(query);
    const stored = snapshot.tickets.find((ticket) => ticket.id === child.id);
    assert.equal(stored.parentTicketId, root.id);
    assert.deepEqual(stored.predecessorIds, [root.id]);
    assert.deepEqual(stored.goalIds, [goal.id]);
    assert.equal(snapshot.goals.find((item) => item.id === goal.id)?.periodStart, "2026-09-21");
    await updateTicket(query, child.id, {
      parentTicketId: null, predecessorIds: [], goalIds: [],
    });
    const cleared = await listBoard(query);
    const clearedChild = cleared.tickets.find((ticket) => ticket.id === child.id);
    assert.equal(clearedChild.parentTicketId, null);
    assert.deepEqual(clearedChild.predecessorIds, []);
    assert.deepEqual(clearedChild.goalIds, []);
  } finally {
    await client.query("ROLLBACK");
    client.release();
    await pool.end();
  }
});
