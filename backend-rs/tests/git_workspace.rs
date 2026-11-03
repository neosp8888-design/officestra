// 작업 공간 검토·승인·거절, CLI 전환 전 세션 종료, 재시작 복구 통합 시험. 임시 Git 저장소와 실제 DB로 Node와 같은 응답·상태·방송을 본다.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::launch::LaunchPaths;
use officestra_backend::agent_runtime::service::{RuntimeServices, SessionRoots};
use officestra_backend::git_workspace::{GitWorkspaceManager, TaskWorkspace, WorkspaceReview};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const UNAVAILABLE: &str = "CLI 실행기가 준비되지 않았습니다.";
const UNSUPPORTED: &str = "상태 변경 요청은 application/json 형식이어야 합니다.";
const NOT_FOUND: &str = "검토할 작업 공간을 찾을 수 없습니다.";
const ROUTE_NOT_FOUND: &str = "경로를 찾을 수 없습니다.";
const SWITCH_BLOCKED: &str = "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 통합하거나 폐기하세요.";
/// Node workspaceReviewPayload의 키 순서.
const PAYLOAD_KEYS: [&str; 19] = [
    "status",
    "repositoryRoot",
    "worktreePath",
    "executionWorkdir",
    "branchName",
    "baseBranch",
    "baseCommit",
    "reviewTurnId",
    "reviewTree",
    "headCommit",
    "changedFiles",
    "taskCommit",
    "mergedCommit",
    "errorMessage",
    "reviewRequestedAt",
    "mergedAt",
    "rejectedAt",
    "diff",
    "diffTruncated",
];

/// 원본 저장소와 worktree 보관 폴더. macOS의 /var는 /private/var 링크라 실제 경로를 쓴다.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("officestra-git-workspace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo")).unwrap();
        std::fs::create_dir_all(root.join("worktrees")).unwrap();
        let fixture = Self { root: std::fs::canonicalize(&root).unwrap() };
        let repo = fixture.repo();
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "기본\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "init"]);
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn manager(&self) -> GitWorkspaceManager {
        GitWorkspaceManager::new(self.repo().to_str().unwrap(), Some(self.root.join("worktrees").to_str().unwrap())).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(directory).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn branch_exists(fixture: &Fixture, branch: &str) -> bool {
    !git(&fixture.repo(), &["branch", "--list", branch]).is_empty()
}

fn state(pool: PgPool, runtime: Arc<Runtime>) -> AppState {
    AppState {
        pool,
        runtime,
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

async fn raw(state: &AppState, method: &str, path: &str, headers: &[(header::HeaderName, &str)], body: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(name.clone(), *value);
    }
    let response = router(state.clone()).oneshot(builder.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    match body {
        Some(body) => raw(state, method, path, &[(header::CONTENT_TYPE, "application/json")], &body.to_string()).await,
        None => raw(state, method, path, &[], "").await,
    }
}

fn error(message: &str) -> Value {
    json!({ "error": message })
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().unwrap().keys().map(String::as_str).collect()
}

async fn fetch(state: &AppState, turn_id: &str) -> (StatusCode, Value) {
    call(state, "GET", &format!("/api/workspace-reviews/{turn_id}"), None).await
}

async fn approve(state: &AppState, turn_id: &str, body: Value) -> (StatusCode, Value) {
    call(state, "POST", &format!("/api/workspace-reviews/{turn_id}/approve"), Some(body)).await
}

async fn reject(state: &AppState, turn_id: &str) -> (StatusCode, Value) {
    call(state, "POST", &format!("/api/workspace-reviews/{turn_id}/reject"), Some(json!({}))).await
}

/// 받은 방송 중 작업 공간·피드 이벤트만 꺼내고 비운다.
fn take_events(events: &Mutex<Vec<Value>>) -> Vec<Value> {
    let mut events = events.lock().unwrap();
    let taken = events
        .iter()
        .filter(|event| matches!(event["type"].as_str(), Some("workspace.changed" | "feed.changed")))
        .cloned()
        .collect();
    events.clear();
    taken
}

fn changed(turn_id: &str, character_id: &str, status: &str) -> Value {
    json!({ "type": "workspace.changed", "turnId": turn_id, "characterId": character_id, "status": status })
}

/// 시험용으로 만든 작업 공간 하나. 세션·턴·결과 기록이 함께 있다.
struct Seeded {
    workspace: TaskWorkspace,
    session_id: String,
    turn_id: String,
    record_id: String,
}

/// 세션을 새로 열고 worktree를 만든 뒤 지정 상태로 넣는다. with_turn이면 업무 턴과 결과 기록을 붙인다.
async fn seed(
    pool: &PgPool,
    manager: &GitWorkspaceManager,
    project_id: &str,
    character_id: &str,
    status: &str,
    with_turn: bool,
) -> Seeded {
    let conversation_id: String =
        sqlx::query_scalar("INSERT INTO conversations (title, workdir) VALUES ('시험', $1) RETURNING id::text")
            .bind(manager.source_workdir())
            .fetch_one(pool)
            .await
            .unwrap();
    let session_id: String =
        sqlx::query_scalar("INSERT INTO cli_sessions (conversation_id, character_id) VALUES ($1::uuid, $2) RETURNING id::text")
            .bind(&conversation_id)
            .bind(character_id)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO active_cli_sessions (character_id, cli_session_id) VALUES ($1, $2::uuid)
         ON CONFLICT (character_id) DO UPDATE SET cli_session_id = EXCLUDED.cli_session_id, updated_at = now()",
    )
    .bind(character_id)
    .bind(&session_id)
    .execute(pool)
    .await
    .unwrap();
    let workspace_id: String = sqlx::query_scalar("SELECT gen_random_uuid()::text").fetch_one(pool).await.unwrap();
    let mut workspace = manager.provision(&workspace_id, character_id).await.unwrap().expect("Git 저장소");
    workspace.id = workspace_id;
    workspace.cli_session_id = session_id.clone();
    workspace.status = status.to_string();
    sqlx::query(
        "INSERT INTO task_workspaces (id, cli_session_id, status, repository_root, source_workdir, worktree_path,
                                      execution_workdir, branch_name, base_branch, base_commit)
         VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(&workspace.id)
    .bind(&session_id)
    .bind(status)
    .bind(&workspace.repository_root)
    .bind(&workspace.source_workdir)
    .bind(&workspace.worktree_path)
    .bind(&workspace.execution_workdir)
    .bind(&workspace.branch_name)
    .bind(&workspace.base_branch)
    .bind(&workspace.base_commit)
    .execute(pool)
    .await
    .unwrap();
    let (mut turn_id, mut record_id) = (String::new(), String::new());
    if with_turn {
        turn_id = sqlx::query_scalar(
            "INSERT INTO turns (cli_session_id, prompt, status, task_workspace_id) VALUES ($1::uuid, '업무', 'completed', $2::uuid)
             RETURNING id::text",
        )
        .bind(&session_id)
        .bind(&workspace.id)
        .fetch_one(pool)
        .await
        .unwrap();
        record_id = sqlx::query_scalar(
            "INSERT INTO work_records (project_id, record_type, title, body, character_id, source_turn_id)
             VALUES ($1::uuid, 'result', '업무 결과', '본문', $2, $3::uuid) RETURNING id::text",
        )
        .bind(project_id)
        .bind(character_id)
        .bind(&turn_id)
        .fetch_one(pool)
        .await
        .unwrap();
    }
    Seeded { workspace, session_id, turn_id, record_id }
}

/// 완료 턴처럼 지금 변경으로 검토를 요청한다.
async fn request_review(pool: &PgPool, manager: &GitWorkspaceManager, seeded: &Seeded) -> WorkspaceReview {
    let review = manager.prepare_review(&seeded.workspace).await.unwrap();
    assert!(review.has_changes);
    sqlx::query(
        "UPDATE task_workspaces
         SET status = 'awaiting_approval', review_turn_id = $2::uuid, review_tree = $3, head_commit = $4,
             changed_files = $5::jsonb, review_requested_at = now(), updated_at = now()
         WHERE id = $1::uuid",
    )
    .bind(&seeded.workspace.id)
    .bind(&seeded.turn_id)
    .bind(&review.review_tree)
    .bind(&review.head_commit)
    .bind(Value::Array(review.changed_files.clone()).to_string())
    .execute(pool)
    .await
    .unwrap();
    review
}

/// 작업 공간 행의 상태·검토 턴·검토 tree·오류·변경 파일.
async fn workspace_row(pool: &PgPool, workspace_id: &str) -> (String, Option<String>, Option<String>, Option<String>, Value) {
    sqlx::query_as(
        "SELECT status, review_turn_id::text, review_tree, error_message, changed_files FROM task_workspaces WHERE id = $1::uuid",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// 결과 기록의 수명 상태·검토 메타데이터·마지막 사건 행위자.
async fn record_review(pool: &PgPool, record_id: &str) -> (String, Value, Option<(String, String)>) {
    let (lifecycle, review): (String, Option<Value>) =
        sqlx::query_as("SELECT lifecycle_state, metadata->'review' FROM work_records WHERE id = $1::uuid")
            .bind(record_id)
            .fetch_one(pool)
            .await
            .unwrap();
    let event: Option<(String, String)> = sqlx::query_as(
        "SELECT event_type, actor_type FROM work_record_events WHERE record_id = $1::uuid ORDER BY record_version DESC LIMIT 1",
    )
    .bind(record_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    (lifecycle, review.unwrap_or(Value::Null), event)
}

async fn session_open(pool: &PgPool, session_id: &str) -> (bool, bool) {
    sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM active_cli_sessions WHERE cli_session_id = $1::uuid),
                (SELECT ended_at IS NULL FROM cli_sessions WHERE id = $1::uuid)",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn 실행기가_없으면_작업_공간_검토는_메서드와_무관하게_503이다() {
    let pool = db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap();
    let state = state(pool, Arc::new(Runtime::new(None, None)));
    for (method, path) in [
        ("GET", "/api/workspace-reviews/turn-1"),
        ("POST", "/api/workspace-reviews/turn-1/approve"),
        ("POST", "/api/workspace-reviews/turn-1/reject"),
        ("DELETE", "/api/workspace-reviews/turn-1"),
        ("GET", "/api/workspace-reviews/turn-1/approve"),
    ] {
        let (status, body) = raw(&state, method, path, &[], "").await;
        assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, error(UNAVAILABLE)), "{method} {path}");
    }
}

#[tokio::test]
async fn 임시_저장소로_검토_승인_거절_세션_종료_복구를_확인한다() {
    let Ok(url) = std::env::var("OFFICE_TEST_DATABASE_URL") else {
        eprintln!("OFFICE_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Ok(migrations_dir) = std::env::var("OFFICE_MIGRATIONS_DIR") else {
        eprintln!("OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return;
    };
    let pool = db::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP SCHEMA public CASCADE; CREATE SCHEMA public;
         CREATE EXTENSION IF NOT EXISTS vector; CREATE EXTENSION IF NOT EXISTS pgcrypto;",
    )
    .execute(&pool)
    .await
    .unwrap();
    migrate::run(&pool, Path::new(&migrations_dir)).await.unwrap();

    let fixture = Fixture::new();
    let repo = fixture.repo();
    for (id, name) in [("dev", "개발"), ("dev2", "변경"), ("dev3", "빈공간"), ("dev4", "일괄"), ("dev5", "턴없음")]
        .into_iter()
        .chain([("dev6", "준비"), ("dev7", "통합"), ("dev8", "손상"), ("dev9", "병합중")])
    {
        sqlx::query("INSERT INTO characters (id, name, seat, backend, permission) VALUES ($1, $2, '상단', 'codex', 'workspace-write')")
            .bind(id)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    let project_id: String = sqlx::query_scalar("INSERT INTO projects (repository_root) VALUES ($1) RETURNING id::text")
        .bind(repo.to_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();

    let runtime = Arc::new(Runtime::new(Some(repo.clone()), Some(repo.clone())));
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&events);
    let mut services = RuntimeServices::new(
        pool.clone(),
        Arc::new(move |event| sink.lock().unwrap().push(event)),
        LaunchPaths {
            result_tool_directory: fixture.root.join("tools").to_str().unwrap().to_string(),
            node_executable: None,
            playwright_driver_directory: String::new(),
            runtime_id: "git-workspace-test".into(),
        },
        SessionRoots { home: fixture.root.join("home").to_str().unwrap().to_string() },
    );
    services.workspace_manager = Some(Arc::new(fixture.manager()));
    assert!(runtime.attach_services(services));
    let state = state(pool.clone(), Arc::clone(&runtime));
    let manager = fixture.manager();

    // 경로 검사. 출처·본문 형식은 승인·거절에만 보고, 메서드나 동작이 맞지 않으면 404다.
    let missing = "00000000-0000-4000-8000-000000000000";
    assert_eq!(fetch(&state, missing).await, (StatusCode::NOT_FOUND, error(NOT_FOUND)));
    let (status, body) = fetch(&state, "not-a-uuid").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body["error"].as_str().unwrap().contains("invalid input syntax for type uuid"), "{body}");
    for action in ["approve", "reject"] {
        let path = format!("/api/workspace-reviews/{missing}/{action}");
        assert_eq!(raw(&state, "POST", &path, &[], "{}").await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, error(UNSUPPORTED)));
        let evil = [(header::CONTENT_TYPE, "application/json"), (header::ORIGIN, "http://evil.example")];
        assert_eq!(
            raw(&state, "POST", &path, &evil, "{}").await,
            (StatusCode::FORBIDDEN, error("신뢰할 수 없는 요청 출처입니다."))
        );
        let (status, _) = raw(&state, "POST", &path, &[(header::CONTENT_TYPE, "application/json")], "{").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(call(&state, "GET", &path, None).await, (StatusCode::NOT_FOUND, error(ROUTE_NOT_FOUND)));
    }
    assert_eq!(
        approve(&state, missing, Value::Null).await,
        (StatusCode::INTERNAL_SERVER_ERROR, error("Cannot read properties of null (reading 'reviewTree')"))
    );
    assert_eq!(reject(&state, missing).await, (StatusCode::NOT_FOUND, error(NOT_FOUND)));
    let path = format!("/api/workspace-reviews/{missing}");
    assert_eq!(call(&state, "DELETE", &path, None).await, (StatusCode::NOT_FOUND, error(ROUTE_NOT_FOUND)));
    assert_eq!(call(&state, "POST", &path, Some(json!({}))).await, (StatusCode::NOT_FOUND, error(ROUTE_NOT_FOUND)));

    // 승인 흐름. 검토 diff를 읽고, 빈·옛 tree는 막고, 검토 뒤 바뀌면 다시 검토를 요구한 뒤 새 tree로 병합한다.
    let merged = seed(&pool, &manager, &project_id, "dev", "active", true).await;
    let worktree = PathBuf::from(&merged.workspace.worktree_path);
    std::fs::write(worktree.join("README.md"), "기본\n업무 변경\n").unwrap();
    std::fs::write(worktree.join("feature.txt"), "새 기능\n").unwrap();
    let review = request_review(&pool, &manager, &merged).await;
    take_events(&events);
    let (status, body) = fetch(&state, &merged.turn_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let payload = &body["workspace"];
    assert_eq!(keys(&body), ["workspace"]);
    assert_eq!(keys(payload), PAYLOAD_KEYS);
    assert_eq!(payload["status"], "awaiting_approval");
    assert_eq!(payload["repositoryRoot"], repo.to_str().unwrap());
    assert_eq!(payload["executionWorkdir"], merged.workspace.execution_workdir.as_str());
    assert_eq!(payload["reviewTurnId"], merged.turn_id.as_str());
    assert_eq!(payload["reviewTree"], review.review_tree.as_str());
    assert_eq!(
        payload["changedFiles"],
        json!([{ "status": "M", "path": "README.md" }, { "status": "A", "path": "feature.txt" }])
    );
    assert!(payload["diff"].as_str().unwrap().contains("+++ b/feature.txt"), "{payload}");
    assert_eq!(payload["diffTruncated"], false);
    assert!(payload["reviewRequestedAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!((&payload["mergedAt"], &payload["taskCommit"]), (&Value::Null, &Value::Null));
    assert_eq!(take_events(&events), Vec::<Value>::new(), "검토 tree가 같으면 갱신 방송이 없다");

    let unknown_version = error("검토한 변경 버전을 확인할 수 없습니다. diff를 다시 불러오세요.");
    assert_eq!(approve(&state, &merged.turn_id, json!({})).await, (StatusCode::CONFLICT, unknown_version.clone()));
    assert_eq!(approve(&state, &merged.turn_id, json!({ "reviewTree": "  " })).await, (StatusCode::CONFLICT, unknown_version));
    assert_eq!(
        approve(&state, &merged.turn_id, json!({ "reviewTree": "stale-review-tree" })).await,
        (StatusCode::CONFLICT, error("검토 뒤 변경사항이 갱신됐습니다. diff를 다시 확인하세요."))
    );
    let (status, _, _, error_message, _) = workspace_row(&pool, &merged.workspace.id).await;
    assert_eq!((status.as_str(), error_message), ("awaiting_approval", None), "옛 tree는 병합도 오류 기록도 하지 않는다");
    assert_eq!(take_events(&events), Vec::<Value>::new());

    std::fs::write(worktree.join("later.txt"), "검토 뒤 추가\n").unwrap();
    let changed_after_review = "검토 후 업무 변경사항이 달라졌습니다. 변경사항을 다시 확인하세요.";
    assert_eq!(
        approve(&state, &merged.turn_id, json!({ "reviewTree": review.review_tree })).await,
        (StatusCode::CONFLICT, error(changed_after_review))
    );
    let (status, review_turn, review_tree, error_message, changed_files) = workspace_row(&pool, &merged.workspace.id).await;
    let three_files = json!([
        { "status": "M", "path": "README.md" },
        { "status": "A", "path": "feature.txt" },
        { "status": "A", "path": "later.txt" },
    ]);
    assert_eq!((status.as_str(), review_turn.as_deref()), ("awaiting_approval", Some(merged.turn_id.as_str())));
    assert_eq!((error_message.as_deref(), &changed_files), (Some(changed_after_review), &three_files));
    let refreshed_tree = review_tree.unwrap();
    assert_ne!(refreshed_tree, review.review_tree);
    let (lifecycle, record, event) = record_review(&pool, &merged.record_id).await;
    assert_eq!(lifecycle, "active");
    assert_eq!(
        (&record["status"], &record["reviewTree"], &record["errorMessage"], &record["changedFiles"]),
        (&json!("awaiting_approval"), &json!(refreshed_tree), &json!(changed_after_review), &three_files)
    );
    assert_eq!(event, Some(("state_changed".to_string(), "system".to_string())));
    assert_eq!(take_events(&events), [changed(&merged.turn_id, "dev", "awaiting_approval")]);
    assert!(runtime.maintenance_status().idle, "실패한 승인도 후처리 수를 돌려놓는다");

    let (status, body) = approve(&state, &merged.turn_id, json!({ "reviewTree": refreshed_tree })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(keys(&body), ["workspace", "cleanupWarning"]);
    let payload = &body["workspace"];
    assert_eq!(keys(payload), PAYLOAD_KEYS);
    assert_eq!((&payload["status"], &body["cleanupWarning"]), (&json!("merged"), &Value::Null));
    assert_eq!((&payload["diff"], &payload["diffTruncated"]), (&json!(""), &json!(false)));
    assert_eq!(payload["executionWorkdir"], repo.to_str().unwrap(), "통합된 공간은 원본 폴더를 보인다");
    let merged_commit = payload["mergedCommit"].as_str().unwrap().to_string();
    let task_commit = payload["taskCommit"].as_str().unwrap().to_string();
    assert!(payload["mergedAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), merged_commit);
    let subject = format!("OFFICESTRA: 개발 업무 {}", &merged.turn_id[..8]);
    assert_eq!(git(&repo, &["log", "-1", "--format=%s", &task_commit]), subject);
    assert_eq!(std::fs::read_to_string(repo.join("later.txt")).unwrap(), "검토 뒤 추가\n");
    assert!(!worktree.exists() && !branch_exists(&fixture, &merged.workspace.branch_name), "승인 뒤 worktree와 브랜치를 정리한다");
    let (status, _, _, error_message, _) = workspace_row(&pool, &merged.workspace.id).await;
    assert_eq!((status.as_str(), error_message), ("merged", None));
    let (lifecycle, record, event) = record_review(&pool, &merged.record_id).await;
    assert_eq!(lifecycle, "active");
    assert_eq!(
        (&record["status"], &record["mergedCommit"], &record["taskCommit"], &record["errorMessage"]),
        (&json!("merged"), &json!(merged_commit), &json!(task_commit), &Value::Null)
    );
    assert_eq!(event, Some(("state_changed".to_string(), "user".to_string())));
    assert_eq!(take_events(&events), [changed(&merged.turn_id, "dev", "merged")]);
    assert_eq!(session_open(&pool, &merged.session_id).await, (true, true), "승인은 활성 CLI 세션을 유지한다");
    assert!(runtime.maintenance_status().idle);
    assert_eq!(
        approve(&state, &merged.turn_id, json!({ "reviewTree": refreshed_tree })).await,
        (StatusCode::CONFLICT, error("현재 상태에서는 이 변경사항을 승인할 수 없습니다."))
    );
    assert_eq!(
        reject(&state, &merged.turn_id).await,
        (StatusCode::CONFLICT, error("현재 상태에서는 이 변경사항을 거절할 수 없습니다."))
    );
    let (status, body) = fetch(&state, &merged.turn_id).await;
    assert_eq!((status, &body["workspace"]["status"]), (StatusCode::OK, &json!("merged")));
    assert!(body["workspace"]["diff"].as_str().unwrap().contains("+++ b/later.txt"), "통합 뒤에도 검토 tree로 diff를 읽는다");

    // 검토 갱신. 승인 대기 중 변경이 늘면 새 tree로 맞추고, 기준과 같아지면 활성 공간으로 되돌린다.
    let refreshed = seed(&pool, &manager, &project_id, "dev", "active", true).await;
    let refreshed_worktree = PathBuf::from(&refreshed.workspace.worktree_path);
    std::fs::write(refreshed_worktree.join("a.txt"), "가\n").unwrap();
    let first = request_review(&pool, &manager, &refreshed).await;
    std::fs::write(refreshed_worktree.join("b.txt"), "나\n").unwrap();
    take_events(&events);
    let (status, body) = fetch(&state, &refreshed.turn_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let payload = &body["workspace"];
    assert_eq!(payload["status"], "awaiting_approval");
    assert_ne!(payload["reviewTree"], first.review_tree.as_str());
    assert_eq!(payload["changedFiles"], json!([{ "status": "A", "path": "a.txt" }, { "status": "A", "path": "b.txt" }]));
    assert!(payload["diff"].as_str().unwrap().contains("+++ b/b.txt"), "{payload}");
    let (_, record, event) = record_review(&pool, &refreshed.record_id).await;
    assert_eq!((&record["status"], &record["reviewTree"]), (&json!("awaiting_approval"), &payload["reviewTree"]));
    assert_eq!(event, Some(("state_changed".to_string(), "system".to_string())));
    assert_eq!(take_events(&events), [changed(&refreshed.turn_id, "dev", "awaiting_approval")]);
    std::fs::remove_file(refreshed_worktree.join("a.txt")).unwrap();
    std::fs::remove_file(refreshed_worktree.join("b.txt")).unwrap();
    let (status, body) = fetch(&state, &refreshed.turn_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let payload = &body["workspace"];
    assert_eq!(
        (&payload["status"], &payload["reviewTurnId"], &payload["reviewTree"], &payload["changedFiles"]),
        (&json!("active"), &Value::Null, &Value::Null, &json!([]))
    );
    assert_eq!((&payload["reviewRequestedAt"], &payload["diff"]), (&Value::Null, &json!("")));
    let (_, record, _) = record_review(&pool, &refreshed.record_id).await;
    assert_eq!((&record["status"], &record["changedFiles"]), (&json!("not_required"), &json!([])));
    assert_eq!(take_events(&events), [changed(&refreshed.turn_id, "dev", "active")]);
    assert_eq!(fetch(&state, &refreshed.turn_id).await, (StatusCode::NOT_FOUND, error(NOT_FOUND)));

    // 충돌·드레인·거절. 원본과 충돌하면 충돌 상태로 두고 같은 tree로 다시 시도할 수 있다.
    // 드레인 중에는 사용자 승인·거절을 받지 않으며, 거절은 세션과 worktree를 남긴다.
    let conflicted = seed(&pool, &manager, &project_id, "dev", "active", true).await;
    let conflicted_worktree = PathBuf::from(&conflicted.workspace.worktree_path);
    std::fs::write(conflicted_worktree.join("README.md"), "업무 공간 내용\n").unwrap();
    let conflict_review = request_review(&pool, &manager, &conflicted).await;
    std::fs::write(repo.join("README.md"), "원본 내용\n").unwrap();
    git(&repo, &["commit", "-am", "원본 변경"]);
    let source_head = git(&repo, &["rev-parse", "HEAD"]);
    take_events(&events);
    let conflict_message = "원본 브랜치의 최신 변경과 업무 변경이 충돌합니다.";
    for _ in 0..2 {
        assert_eq!(
            approve(&state, &conflicted.turn_id, json!({ "reviewTree": conflict_review.review_tree })).await,
            (StatusCode::CONFLICT, error(conflict_message))
        );
        let (status, _, _, error_message, _) = workspace_row(&pool, &conflicted.workspace.id).await;
        assert_eq!((status.as_str(), error_message.as_deref()), ("conflict", Some(conflict_message)));
        assert_eq!(take_events(&events), [changed(&conflicted.turn_id, "dev", "conflict")]);
    }
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), source_head, "충돌한 병합은 원본을 바꾸지 않는다");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let (_, record, _) = record_review(&pool, &conflicted.record_id).await;
    assert_eq!((&record["status"], &record["errorMessage"]), (&json!("conflict"), &json!(conflict_message)));

    let (status, body) = call(&state, "POST", "/api/maintenance/drain", Some(json!({}))).await;
    assert_eq!((status, &body["draining"]), (StatusCode::OK, &json!(true)));
    assert_eq!(
        approve(&state, &conflicted.turn_id, json!({ "reviewTree": conflict_review.review_tree })).await,
        (StatusCode::SERVICE_UNAVAILABLE, error("백엔드가 안전한 전환을 준비 중이라 새 통합을 받지 않습니다."))
    );
    assert_eq!(
        reject(&state, &conflicted.turn_id).await,
        (StatusCode::SERVICE_UNAVAILABLE, error("백엔드가 안전한 전환을 준비 중이라 새 거절을 받지 않습니다."))
    );
    let (status, body) = call(&state, "DELETE", "/api/maintenance/drain", Some(json!({}))).await;
    assert_eq!((status, &body["draining"]), (StatusCode::OK, &json!(false)));

    let (status, body) = reject(&state, &conflicted.turn_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(keys(&body), ["workspace"]);
    let payload = &body["workspace"];
    assert_eq!(keys(payload), PAYLOAD_KEYS);
    assert_eq!(
        (&payload["status"], &payload["errorMessage"], &payload["diff"]),
        (&json!("rejected"), &Value::Null, &json!(""))
    );
    assert_eq!(payload["executionWorkdir"], conflicted.workspace.execution_workdir.as_str());
    assert!(payload["rejectedAt"].as_str().unwrap().ends_with('Z'));
    let (status, _, _, error_message, _) = workspace_row(&pool, &conflicted.workspace.id).await;
    assert_eq!((status.as_str(), error_message), ("rejected", None));
    let (lifecycle, record, event) = record_review(&pool, &conflicted.record_id).await;
    assert_eq!((lifecycle.as_str(), &record["status"]), ("archived", &json!("rejected")));
    assert_eq!(event, Some(("state_changed".to_string(), "user".to_string())));
    assert_eq!(take_events(&events), [changed(&conflicted.turn_id, "dev", "rejected")]);
    assert!(conflicted_worktree.exists() && branch_exists(&fixture, &conflicted.workspace.branch_name), "거절은 worktree를 보존한다");
    assert_eq!(session_open(&pool, &conflicted.session_id).await, (true, true), "거절은 활성 CLI 세션을 유지한다");
    assert!(runtime.maintenance_status().idle);

    // CLI 전환 전 세션 종료. 변경이 있으면 승인 대기로 돌리고 막으며, 변경이 없으면 worktree를 지우고 공간을 닫는다.
    let claude = |character_id: &str| {
        json!({ "characterId": character_id, "backend": "claude", "model": "claude-opus-5", "effort": "high", "fastMode": true, "permission": "acceptEdits" })
    };
    let settings = async |character_id: &str| {
        call(&state, "PUT", &format!("/api/characters/{character_id}/settings"), Some(claude(character_id))).await
    };
    let bulk = |character_id: &'static str| call(&state, "PUT", "/api/characters/settings/bulk", Some(json!({ "updates": [claude(character_id)] })));
    let backend = async |character_id: &str| -> String {
        sqlx::query_scalar("SELECT backend FROM characters WHERE id = $1").bind(character_id).fetch_one(&pool).await.unwrap()
    };

    let pending = seed(&pool, &manager, &project_id, "dev2", "active", true).await;
    std::fs::write(PathBuf::from(&pending.workspace.worktree_path).join("draft.txt"), "초안\n").unwrap();
    take_events(&events);
    assert_eq!(settings("dev2").await, (StatusCode::CONFLICT, error(SWITCH_BLOCKED)));
    let (status, review_turn, review_tree, _, changed_files) = workspace_row(&pool, &pending.workspace.id).await;
    assert_eq!((status.as_str(), review_turn.as_deref()), ("awaiting_approval", Some(pending.turn_id.as_str())));
    assert!(review_tree.is_some());
    assert_eq!(changed_files, json!([{ "status": "A", "path": "draft.txt" }]));
    let (_, record, _) = record_review(&pool, &pending.record_id).await;
    assert_eq!((&record["status"], &record["changedFiles"]), (&json!("awaiting_approval"), &changed_files));
    assert_eq!(
        take_events(&events),
        [
            changed(&pending.turn_id, "dev2", "awaiting_approval"),
            json!({ "type": "feed.changed", "turnId": pending.turn_id, "characterId": "dev2" }),
        ]
    );
    assert_eq!(session_open(&pool, &pending.session_id).await, (true, true));
    assert_eq!(backend("dev2").await, "codex");
    assert_eq!(bulk("dev2").await, (StatusCode::CONFLICT, error("이 직원의 대기 중 변경사항을 먼저 통합하거나 폐기하세요.")));
    assert_eq!(settings("dev2").await, (StatusCode::CONFLICT, error("이 직원의 대기 중 변경사항을 먼저 통합하거나 폐기하세요.")));

    for (character_id, single) in [("dev3", true), ("dev4", false)] {
        let empty = seed(&pool, &manager, &project_id, character_id, "active", true).await;
        take_events(&events);
        let (status, body) = if single { settings(character_id).await } else { bulk(character_id).await };
        assert_eq!(status, StatusCode::OK, "{character_id} {body}");
        if !single {
            assert_eq!((&body["ok"], &body["warnings"]), (&json!(true), &json!([])));
        }
        assert_eq!(backend(character_id).await, "claude");
        let (status, _, review_tree, _, changed_files) = workspace_row(&pool, &empty.workspace.id).await;
        assert_eq!((status.as_str(), review_tree, changed_files), ("closed", None, json!([])), "{character_id}");
        assert!(
            !Path::new(&empty.workspace.worktree_path).exists() && !branch_exists(&fixture, &empty.workspace.branch_name),
            "{character_id} 빈 작업 공간을 정리한다"
        );
        assert_eq!(session_open(&pool, &empty.session_id).await, (false, false), "{character_id}");
        let (_, record, _) = record_review(&pool, &empty.record_id).await;
        assert_eq!((&record["status"], &record["changedFiles"]), (&json!("not_required"), &json!([])), "{character_id}");
        assert_eq!(take_events(&events), [changed(&empty.turn_id, character_id, "closed")], "{character_id}");
    }

    let no_turn = seed(&pool, &manager, &project_id, "dev5", "active", false).await;
    assert_eq!(settings("dev5").await, (StatusCode::CONFLICT, error("작업 공간에 연결된 업무 기록을 찾을 수 없습니다.")));
    assert_eq!(workspace_row(&pool, &no_turn.workspace.id).await.0, "active");
    assert_eq!(backend("dev5").await, "codex");

    // 재시작 복구. 준비 중이던 공간은 지우고 실패로, 통합 중이던 공간은 실패로, 통합된 공간은 정리하고,
    // 손상된 활성 공간은 실패로 둔다. 온전한 활성 공간과 다른 상태는 그대로다.
    let provisioning = seed(&pool, &manager, &project_id, "dev6", "provisioning", false).await;
    let leftover = seed(&pool, &manager, &project_id, "dev7", "merged", false).await;
    let broken = seed(&pool, &manager, &project_id, "dev8", "active", false).await;
    std::fs::remove_dir_all(&broken.workspace.worktree_path).unwrap();
    let merging = seed(&pool, &manager, &project_id, "dev9", "merging", false).await;
    assert_eq!(runtime.recover_interrupted_jobs().await.unwrap(), 0);
    let status_and_error = async |seeded: &Seeded| {
        let (status, _, _, error_message, _) = workspace_row(&pool, &seeded.workspace.id).await;
        (status, error_message)
    };
    assert_eq!(
        status_and_error(&provisioning).await,
        ("failed".to_string(), Some("백엔드 재시작 전에 작업 공간 준비가 끝나지 않았습니다.".to_string()))
    );
    assert!(!Path::new(&provisioning.workspace.worktree_path).exists());
    assert!(!branch_exists(&fixture, &provisioning.workspace.branch_name));
    assert_eq!(status_and_error(&leftover).await, ("merged".to_string(), None));
    assert!(!Path::new(&leftover.workspace.worktree_path).exists());
    assert!(!branch_exists(&fixture, &leftover.workspace.branch_name));
    assert_eq!(
        status_and_error(&merging).await,
        ("failed".to_string(), Some("백엔드 재시작 전에 통합이 시작됐습니다. 원본 Git 상태를 확인하세요.".to_string()))
    );
    assert!(Path::new(&merging.workspace.worktree_path).exists(), "통합 중이던 공간은 지우지 않는다");
    let (status, error_message) = status_and_error(&broken).await;
    assert_eq!(status, "failed");
    assert!(error_message.unwrap().starts_with("활성 작업 공간을 복구할 수 없습니다. "));
    assert_eq!(status_and_error(&merged).await, ("merged".to_string(), None), "이미 정리된 통합 공간은 경고 없이 넘어간다");
    assert_eq!(status_and_error(&refreshed).await, ("active".to_string(), None));
    assert_eq!(status_and_error(&no_turn).await, ("active".to_string(), None));
    assert_eq!(status_and_error(&conflicted).await, ("rejected".to_string(), None));
    assert_eq!(status_and_error(&pending).await.0, "awaiting_approval");
}
