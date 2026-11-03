// 업무 보드 DB 테스트. Node work-board.test.mjs의 DB 사례를 같은 순서로 옮겼고, 각 사례는 트랜잭션 안에서 돌린 뒤 되돌린다.

use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::work_board::*;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::OnceCell;
use tower::ServiceExt;

static READY: OnceCell<()> = OnceCell::const_new();

/// Node 테스트가 기대하는 직원 두 명과 저장소 프로젝트 하나를 테스트 DB에 둔다.
const FIXTURES: &str = "
INSERT INTO characters (id, name, seat, backend, permission) VALUES
  ('left-man', '왼쪽 남', 'left-man', 'claude', 'default'),
  ('left-woman', '왼쪽 여', 'left-woman', 'claude', 'default')
ON CONFLICT (id) DO NOTHING;
INSERT INTO projects (repository_root)
  SELECT '/tmp/office-rs-work-board-test' WHERE NOT EXISTS (SELECT 1 FROM projects);
";

async fn test_pool() -> Option<PgPool> {
    let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR"))
    else {
        eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return None;
    };
    let pool = db::connect(&url).await.unwrap();
    READY
        .get_or_init(|| async {
            migrate::run(&pool, Path::new(&migrations)).await.unwrap();
            sqlx::raw_sql(FIXTURES).execute(&pool).await.unwrap();
        })
        .await;
    Some(pool)
}

async fn begin() -> Option<(PgPool, Transaction<'static, Postgres>)> {
    let pool = test_pool().await?;
    let transaction = pool.begin().await.unwrap();
    Some((pool, transaction))
}

async fn random_uuid(c: &mut PgConnection) -> String {
    sqlx::query_scalar("SELECT gen_random_uuid()::text").fetch_one(c).await.unwrap()
}

/// Node의 `${prefix}-${randomUUID().slice(0, 12)}`.
async fn project_key(c: &mut PgConnection, prefix: &str) -> String {
    format!("{prefix}-{}", &random_uuid(c).await[..12])
}

async fn new_project(c: &mut PgConnection, prefix: &str, title: &str) -> Value {
    let key = project_key(c, prefix).await;
    create_project(&mut *c, &json!({ "projectKey": key, "title": title })).await.unwrap()
}

fn id(value: &Value) -> String {
    value["id"].as_str().expect("id").to_string()
}

async fn ticket(c: &mut PgConnection, body: Value) -> Value {
    create_ticket(c, &body).await.unwrap()
}

async fn update(c: &mut PgConnection, ticket_id: &str, body: Value) -> Value {
    update_ticket(c, ticket_id, &body).await.unwrap()
}

/// assert.rejects(…, /패턴/)
fn rejects_with<T: Debug>(result: BoardResult<T>, pattern: &str) {
    match result {
        Err(WorkBoardError::Rejected { message, .. }) if message.contains(pattern) => {}
        other => panic!("'{pattern}' 거부가 아님: {other:?}"),
    }
}

/// assert.rejects(…, WorkBoardError)
fn rejects<T: Debug>(result: BoardResult<T>) -> (u16, String) {
    match result {
        Err(WorkBoardError::Rejected { status, message }) => (status, message),
        other => panic!("WorkBoardError가 아님: {other:?}"),
    }
}

async fn board_ticket(c: &mut PgConnection, ticket_id: &str) -> Value {
    let board = list_board(&mut *c).await.unwrap();
    board["tickets"].as_array().unwrap().iter().find(|item| item["id"] == ticket_id).cloned().expect("목록에 티켓")
}

async fn activity(c: &mut PgConnection, ticket_id: &str) -> Vec<Value> {
    list_ticket_activity(&mut *c, ticket_id).await.unwrap()
}

#[tokio::test]
async fn 유형별_완료_조건과_사용자_검토_기록은_기존_미분류_완료를_소급하지_않는다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"lifecycle", "완료 규칙").await);
    let legacy = ticket(
        c,
        json!({ "projectId": project, "title": "이전 완료", "state": "done", "pushedCommitSha": "a".repeat(40) }),
    )
    .await;
    assert_eq!(legacy["ticketType"], "general");
    assert_eq!(legacy["state"], "done");
    let plan = id(&ticket(
        c,
        json!({ "projectId": project, "title": "기획", "ticketType": "planning", "completionCriteria": "결정문 승인" }),
    )
    .await);
    rejects_with(update_ticket(c, &plan, &json!({ "state": "done" })).await, "상태 전이");
    update(c, &plan, json!({ "state": "in_progress" })).await;
    update(c, &plan, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "범위와 선택지 문서 작성" }))
        .await;
    rejects_with(update_ticket(c, &plan, &json!({ "state": "done" })).await, "사용자 검토");
    rejects_with(
        update_ticket(c, &plan, &json!({ "state": "done", "userReview": "approved", "userReviewNote": "사용자가 선택지 승인" }))
            .await,
        "기록자",
    );
    let completed = update(
        c,
        &plan,
        json!({ "state": "done", "userReview": "approved", "userReviewNote": "사용자가 선택지 승인", "reportedActorId": "user" }),
    )
    .await;
    assert_eq!(completed["pushedCommitSha"], Value::Null);
    assert_eq!(completed["userReview"], "approved");
    assert!(completed["userReviewedAt"].is_string());
    assert!(
        activity(c, &plan)
            .await
            .iter()
            .any(|item| item["changes"]["userReview"]["after"] == "approved" && item["reportedActorId"] == "user")
    );
    rejects_with(
        update_ticket(c, &plan, &json!({ "description": "승인 뒤 변경", "userReview": "approved", "userReviewNote": "옛 승인" }))
            .await,
        "완료된 유형 티켓",
    );
    let implementation = id(&ticket(
        c,
        json!({ "projectId": project, "title": "구현", "ticketType": "implementation", "completionCriteria": "테스트 통과" }),
    )
    .await);
    update(c, &implementation, json!({ "state": "in_progress" })).await;
    update(
        c,
        &implementation,
        json!({ "state": "review", "completedById": "left-man", "completionEvidence": "코드와 로컬 테스트 결과" }),
    )
    .await;
    rejects_with(update_ticket(c, &implementation, &json!({ "ticketType": "pretest" })).await, "유형을 바꿀 수");
    rejects_with(update_ticket(c, &implementation, &json!({ "state": "done" })).await, "커밋 SHA");
    let done = update(c, &implementation, json!({ "state": "done", "pushedCommitSha": "b".repeat(40) })).await;
    assert_eq!(done["state"], "done");
    let pretest = id(&ticket(
        c,
        json!({ "projectId": project, "title": "사전테스트", "ticketType": "pretest", "completionCriteria": "실행 결과 기록" }),
    )
    .await);
    update(c, &pretest, json!({ "state": "in_progress" })).await;
    update(c, &pretest, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "재현 절차와 11/11 결과" }))
        .await;
    assert_eq!(update(c, &pretest, json!({ "state": "done" })).await["pushedCommitSha"], Value::Null);
    let operations = id(&ticket(
        c,
        json!({ "projectId": project, "title": "외부 운영", "ticketType": "operations", "completionCriteria": "실행 결과 확인" }),
    )
    .await);
    update(c, &operations, json!({ "state": "in_progress" })).await;
    update(
        c,
        &operations,
        json!({ "state": "review", "completedById": "left-man", "operationImpact": "external",
            "completionEvidence": "실행 로그와 영향 범위" }),
    )
    .await;
    rejects_with(update_ticket(c, &operations, &json!({ "state": "done" })).await, "사용자 검토");
    let approved = update(
        c,
        &operations,
        json!({ "state": "done", "userReview": "approved", "userReviewNote": "사용자 확인", "reportedActorId": "user" }),
    )
    .await;
    assert_eq!(approved["state"], "done");
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 미분류_검토_티켓은_진행_복귀_뒤에만_유형을_지정하고_생성_승인도_기록자를_요구한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"retype", "유형 변경 검증").await);
    let legacy = id(&ticket(
        c,
        json!({ "projectId": project, "title": "기존 검토", "state": "review", "completionCriteria": "실행 결과" }),
    )
    .await);
    rejects_with(
        update_ticket(
            c,
            &legacy,
            &json!({ "ticketType": "pretest", "state": "done", "completedById": "left-man", "completionEvidence": "11/11 통과" }),
        )
        .await,
        "유형을 바꿀 수",
    );
    assert_eq!(board_ticket(c, &legacy).await["state"], "review");
    update(c, &legacy, json!({ "state": "in_progress" })).await;
    update(c, &legacy, json!({ "ticketType": "pretest" })).await;
    update(c, &legacy, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "11/11 통과" })).await;
    let completed = update(c, &legacy, json!({ "state": "done" })).await;
    assert_eq!(completed["state"], "done");
    assert_eq!(completed["pushedCommitSha"], Value::Null);

    let approved = json!({ "projectId": project, "title": "기존 형식 승인", "state": "review",
        "userReview": "approved", "userReviewNote": "사용자 확인" });
    rejects_with(create_ticket(c, &approved).await, "기록자");
    let mut unknown = approved.clone();
    unknown["reportedActorId"] = json!("unknown-actor");
    rejects_with(create_ticket(c, &unknown).await, "등록된 직원");
    let mut by_user = approved.clone();
    by_user["reportedActorId"] = json!("user");
    let created = ticket(c, by_user).await;
    assert_eq!(created["userReview"], "approved");
    assert!(created["userReviewedAt"].is_string());
    assert!(
        activity(c, &id(&created))
            .await
            .iter()
            .any(|item| item["kind"] == "created" && item["reportedActorId"] == "user")
    );
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 불합격_독립검증은_자기_검증을_막고_대상의_완료_근거를_원자적으로_철회한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"verify", "독립검증").await);
    let target = id(&ticket(
        c,
        json!({ "projectId": project, "title": "대상", "ticketType": "implementation", "assigneeId": "left-man",
            "completionCriteria": "검증 통과" }),
    )
    .await);
    update(c, &target, json!({ "state": "in_progress" })).await;
    update(c, &target, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "코드 변경과 테스트" })).await;
    update(c, &target, json!({ "state": "done", "pushedCommitSha": "c".repeat(40) })).await;
    let check = id(&ticket(
        c,
        json!({ "projectId": project, "title": "독립검증", "ticketType": "verification",
            "completionCriteria": "판정 근거 기록", "targetTicketId": target }),
    )
    .await);
    update(c, &check, json!({ "state": "in_progress" })).await;
    update(c, &check, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "재현 결과 불일치" })).await;
    rejects_with(
        update_ticket(c, &check, &json!({ "state": "done", "verificationVerdict": "fail" })).await,
        "담당자·완료자",
    );
    assert_eq!(board_ticket(c, &target).await["state"], "done");
    update(c, &check, json!({ "completedById": "left-woman" })).await;
    assert_eq!(update(c, &check, json!({ "state": "done", "verificationVerdict": "fail" })).await["state"], "done");
    let reopened = board_ticket(c, &target).await;
    assert_eq!(reopened["state"], "in_progress");
    assert_eq!(reopened["pushedCommitSha"], Value::Null);
    assert_eq!(reopened["completedById"], Value::Null);
    assert_eq!(reopened["completionEvidence"], "");
    assert!(activity(c, &target).await.iter().any(|item| item["changes"]["state"]["after"] == "in_progress"));
    update(c, &target, json!({ "state": "review", "completedById": "left-man", "completionEvidence": "결함 수정과 재시험" })).await;
    update(c, &target, json!({ "state": "done", "pushedCommitSha": "d".repeat(40) })).await;
    let recheck = id(&ticket(
        c,
        json!({ "projectId": project, "title": "재검증", "ticketType": "verification",
            "completionCriteria": "수정 확인", "targetTicketId": target }),
    )
    .await);
    update(c, &recheck, json!({ "state": "in_progress" })).await;
    update(c, &recheck, json!({ "state": "review", "completedById": "left-woman", "completionEvidence": "독립 재현 11/11" })).await;
    update(c, &recheck, json!({ "state": "done", "verificationVerdict": "pass" })).await;
    let verified = board_ticket(c, &target).await;
    assert_eq!(verified["state"], "done");
    assert_eq!(verified["verifiedById"], "left-woman");
    assert!(activity(c, &target).await.iter().any(|item| item["changes"]["verifiedById"]["after"] == "left-woman"));
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 오피스_완료에는_푸시_커밋_근거가_필요하고_toss는_별도_정책을_쓴다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let office = id(&new_project(c,"delivery", "오피스 반영").await);
    rejects_with(
        create_ticket(c, &json!({ "projectId": office, "title": "근거 없는 완료", "state": "done" })).await,
        "커밋 SHA",
    );
    let review = id(&ticket(c, json!({ "projectId": office, "title": "검토" })).await);
    update(c, &review, json!({ "state": "review" })).await;
    rejects(update_ticket(c, &review, &json!({ "state": "done" })).await);
    let sha = "a".repeat(40);
    let completed = update(c, &review, json!({ "state": "done", "pushedCommitSha": sha })).await;
    assert_eq!(completed["state"], "done");
    assert_eq!(completed["pushedCommitSha"], sha);
    rejects(update_ticket(c, &review, &json!({ "pushedCommitSha": null })).await);
    let toss: String = sqlx::query_scalar("SELECT id::text FROM work_board_projects WHERE project_key = 'toss-trading'")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    let separate = ticket(c, json!({ "projectId": toss, "title": "별도 저장소", "state": "done" })).await;
    assert_eq!(separate["pushedCommitSha"], Value::Null);
    let studio: Option<String> =
        sqlx::query_scalar("SELECT id::text FROM work_board_projects WHERE project_key = 'one-click-studio'")
            .fetch_optional(&mut *c)
            .await
            .unwrap();
    let studio = match studio {
        Some(studio) => studio,
        None => id(&create_project(&mut *c, &json!({ "projectKey": "one-click-studio", "title": "딸깍 스튜디오" }))
            .await
            .unwrap()),
    };
    let no_remote = ticket(c, json!({ "projectId": studio, "title": "원격 없는 저장소", "state": "done" })).await;
    assert_eq!(no_remote["pushedCommitSha"], Value::Null);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 진행_메모와_티켓_수정_이력을_누적하고_결정_대기를_상태와_분리한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"activity", "이력 검증").await);
    let tts = id(&ticket(c, json!({ "projectId": project, "title": "TTS", "state": "open" })).await);
    let first = add_ticket_activity(
        c,
        &tts,
        &json!({ "kind": "progress", "body": "첫 측정 2.1초", "reportedActorId": "left-woman" }),
    )
    .await
    .unwrap();
    add_ticket_activity(c, &tts, &json!({ "kind": "verification", "body": "두 번째 측정 1.8초" })).await.unwrap();
    update(c, &tts, json!({ "state": "deferred", "decisionPending": true })).await;
    update(c, &tts, json!({ "state": "deferred" })).await;
    let events = activity(c, &tts).await;
    assert_eq!(events.len(), 4, "생성, 메모 2건, 실제 변경 1건");
    assert_eq!(events.iter().find(|item| item["id"] == first["id"]).unwrap()["reportedActorId"], "left-woman");
    let change = events.iter().find(|item| item["kind"] == "changed").unwrap();
    assert_eq!(change["changes"]["state"], json!({ "before": "open", "after": "deferred" }));
    assert_eq!(change["changes"]["decisionPending"], json!({ "before": false, "after": true }));
    assert_eq!(board_ticket(c, &tts).await["decisionPending"], true);
    let missing = random_uuid(c).await;
    assert_eq!(rejects(list_ticket_activity(&mut *c, &missing).await).0, 404);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 티켓과_실제_업무_기록을_연결하고_해제하되_본문은_목록에서_제외한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let repository: String =
        sqlx::query_scalar("SELECT id::text FROM projects LIMIT 1").fetch_one(&mut *c).await.unwrap();
    let record: String = sqlx::query_scalar(
        "INSERT INTO work_records (project_id, record_type, title, body)
       VALUES ($1::uuid, 'result', 'TTS 확인 결과', '상세 본문') RETURNING id::text",
    )
    .bind(&repository)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    let project = id(&new_project(c,"record", "연결 검증").await);
    let linked_ticket = ticket(c, json!({ "projectId": project, "title": "음성 확인", "workRecordIds": [record] })).await;
    assert_eq!(linked_ticket["workRecordIds"], json!([record]));
    let ticket_id = id(&linked_ticket);
    let snapshot = list_board(&mut *c).await.unwrap();
    let linked = snapshot["workRecords"].as_array().unwrap().iter().find(|item| item["id"] == record).unwrap();
    assert_eq!(linked["ticketId"], ticket_id);
    assert_eq!(linked["title"], "TTS 확인 결과");
    assert!(linked.get("body").is_none());
    assert_eq!(linked["bodyPreview"], "상세 본문");
    let detail = read_board_work_record(&mut *c, &record).await.unwrap();
    assert_eq!(detail["body"], "상세 본문");
    let missing = random_uuid(c).await;
    assert_eq!(rejects(read_board_work_record(&mut *c, &missing).await).0, 404);
    let unknown = random_uuid(c).await;
    rejects_with(update_ticket(c, &ticket_id, &json!({ "workRecordIds": [unknown] })).await, "없는 업무 기록");
    assert_eq!(board_ticket(c, &ticket_id).await["workRecordIds"], json!([record]));
    update(c, &ticket_id, json!({ "workRecordIds": [] })).await;
    assert_eq!(board_ticket(c, &ticket_id).await["workRecordIds"], json!([]));
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 프로젝트_키_중복을_거부한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let key = project_key(c, "board-test").await;
    create_project(&mut *c, &json!({ "projectKey": key, "title": "검증 프로젝트" })).await.unwrap();
    // 중복 INSERT는 트랜잭션을 실패 상태로 만들므로 이 뒤로 질의하지 않는다.
    let (status, message) = rejects(create_project(&mut *c, &json!({ "projectKey": key, "title": "중복" })).await);
    assert_eq!((status, message.as_str()), (409, "이미 있는 프로젝트 키입니다."));
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn 프로젝트_티켓의_실제_저장_상태_변경_담당_미정을_조회한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"board-test", "검증 프로젝트").await);
    let created = ticket(
        c,
        json!({ "projectId": project, "title": "완료 조건 확인", "completionCriteria": "사용자가 직접 확인", "dueDate": null }),
    )
    .await;
    assert_eq!(created["state"], "open");
    assert_eq!(created["assigneeId"], Value::Null);
    let updated =
        update(c, &id(&created), json!({ "state": "deferred", "dueDate": "2026-10-01", "description": "외부 결정 대기" })).await;
    assert_eq!(updated["state"], "deferred");
    assert_eq!(updated["dueDate"], "2026-10-01");
    let snapshot = list_board(&mut *c).await.unwrap();
    let stored = snapshot["tickets"].as_array().unwrap().iter().find(|item| item["id"] == created["id"]).unwrap();
    assert_eq!(stored["description"], "외부 결정 대기");
    let listed = snapshot["projects"].as_array().unwrap().iter().find(|item| item["id"] == project).unwrap();
    assert_eq!(listed["title"], "검증 프로젝트");
    let missing = random_uuid(c).await;
    assert_eq!(rejects(update_ticket(c, &missing, &json!({ "state": "done" })).await).0, 404);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn wbs_부모_선행_순환과_다른_프로젝트_연결은_거부하고_목표_연결은_보존한다() {
    let Some((_pool, mut transaction)) = begin().await else { return };
    let c = &mut *transaction;
    let project = id(&new_project(c,"wbs", "WBS 검증").await);
    let other = id(&new_project(c, "wbs", "다른 프로젝트").await);
    let goal = create_goal(
        c,
        &json!({ "projectId": project, "horizon": "weekly", "periodStart": "2026-09-21", "title": "한 주 목표",
            "metricName": "완료 건수", "targetValue": "5" }),
    )
    .await
    .unwrap();
    assert_eq!(goal["actualValue"], Value::Null);
    let goal_id = id(&goal);
    let root = id(&ticket(c, json!({ "projectId": project, "title": "상위" })).await);
    let child = ticket(
        c,
        json!({ "projectId": project, "title": "하위", "parentTicketId": root, "predecessorIds": [root], "goalIds": [goal_id] }),
    )
    .await;
    assert_eq!(child["parentTicketId"], root);
    assert_eq!(child["predecessorIds"], json!([root]));
    assert_eq!(child["goalIds"], json!([goal_id]));
    let child = id(&child);
    rejects_with(update_ticket(c, &root, &json!({ "parentTicketId": child })).await, "순환");
    rejects_with(update_ticket(c, &root, &json!({ "predecessorIds": [child] })).await, "순환");
    let foreign = id(&ticket(c, json!({ "projectId": other, "title": "다른 티켓" })).await);
    rejects(update_ticket(c, &root, &json!({ "parentTicketId": foreign })).await);
    rejects(update_ticket(c, &root, &json!({ "predecessorIds": [foreign] })).await);
    rejects(update_ticket(c, &foreign, &json!({ "goalIds": [goal_id] })).await);
    let revised = update_goal(c, &goal_id, &json!({ "actualValue": "2" })).await.unwrap();
    assert_eq!(revised["targetValue"], "5.0000");
    assert_eq!(revised["actualValue"], "2.0000");
    let snapshot = list_board(&mut *c).await.unwrap();
    let stored = snapshot["tickets"].as_array().unwrap().iter().find(|item| item["id"] == child).unwrap();
    assert_eq!(stored["parentTicketId"], root);
    assert_eq!(stored["predecessorIds"], json!([root]));
    assert_eq!(stored["goalIds"], json!([goal_id]));
    let stored_goal = snapshot["goals"].as_array().unwrap().iter().find(|item| item["id"] == goal_id).unwrap();
    assert_eq!(stored_goal["periodStart"], "2026-09-21");
    update(c, &child, json!({ "parentTicketId": null, "predecessorIds": [], "goalIds": [] })).await;
    let cleared = board_ticket(c, &child).await;
    assert_eq!(cleared["parentTicketId"], Value::Null);
    assert_eq!(cleared["predecessorIds"], json!([]));
    assert_eq!(cleared["goalIds"], json!([]));
    transaction.rollback().await.unwrap();
}

async fn call(pool: &PgPool, request: Request<Body>) -> (StatusCode, Value) {
    let state = AppState {
        pool: pool.clone(),
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    };
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn json_request(method: &str, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().unwrap().keys().map(String::as_str).collect()
}

fn is_iso_millis(value: &Value) -> bool {
    let text = value.as_str().unwrap_or_default();
    let bytes = text.as_bytes();
    bytes.len() == 24
        && text.ends_with('Z')
        && bytes[10] == b'T'
        && bytes[19] == b'.'
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            13 | 16 => *byte == b':',
            10 | 19 | 23 => true,
            _ => byte.is_ascii_digit(),
        })
}

/// HTTP 경로로 만들고 고친 뒤, 응답 감싸기·키 순서·시각 형식을 Node 열 목록과 비교한다. 만든 행은 끝에 지운다.
#[tokio::test]
async fn http_경로는_node와_같은_감싸기와_키_순서와_시각_형식을_쓴다() {
    let Some(pool) = test_pool().await else { return };
    let mut connection = pool.acquire().await.unwrap();
    let key = project_key(&mut connection, "http").await;
    drop(connection);
    let (status, body) =
        call(&pool, json_request("POST", "/api/work-board/projects", &json!({ "projectKey": key, "title": " HTTP " }))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(keys(&body), ["project"]);
    let project = body["project"].clone();
    assert_eq!(keys(&project), ["id", "projectKey", "title", "description", "createdAt", "updatedAt"]);
    assert_eq!(project["title"], "HTTP");
    assert!(is_iso_millis(&project["createdAt"]), "{}", project["createdAt"]);
    let project_id = id(&project);

    let checks = {
        let pool = pool.clone();
        let project_id = project_id.clone();
        async move { http_round_trip(&pool, &project_id).await }
    };
    let outcome = tokio::spawn(checks).await;
    for sql in [
        "DELETE FROM work_board_tickets WHERE project_id = $1::uuid",
        "DELETE FROM work_board_goals WHERE project_id = $1::uuid",
        "DELETE FROM work_board_projects WHERE id = $1::uuid",
    ] {
        sqlx::query(sql).bind(&project_id).execute(&pool).await.unwrap();
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn http_round_trip(pool: &PgPool, project_id: &str) {
    {
        let (status, body) = call(
            &pool,
            json_request("POST", "/api/work-board/tickets", &json!({ "projectId": project_id, "title": "HTTP 티켓" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let ticket = body["ticket"].clone();
        assert_eq!(
            keys(&ticket),
            [
                "id", "projectId", "title", "description", "assigneeId", "completedById", "verifiedById", "state",
                "pushedCommitSha", "ticketType", "completionEvidence", "userReview", "userReviewNote", "userReviewedAt",
                "targetTicketId", "verificationVerdict", "operationImpact", "resolutionReason", "dueDate",
                "completionCriteria", "decisionPending", "parentTicketId", "createdAt", "updatedAt", "predecessorIds",
                "goalIds", "workRecordIds",
            ]
        );
        let ticket_id = id(&ticket);
        let path = format!("/api/work-board/tickets/{ticket_id}");
        let (status, body) = call(&pool, json_request("PATCH", &path, &json!({ "assigneeId": "nobody-here" }))).await;
        assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({ "error": "담당자가 없습니다." })));
        let (status, body) = call(&pool, json_request("PATCH", &path, &json!({ "state": "in_progress" }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ticket"]["state"], "in_progress");

        let activity_path = format!("/api/work-board/tickets/{ticket_id}/activity");
        let (status, body) =
            call(&pool, json_request("POST", &activity_path, &json!({ "kind": "progress", "body": "HTTP 메모" }))).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            keys(&body["activity"]),
            ["id", "ticketId", "kind", "body", "changes", "reportedActorId", "createdAt", "summary"]
        );
        let (status, body) = call(&pool, Request::get(&activity_path).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let summaries: Vec<&str> = body["activity"].as_array().unwrap().iter().map(|item| item["summary"].as_str().unwrap()).collect();
        assert_eq!(summaries, ["HTTP 메모", "상태: \"open\" → \"in_progress\"", "티켓 생성"]);

        let (status, body) = call(
            &pool,
            json_request(
                "POST",
                "/api/work-board/goals",
                &json!({ "projectId": project_id, "horizon": "monthly", "periodStart": "2026-10-01", "title": "HTTP 목표" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let goal = body["goal"].clone();
        assert_eq!(
            keys(&goal),
            [
                "id", "projectId", "horizon", "periodStart", "title", "description", "metricName", "metricUnit",
                "targetValue", "actualValue", "createdAt", "updatedAt",
            ]
        );
        let (status, body) =
            call(&pool, json_request("PATCH", &format!("/api/work-board/goals/{}", id(&goal)), &json!({ "title": "고침" }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["goal"]["title"], "고침");

        let (status, body) =
            call(&pool, json_request("PATCH", &format!("/api/work-board/projects/{project_id}"), &json!({ "description": "설명" })))
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["project"]["description"], "설명");

        let (status, body) = call(&pool, Request::get("/api/work-board").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(keys(&body), ["projects", "goals", "workRecords", "tickets"]);
        let missing = format!("/api/work-board/tickets/{}", "0".repeat(8) + "-0000-4000-8000-" + &"0".repeat(12));
        let (status, body) = call(&pool, json_request("PATCH", &missing, &json!({ "state": "done" }))).await;
        assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "티켓이 없습니다." })));
    }
}
