// 위키 제안 수명주기와 게시·검색 분리, 위키 HTTP 경로 테스트. Node wiki-knowledge.test.mjs를 옮겼다. DB가 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::wiki_knowledge::*;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use tokio::sync::OnceCell;
use tower::ServiceExt;

static READY: OnceCell<()> = OnceCell::const_new();

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
        })
        .await;
    Some(pool)
}

fn state(pool: PgPool, repository_root: Option<&str>) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, repository_root.map(Into::into))),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    }
}

fn state_without_db() -> AppState {
    state(db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap(), None)
}

async fn call(state: &AppState, request: Request<Body>) -> (StatusCode, Value) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn json_request(method: &str, path: &str, decision: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(decision) = decision {
        builder = builder.header("x-officestra-user-decision", decision);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

/// Node 테스트의 `error instanceof WikiKnowledgeError && error.statusCode === N`.
fn knowledge_status<T>(result: Result<T, WikiError>) -> u16 {
    match result {
        Err(error @ WikiError::Knowledge { .. }) => error.status(),
        Err(error) => panic!("WikiKnowledgeError가 아닙니다: {error:?}"),
        Ok(_) => panic!("오류가 나야 합니다"),
    }
}

async fn scalar(connection: &mut PgConnection, sql: &str, value: &str) -> Vec<Value> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(value).fetch_all(connection).await.unwrap()
}

async fn insert_character(connection: &mut PgConnection, id: &str) {
    sqlx::query(
        "INSERT INTO characters (id, name, seat, backend, model, effort, permission, identity_prompt, config)
         VALUES ($1, $1, '격리 좌석', 'codex', 'gpt-5.6-sol', 'high', 'workspace-write', '', '{}'::jsonb)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .execute(connection)
    .await
    .unwrap();
}

async fn insert_project(connection: &mut PgConnection, name: &str, root: &str) -> String {
    sqlx::query_scalar("INSERT INTO projects (name, repository_root) VALUES ($1, $2) RETURNING id::text")
        .bind(name)
        .bind(root)
        .fetch_one(connection)
        .await
        .unwrap()
}

async fn insert_source(connection: &mut PgConnection, project: &str, title: &str, author: &str, metadata: &str) -> String {
    sqlx::query_scalar(
        "INSERT INTO work_records (project_id, record_type, lifecycle_state, title, body, character_id, attribution, metadata)
         VALUES ($1::uuid, 'result', 'active', $2, '근거 본문', $3, 'character', $4::jsonb)
         RETURNING id::text",
    )
    .bind(project)
    .bind(title)
    .bind(author)
    .bind(metadata)
    .fetch_one(connection)
    .await
    .unwrap()
}

/// Node 테스트의 createWikiProposal 인자. 상태는 Node 기본값 drafted다.
fn proposal<'a>(
    project: &'a str,
    page_key: &'a Value,
    grade: &'a str,
    title: &'a Value,
    body: Option<&'a Value>,
    sources: &'a Value,
    author: &'a str,
) -> NewProposal<'a> {
    NewProposal {
        project_id: Some(project),
        repository_root: None,
        page_key: Some(page_key),
        approval_grade: grade,
        state: "drafted",
        kind: None,
        ordinal: None,
        draft_title: Some(title),
        draft_body: body,
        source_work_record_ids: Some(sources),
        author_character_id: Some(author),
        author_turn_id: None,
        base_version: None,
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_else(|| panic!("문자열이 아닙니다: {value}"))
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn source_ids(page: &Value) -> Vec<String> {
    page["sources"].as_array().unwrap().iter().map(|source| text(&source["workRecordId"]).to_string()).collect()
}

#[tokio::test]
async fn 격리_postgresql_제안_생성·검증·승인·게시·cas_충돌·검색_분리() {
    let Some(pool) = test_pool().await else { return };
    let mut transaction = pool.begin().await.unwrap();
    let c = &mut *transaction;
    let suffix: String = sqlx::query_scalar("SELECT left(gen_random_uuid()::text, 8)").fetch_one(&mut *c).await.unwrap();
    let repository_root = format!("/tmp/wiki-test-{suffix}");
    let other_root = format!("{repository_root}-other");
    let (author, reviewer) = (format!("wiki-author-{suffix}"), format!("wiki-reviewer-{suffix}"));
    let page_key = json!(format!("incident-{suffix}"));
    for id in [&author, &reviewer] {
        insert_character(c, id).await;
    }
    let project = insert_project(c, &format!("wiki-test-{suffix}"), &repository_root).await;
    let other_project = insert_project(c, &format!("wiki-test-other-{suffix}"), &other_root).await;
    let source_a = insert_source(c, &project, "근거 A", &author, "{}").await;
    let source_b = insert_source(c, &project, "근거 B", &author, "{}").await;
    let foreign = insert_source(c, &other_project, "외부 근거", &author, "{}").await;
    let incident = json!("incident");
    let both = json!([source_a, source_b]);

    // 1) peer 등급 수명주기: drafted → peer_verified → published v1.
    let title = json!("침해 사건 요약");
    let body = json!("SSH root 침해와 채굴기 제거 사건의 합의본.");
    let created = create_wiki_proposal(
        c,
        NewProposal { kind: Some(&incident), ..proposal(&project, &page_key, "peer", &title, Some(&body), &both, &author) },
    )
    .await
    .unwrap();
    assert_eq!(created["state"], "drafted");
    assert_eq!(created["pageKey"], page_key);
    assert_eq!(created["kind"], "incident");
    assert_eq!(created["approvalTier"], "peer");
    let created_sources: Vec<String> =
        created["sourceRecordIds"].as_array().unwrap().iter().map(|id| text(id).to_string()).collect();
    assert_eq!(sorted(created_sources), sorted(vec![source_a.clone(), source_b.clone()]));
    assert_eq!(created["baseVersion"], 0);
    let created_id = text(&created["id"]).to_string();

    assert_eq!(knowledge_status(verify_wiki_proposal(c, &created_id, Some(&json!(author)), None).await), 403);
    let verified = verify_wiki_proposal(c, &created_id, Some(&json!(reviewer)), None).await.unwrap();
    assert_eq!(verified["state"], "peer_verified");
    assert_eq!(verified["verifierCharacterId"], json!(reviewer));

    let approved = approve_wiki_proposal(c, &created_id, "character", Some(&reviewer), None).await.unwrap();
    assert!(!approved.conflicted);
    assert_eq!(approved.proposal["state"], "published");
    let page_id = text(&approved.proposal["targetRecordId"]).to_string();

    let page = get_wiki_page(&mut *c, &page_id, None).await.unwrap().unwrap();
    assert_eq!(page["pageKey"], page_key);
    assert_eq!(page["kind"], "incident");
    assert_eq!(page["version"], 1);
    assert_eq!(sorted(source_ids(&page)), sorted(vec![source_a.clone(), source_b.clone()]));

    // 게시본만 RAG에 파생되고, 원본 검색에서는 제외되며 위키 뷰에는 나타난다.
    let rag = "SELECT to_jsonb(id) FROM rag_documents WHERE work_record_id = $1::uuid";
    assert_eq!(scalar(c, rag, &page_id).await.len(), 1);
    let record_search = "SELECT to_jsonb(1) FROM searchable_rag_documents WHERE work_record_id = $1::uuid";
    assert_eq!(scalar(c, record_search, &page_id).await.len(), 0);
    let wiki_search = "SELECT to_jsonb(1) FROM searchable_wiki_page_documents WHERE work_record_id = $1::uuid";
    assert_eq!(scalar(c, wiki_search, &page_id).await.len(), 1);
    let searched = list_wiki_pages(&mut *c, Some("침해 사건"), None, None).await.unwrap();
    assert!(searched.iter().any(|row| row["id"] == json!(page_id)));
    assert_eq!(list_wiki_pages(&mut *c, None, None, Some(&other_root)).await.unwrap(), Vec::<Value>::new());
    assert_eq!(get_wiki_page(&mut *c, &page_id, Some(&other_root)).await.unwrap(), None);

    // 2) auto 등급 재게시: base 1 → published v2.
    let only_a = json!([source_a]);
    let revised_body = json!("재부팅 후 점검 결과를 추가한 개정본.");
    let second = create_wiki_proposal(
        c,
        NewProposal { kind: Some(&incident), ..proposal(&project, &page_key, "auto", &title, Some(&revised_body), &only_a, &author) },
    )
    .await
    .unwrap();
    assert_eq!(second["baseVersion"], 1);
    let second_approved = approve_wiki_proposal(c, text(&second["id"]), "system", None, None).await.unwrap();
    assert_eq!(second_approved.proposal["state"], "published");
    let revised = get_wiki_page(&mut *c, &page_id, None).await.unwrap().unwrap();
    assert_eq!(revised["version"], 2);
    assert!(text(&revised["body"]).contains("재부팅"));
    assert_eq!(source_ids(&revised), vec![source_a.clone()]);

    // 3) 낡은 base version은 게시되지 않고 conflict로 종결된다.
    let (stale_title, stale_body, only_b, base) =
        (json!("낡은 개정"), json!("버전 1 기준으로 작성된 초안."), json!([source_b]), json!(1));
    let stale = create_wiki_proposal(
        c,
        NewProposal {
            kind: Some(&incident),
            base_version: Some(&base),
            ..proposal(&project, &page_key, "auto", &stale_title, Some(&stale_body), &only_b, &author)
        },
    )
    .await
    .unwrap();
    let stale_outcome = approve_wiki_proposal(c, text(&stale["id"]), "system", None, None).await.unwrap();
    assert!(stale_outcome.conflicted);
    assert_eq!(stale_outcome.proposal["state"], "conflict");
    let unchanged = get_wiki_page(&mut *c, &page_id, None).await.unwrap().unwrap();
    assert_eq!(unchanged["version"], 2);
    assert!(text(&unchanged["body"]).contains("재부팅"));

    // 4) user 등급은 pending_user에서 사용자만 게시할 수 있다.
    let user_body = json!("사용자 선호를 반영한 개정본.");
    let user_grade = create_wiki_proposal(
        c,
        NewProposal { kind: Some(&incident), ..proposal(&project, &page_key, "user", &title, Some(&user_body), &both, &author) },
    )
    .await
    .unwrap();
    let user_id = text(&user_grade["id"]).to_string();
    let pending_user = verify_wiki_proposal(c, &user_id, Some(&json!(reviewer)), None).await.unwrap();
    assert_eq!(pending_user["state"], "pending_user");
    assert_eq!(
        knowledge_status(approve_wiki_proposal(c, &user_id, "character", Some(&reviewer), None).await.map(|outcome| outcome.proposal)),
        403
    );
    let user_approved = approve_wiki_proposal(c, &user_id, "user", None, None).await.unwrap();
    assert_eq!(user_approved.proposal["state"], "published");
    assert_eq!(get_wiki_page(&mut *c, &page_id, None).await.unwrap().unwrap()["version"], 3);

    // 5) 근거 규칙: 자기참조(synthesis)와 다른 프로젝트 근거는 거부.
    for (key, title, sources, status) in [
        (format!("self-{suffix}"), "자기참조", json!([page_id]), 422),
        (format!("foreign-{suffix}"), "외부 근거", json!([foreign]), 422),
        (format!("empty-{suffix}"), "근거 없음", json!([]), 400),
    ] {
        let (key, title) = (json!(key), json!(title));
        let result = create_wiki_proposal(
            c,
            NewProposal { kind: Some(&incident), ..proposal(&project, &key, "auto", &title, None, &sources, &author) },
        )
        .await;
        assert_eq!(knowledge_status(result), status, "{key}");
    }
    let unapproved = insert_source(c, &project, "승인 전 근거", &author, r#"{"review":{"status":"awaiting_approval"}}"#).await;
    let (blocked_key, blocked_title, decision, blocked_sources) =
        (json!(format!("unapproved-{suffix}")), json!("승인 전 게시 금지"), json!("decision"), json!([unapproved]));
    let blocked = create_wiki_proposal(
        c,
        NewProposal { kind: Some(&decision), ..proposal(&project, &blocked_key, "auto", &blocked_title, None, &blocked_sources, &author) },
    )
    .await
    .unwrap();
    assert_eq!(
        knowledge_status(approve_wiki_proposal(c, text(&blocked["id"]), "system", None, None).await.map(|outcome| outcome.proposal)),
        422
    );

    // 6) 초안은 work_records를 만들지 않으므로 어떤 검색에도 없다.
    let (draft_key, draft_title, draft_body) = (format!("draft-only-{suffix}"), json!("초안 전용"), json!("게시 전 초안 본문"));
    let draft_only = create_wiki_proposal(
        c,
        NewProposal {
            kind: Some(&decision),
            ..proposal(&project, &json!(draft_key), "peer", &draft_title, Some(&draft_body), &only_a, &author)
        },
    )
    .await
    .unwrap();
    let synthesis = "SELECT to_jsonb(1) FROM work_records WHERE record_type = 'synthesis' AND metadata->>'pageKey' = $1";
    assert_eq!(scalar(c, synthesis, &draft_key).await.len(), 0);

    // 7) 거절과 종결 후 재조작 금지.
    let draft_id = text(&draft_only["id"]).to_string();
    let rejected = reject_wiki_proposal(c, &draft_id, Some(&json!("초안 품질 미달")), None).await.unwrap();
    assert_eq!(rejected["state"], "rejected");
    assert_eq!(rejected["reason"], "초안 품질 미달");
    assert_eq!(knowledge_status(verify_wiki_proposal(c, &draft_id, Some(&json!(reviewer)), None).await), 409);
    let published = list_wiki_proposals(&mut *c, Some("published"), None).await.unwrap();
    assert!(published.iter().any(|row| row["id"] == json!(created_id)));

    // 8) 사용자 삭제: archived로 바뀌어 목록·검색·단건 조회에서 사라지고, 같은 키의 제안이 다시 승인되면 active로 돌아온다.
    let archived = archive_wiki_page(c, &page_id, None).await.unwrap();
    assert_eq!(archived["lifecycleState"], "archived");
    assert_eq!(get_wiki_page(&mut *c, &page_id, None).await.unwrap(), None);
    assert!(!list_wiki_pages(&mut *c, None, None, None).await.unwrap().iter().any(|row| row["id"] == json!(page_id)));
    assert!(!list_wiki_pages(&mut *c, Some("침해 사건"), None, None).await.unwrap().iter().any(|row| row["id"] == json!(page_id)));
    assert_eq!(knowledge_status(archive_wiki_page(c, &page_id, None).await), 404);
    let event: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('eventType', event_type, 'actorType', actor_type, 'next', next_value)
         FROM work_record_events WHERE record_id = $1::uuid ORDER BY record_version DESC LIMIT 1",
    )
    .bind(&page_id)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(event["eventType"], "state_changed");
    assert_eq!(event["actorType"], "user");
    assert_eq!(event["next"]["lifecycleState"], "archived");

    let (revival_title, revival_body, four) = (json!("삭제 뒤 다시 게시"), json!("다시 살아난 본문"), json!(4));
    let revival = create_wiki_proposal(
        c,
        NewProposal {
            kind: Some(&incident),
            base_version: Some(&four),
            ..proposal(&project, &page_key, "auto", &revival_title, Some(&revival_body), &only_a, &author)
        },
    )
    .await
    .unwrap();
    let revived = approve_wiki_proposal(c, text(&revival["id"]), "system", None, None).await.unwrap();
    assert_eq!(revived.proposal["state"], "published");
    assert_eq!(get_wiki_page(&mut *c, &page_id, None).await.unwrap().unwrap()["title"], "삭제 뒤 다시 게시");
    transaction.rollback().await.unwrap();
}

const PROPOSAL: &str = "00000000-0000-4000-8000-000000000001";

#[tokio::test]
async fn 위키_변경_경로는_본문보다_출처와_사용자_결정을_먼저_본다() {
    let state = state_without_db();
    let plain = Request::post("/api/wiki/proposals").header(header::CONTENT_TYPE, "text/plain").body(Body::from("{}")).unwrap();
    assert_eq!(call(&state, plain).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let foreign = Request::post(format!("/api/wiki/proposals/{PROPOSAL}/verify"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "https://example.com")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(call(&state, foreign).await.0, StatusCode::FORBIDDEN);
    let decision_required = json!({ "error": "사내 위키 화면에서 사용자가 직접 결정해야 합니다." });
    let mismatched = [None, Some("reject:x".to_string()), Some(format!("approve:{PROPOSAL}"))];
    for (action, decision) in ["approve", "approve", "reject"].into_iter().zip(mismatched) {
        let path = format!("/api/wiki/proposals/{PROPOSAL}/{action}");
        let request = json_request("POST", &path, decision.as_deref(), "not json");
        assert_eq!(call(&state, request).await, (StatusCode::FORBIDDEN, decision_required.clone()), "{action}");
    }
    let delete = json_request("DELETE", &format!("/api/wiki/pages/{PROPOSAL}"), Some(&format!("approve:{PROPOSAL}")), "");
    assert_eq!(call(&state, delete).await, (StatusCode::FORBIDDEN, decision_required.clone()));

    // 헤더 검사 뒤에 본문을 읽고, null 본문은 Node처럼 속성 읽기 오류다.
    let verify = json_request("POST", &format!("/api/wiki/proposals/{PROPOSAL}/verify"), None, "null");
    let null_body = json!({ "error": "Cannot read properties of null (reading 'characterId')" });
    assert_eq!(call(&state, verify).await, (StatusCode::INTERNAL_SERVER_ERROR, null_body));
    let create = json_request("POST", "/api/wiki/proposals", None, "null");
    let null_tier = json!({ "error": "Cannot read properties of null (reading 'approvalTier')" });
    assert_eq!(call(&state, create).await, (StatusCode::INTERNAL_SERVER_ERROR, null_tier));
    let peer = json_request("POST", "/api/wiki/proposals", None, r#"{"approvalTier":"peer"}"#);
    let user_only = json!({ "error": "공개 제안 API는 사용자 승인 등급만 만들 수 있습니다." });
    assert_eq!(call(&state, peer).await, (StatusCode::BAD_REQUEST, user_only));
}

#[tokio::test]
async fn 위키_경로는_36자_id만_받고_다른_메서드는_404다() {
    let state = state_without_db();
    let not_found = json!({ "error": "경로를 찾을 수 없습니다." });
    for request in [
        get("/api/wiki/pages/not-a-uuid"),
        get(&format!("/api/wiki/pages/{PROPOSAL}/extra")),
        get(&format!("/api/wiki/pages/%30{}", &PROPOSAL[1..])),
        json_request("PUT", &format!("/api/wiki/pages/{PROPOSAL}"), None, "{}"),
        json_request("POST", &format!("/api/wiki/proposals/{PROPOSAL}/publish"), None, "{}"),
        json_request("PATCH", "/api/wiki/proposals", None, "{}"),
        get(&format!("/api/wiki/proposals/{PROPOSAL}/approve")),
        json_request("GET", "/api/rag/search", None, "{}"),
    ] {
        let path = request.uri().to_string();
        assert_eq!(call(&state, request).await, (StatusCode::NOT_FOUND, not_found.clone()), "{path}");
    }
}

#[tokio::test]
async fn node와_같이_대소문자가_다른_동작_이름은_사용자_결정_헤더를_건너뛴다() {
    // Node routeWikiProposalAction은 /i로 맞추고 헤더 검사는 소문자 approve·reject만 본다(보고한 결함을 그대로 옮김).
    // 여기서는 DB가 없으므로 헤더 검사를 통과해 승인 분기의 DB 접속 단계까지 간 것(403이 아님)만 확인한다.
    let state = state_without_db();
    for action in ["APPROVE", "Reject", "Verify"] {
        let request = json_request("POST", &format!("/api/wiki/proposals/{}/{action}", PROPOSAL.to_uppercase()), None, "{}");
        assert_eq!(call(&state, request).await.0, StatusCode::INTERNAL_SERVER_ERROR, "{action}");
    }
}

#[tokio::test]
async fn 위키_http_흐름은_node와_같은_상태_코드와_본문_키를_돌려준다() {
    let Some(pool) = test_pool().await else { return };
    let mut setup = pool.acquire().await.unwrap();
    let suffix: String = sqlx::query_scalar("SELECT left(gen_random_uuid()::text, 8)").fetch_one(&mut *setup).await.unwrap();
    let root = format!("/tmp/wiki-http-{suffix}");
    let author = format!("wiki-http-author-{suffix}");
    insert_character(&mut setup, &author).await;
    let project = insert_project(&mut setup, &format!("wiki-http-{suffix}"), &root).await;
    let source = insert_source(&mut setup, &project, "HTTP 근거", &author, "{}").await;
    let state = state(pool.clone(), Some(&root));
    let page_key = format!("http-{suffix}");

    let invalid_state = call(&state, get("/api/wiki/proposals?state=bogus")).await;
    assert_eq!(invalid_state, (StatusCode::BAD_REQUEST, json!({ "error": "알 수 없는 제안 상태입니다. bogus" })));
    let body = json!({ "pageKey": page_key, "kind": "decision", "title": "HTTP 결정", "body": "HTTP 본문", "sourceRecordIds": [source] });
    let (status, created) = call(&state, json_request("POST", "/api/wiki/proposals", None, &body.to_string())).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!((&created["proposal"]["state"], &created["proposal"]["approvalTier"]), (&json!("pending_user"), &json!("user")));
    let id = text(&created["proposal"]["id"]).to_string();
    let (status, pending) = call(&state, get("/api/wiki/proposals?state=pending_user")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(pending["proposals"].as_array().unwrap().iter().any(|row| row["id"] == json!(id)));

    let approve = format!("/api/wiki/proposals/{id}/approve");
    let (status, approved) = call(&state, json_request("POST", &approve, Some(&format!("approve:{id}")), "")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approved["proposal"]["state"], "published");
    let page_id = text(&approved["proposal"]["targetRecordId"]).to_string();
    let (status, pages) = call(&state, get("/api/wiki/pages?query=HTTP&limit=5")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(pages["pages"].as_array().unwrap().iter().any(|page| page["id"] == json!(page_id)));
    let (status, page) = call(&state, get(&format!("/api/wiki/pages/{}", page_id.to_uppercase()))).await;
    assert_eq!((status, &page["page"]["pageKey"]), (StatusCode::OK, &json!(page_key)));

    // 종결된 제안을 다시 승인하면 전이 판정의 409다.
    let (status, again) = call(&state, json_request("POST", &approve, Some(&format!("approve:{id}")), "")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(again["error"].is_string());

    let delete = format!("/api/wiki/pages/{page_id}");
    let (status, archived) = call(&state, json_request("DELETE", &delete, Some(&format!("delete:{page_id}")), "")).await;
    assert_eq!((status, archived), (StatusCode::OK, json!({ "page": { "id": page_id, "lifecycleState": "archived" } })));
    let missing = call(&state, get(&delete)).await;
    assert_eq!(missing, (StatusCode::NOT_FOUND, json!({ "error": "위키 페이지를 찾을 수 없습니다." })));

    let second =
        json!({ "pageKey": format!("{page_key}-2"), "kind": "decision", "title": "거절할 제안", "sourceRecordIds": [source] });
    let (_, second) = call(&state, json_request("POST", "/api/wiki/proposals", None, &second.to_string())).await;
    let second_id = text(&second["proposal"]["id"]).to_string();
    let reject = format!("/api/wiki/proposals/{second_id}/reject");
    let (status, rejected) =
        call(&state, json_request("POST", &reject, Some(&format!("reject:{second_id}")), r#"{"reason":"중복"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((&rejected["proposal"]["state"], &rejected["proposal"]["reason"]), (&json!("rejected"), &json!("중복")));

    for sql in [
        "DELETE FROM wiki_proposals WHERE project_id = $1::uuid",
        "DELETE FROM work_record_links WHERE source_record_id IN (SELECT id FROM work_records WHERE project_id = $1::uuid)",
        "DELETE FROM work_record_events WHERE record_id IN (SELECT id FROM work_records WHERE project_id = $1::uuid)",
        "DELETE FROM rag_documents WHERE work_record_id IN (SELECT id FROM work_records WHERE project_id = $1::uuid)",
        "DELETE FROM work_records WHERE project_id = $1::uuid",
        "DELETE FROM projects WHERE id = $1::uuid",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&project).execute(&mut *setup).await.unwrap();
    }
    sqlx::query("DELETE FROM characters WHERE id = $1").bind(&author).execute(&mut *setup).await.unwrap();
}
