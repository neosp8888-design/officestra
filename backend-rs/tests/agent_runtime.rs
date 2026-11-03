// 직원 CLI 잡 생명주기 통합 시험. 모의 codex·claude 셸 스크립트로 접수·완료·실패·중단·복구·도구 이벤트·세션 종료를 본다.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::launch::LaunchPaths;
use officestra_backend::agent_runtime::service::{RuntimeServices, SessionRoots};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const BUSY_HINT: &str = "수신 직원의 실행 상태를 확인하세요. work-records의 active는 원인이 아닙니다. DELETE /api/agent-jobs는 기록 정리가 아니라 업무 중단이므로 접수 오류 해결에 사용하지 마세요.";
const NOTE: &str =
    "현재 시점의 접수 가능 여부입니다. 실제 접수는 POST 응답으로 확인하세요. 기록·세션의 active는 실행 상태가 아닙니다.";

/// 마지막 인자(프롬프트)의 표식으로 동작을 고른다. 받은 앞 인자 넷을 남긴다.
const FAKE_CODEX: &str = r#"#!/bin/sh
dir='__DIR__'
echo "$1 $2 $3 $4" >> "$dir/codex-args.log"
for prompt; do :; done
case "$prompt" in
  *MOCK_FAIL*)
    echo '모의 CLI가 실패했습니다.' >&2
    exit 3 ;;
  *MOCK_WAIT*)
    echo $$ > "$dir/wait.pid"
    echo '{"type":"thread.started","thread_id":"thread-wait"}'
    while [ ! -f "$dir/release" ] && [ -d "$dir" ]; do sleep 0.05; done ;;
  *)
    echo '{"type":"thread.started","thread_id":"thread-ok"}' ;;
esac
echo '{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"모의 답변입니다."}}'
echo '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}'
"#;

/// stream-json 입력 한 줄마다 init과 result를 낸다. 중단 제어 요청은 무시한다.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir='__DIR__'
echo $$ > "$dir/claude.pid"
while IFS= read -r line; do
  case "$line" in
    *'"type":"control_request"'*) ;;
    *)
      echo '{"type":"system","subtype":"init","session_id":"claude-session","model":"claude-opus-5"}'
      echo '{"type":"result","subtype":"success","is_error":false,"result":"클로드 답변입니다.","session_id":"claude-session","total_cost_usd":0.1,"usage":{"input_tokens":20,"output_tokens":7}}' ;;
  esac
done
"#;

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("officestra-agent-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["bin", "work", "home"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        // macOS의 /var는 /private/var 링크라 실행기의 저장소 비교와 맞추려고 실제 경로를 쓴다.
        let root = std::fs::canonicalize(&root).unwrap();
        let bin = root.join("bin");
        let text = root.to_str().unwrap();
        write_script(&bin.join("codex"), &FAKE_CODEX.replace("__DIR__", text));
        write_script(&bin.join("claude"), &FAKE_CLAUDE.replace("__DIR__", text));
        Self { root, bin }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn config(&self, executable: &str) -> Value {
        json!({ "executablePath": self.bin.join(executable).to_str().unwrap() })
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::write(self.path("release"), "");
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

async fn call(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    let body = match body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router(state.clone()).oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn start(state: &AppState, body: Value) -> (StatusCode, Value) {
    call(state, "POST", "/api/agent-jobs", Some(body)).await
}

/// DB 상태가 끝났고 실행 표에서도 빠질 때까지 기다린다.
async fn wait_for_turn(state: &AppState, turn_id: &str, character_id: &str) -> (String, Option<String>) {
    for _ in 0..200 {
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, error_message FROM turns WHERE id = $1::uuid")
                .bind(turn_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        let running = state.runtime.running_character_ids().iter().any(|id| id == character_id);
        if !matches!(row.0.as_str(), "pending" | "running") && !running {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{turn_id} 턴이 10초 안에 끝나지 않았습니다.");
}

async fn wait_for_external_id(pool: &PgPool, turn_id: &str, expected: &str) {
    for _ in 0..200 {
        let external: Option<String> = sqlx::query_scalar(
            "SELECT session.external_id FROM turns turn JOIN cli_sessions session ON session.id = turn.cli_session_id
             WHERE turn.id = $1::uuid",
        )
        .bind(turn_id)
        .fetch_one(pool)
        .await
        .unwrap();
        if external.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{turn_id} 턴의 외부 세션 ID가 {expected}로 저장되지 않았습니다.");
}

async fn messages(pool: &PgPool, turn_id: &str) -> Vec<(String, String)> {
    sqlx::query_as("SELECT role, text FROM messages WHERE turn_id = $1::uuid ORDER BY role")
        .bind(turn_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn usage(pool: &PgPool, turn_id: &str) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<f64>)> {
    sqlx::query_as(
        "SELECT input_tokens, output_tokens, cached_input_tokens, cost_usd::float8 FROM usage_records WHERE turn_id = $1::uuid",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: 신호 0은 프로세스 존재만 확인하고 아무것도 보내지 않는다.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_for_exit(pid: i32) -> bool {
    for _ in 0..100 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn availability(state: &AppState, character_id: &str) -> Value {
    let (status, body) = call(state, "GET", &format!("/api/agent-jobs?characterId={character_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["note"], NOTE);
    body["employees"][0].clone()
}

#[tokio::test]
async fn 실행기가_없으면_잡_경로는_node처럼_답한다() {
    let pool = db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap();
    let state = state(pool, Arc::new(Runtime::new(None, None)));
    let unavailable = json!({ "error": "CLI 실행기가 준비되지 않았습니다." });

    let (status, body) = call(&state, "GET", "/api/agent-jobs", None).await;
    assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, unavailable.clone()));
    // Node readJSON처럼 본문 해석이 실행기 확인보다 먼저이고, 해석 오류는 바깥 catch의 500이다.
    let request = Request::builder().method("POST").uri("/api/agent-jobs").body(Body::from("{")).unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body) = start(&state, json!({ "characterId": "boss", "prompt": "x" })).await;
    assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, unavailable.clone()));
    let (status, body) = call(&state, "DELETE", "/api/agent-jobs/boss", None).await;
    assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, unavailable));
    let event = json!({
        "characterId": "boss", "sessionId": "s", "mode": "gui",
        "activity": { "eventKey": "developer-tool:x", "status": "running", "text": "도구" },
    });
    let (status, body) = call(&state, "POST", "/api/developer-tool-events", Some(event)).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "accepted": false, "reason": "inactive_session" })));
}

#[tokio::test]
async fn 모의_cli로_잡을_접수하고_끝낸다() {
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
    for (id, name, backend, permission, config) in [
        ("codex-a", "코덱스", "codex", "workspace-write", fixture.config("codex")),
        ("codex-wait", "대기", "codex", "workspace-write", fixture.config("codex")),
        ("codex-fail", "실패", "codex", "workspace-write", fixture.config("codex")),
        ("claude-a", "클로드", "claude", "acceptEdits", fixture.config("claude")),
        ("sender", "발신", "codex", "workspace-write", json!({})),
    ] {
        sqlx::query("INSERT INTO characters (id, name, seat, backend, permission, config) VALUES ($1, $2, '상단', $3, $4, $5)")
            .bind(id)
            .bind(name)
            .bind(backend)
            .bind(permission)
            .bind(config)
            .execute(&pool)
            .await
            .unwrap();
    }

    let work = fixture.path("work");
    let runtime = Arc::new(Runtime::new(Some(work.clone()), Some(work.clone())));
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&events);
    let mut services = RuntimeServices::new(
        pool.clone(),
        Arc::new(move |event| sink.lock().unwrap().push(event)),
        LaunchPaths {
            result_tool_directory: fixture.path("tools").to_str().unwrap().to_string(),
            node_executable: None,
            playwright_driver_directory: String::new(),
            runtime_id: "agent-runtime-test".into(),
        },
        SessionRoots { home: fixture.path("home").to_str().unwrap().to_string() },
    );
    services.claude_suggestion_grace = Duration::from_millis(50);
    services.claude_interrupt_result_timeout = Duration::from_millis(500);
    assert!(runtime.attach_services(services));
    let state = state(pool.clone(), Arc::clone(&runtime));

    // 접수 가능 여부.
    assert_eq!(
        availability(&state, "codex-a").await,
        json!({
            "characterId": "codex-a", "name": "코덱스", "sessionOpen": false,
            "turnId": null, "turnStatus": null, "state": "ready", "canReceive": true,
        })
    );
    let (status, body) = call(&state, "GET", "/api/agent-jobs?characterId=nobody", None).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "수신 직원을 찾을 수 없습니다." })));

    // 접수 전 검사.
    let (status, body) = start(&state, json!({ "characterId": "nobody", "prompt": "MOCK_OK" })).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "캐릭터를 찾을 수 없습니다." })));
    let (status, body) = start(&state, json!({ "characterId": "codex-a", "prompt": "  " })).await;
    assert_eq!((status, body), (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "업무 내용을 입력하세요." })));
    let (status, body) = start(&state, json!({ "characterId": "codex-a", "prompt": "MOCK_OK", "senderCharacterId": "nobody" })).await;
    assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({ "error": "발신 직원을 찾을 수 없습니다." })));

    // 새 세션으로 완료한다.
    let (status, job) =
        start(&state, json!({ "characterId": "codex-a", "prompt": "MOCK_OK 첫 업무", "senderCharacterId": "sender" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job.as_object().unwrap().keys().collect::<Vec<_>>(), ["turnId", "conversationId", "status"]);
    assert_eq!(job["status"], "running");
    let first = job["turnId"].as_str().unwrap().to_string();
    assert_eq!(wait_for_turn(&state, &first, "codex-a").await, ("completed".to_string(), None));
    assert_eq!(
        messages(&pool, &first).await,
        [("assistant".to_string(), "모의 답변입니다.".to_string()), ("user".to_string(), "MOCK_OK 첫 업무".to_string())]
    );
    let (sender, backend): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT sender_character_id, backend FROM turns WHERE id = $1::uuid")
            .bind(&first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((sender.as_deref(), backend.as_deref()), (Some("sender"), Some("codex")));
    let (input, output, cached, _) = usage(&pool, &first).await.expect("사용량 기록");
    assert_eq!((input, output, cached), (Some(10), Some(5), Some(0)));
    let (status, sessions) = call(&state, "GET", "/api/active-sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let codex_session = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["characterId"] == "codex-a")
        .cloned()
        .expect("codex-a 활성 세션");
    assert_eq!(codex_session["externalSessionId"], "thread-ok");
    assert_eq!(codex_session["conversationId"], job["conversationId"]);
    assert!(
        events.lock().unwrap().iter().any(|event| event["type"] == "feed.changed" && event["turnId"] == first.as_str()),
        "feed.changed 방송"
    );

    // 같은 직원의 다음 업무는 저장된 세션을 이어 간다.
    let (status, second_job) = start(&state, json!({ "characterId": "codex-a", "prompt": "MOCK_OK 둘째 업무" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{second_job}");
    assert_eq!(second_job["conversationId"], job["conversationId"]);
    let second = second_job["turnId"].as_str().unwrap().to_string();
    assert_eq!(wait_for_turn(&state, &second, "codex-a").await, ("completed".to_string(), None));
    assert_eq!(fixture.read("codex-args.log"), "exec --json --skip-git-repo-check -c\nexec resume thread-ok --json\n");

    // 답 없이 실패하면 표준 오류가 실패 사유이고, 외부 세션이 없으니 CLI 세션을 닫는다.
    let (status, job) = start(&state, json!({ "characterId": "codex-fail", "prompt": "MOCK_FAIL" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let failed = job["turnId"].as_str().unwrap().to_string();
    assert_eq!(
        wait_for_turn(&state, &failed, "codex-fail").await,
        ("failed".to_string(), Some("모의 CLI가 실패했습니다.".to_string()))
    );
    let ended: bool = sqlx::query_scalar(
        "SELECT session.ended_at IS NOT NULL FROM turns turn JOIN cli_sessions session ON session.id = turn.cli_session_id
         WHERE turn.id = $1::uuid",
    )
    .bind(&failed)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ended);
    assert_eq!(availability(&state, "codex-fail").await["state"], "ready");

    // 실행 중: 상태·중복 접수·도구 이벤트·드레인·CLI 변경 차단·중단.
    let (status, job) = start(&state, json!({ "characterId": "codex-wait", "prompt": "MOCK_WAIT" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let waiting = job["turnId"].as_str().unwrap().to_string();
    wait_for_external_id(&pool, &waiting, "thread-wait").await;
    let running = availability(&state, "codex-wait").await;
    assert_eq!(
        (&running["state"], &running["canReceive"], &running["turnStatus"], &running["turnId"]),
        (&json!("running"), &json!(false), &json!("running"), &json!(waiting))
    );
    let (status, body) = start(&state, json!({ "characterId": "codex-wait", "prompt": "MOCK_OK" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({
            "error": "이 직원의 현재 업무가 끝난 뒤 새 업무를 시작하세요.",
            "characterId": "codex-wait",
            "statusURL": "/api/agent-jobs?characterId=codex-wait",
            "hint": BUSY_HINT,
        })
    );

    let event = |session: &str, mode: &str| {
        json!({
            "characterId": "codex-wait", "sessionId": session, "mode": mode,
            "activity": { "eventKey": "developer-tool:browser:1", "status": "running", "text": "브라우저 여는 중" },
        })
    };
    let (status, body) = call(&state, "POST", "/api/developer-tool-events", Some(event("thread-wait", "gui"))).await;
    assert_eq!((status, body), (StatusCode::ACCEPTED, json!({ "accepted": true })));
    let (status, body) = call(&state, "POST", "/api/developer-tool-events", Some(event("other", "gui"))).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "accepted": false, "reason": "inactive_session" })));
    let (status, body) = call(&state, "POST", "/api/developer-tool-events", Some(event("thread-wait", "terminal"))).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "accepted": false, "reason": "inactive_session" })));
    let (status, body) = call(&state, "POST", "/api/developer-tool-events", Some(event("thread-wait", "x"))).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "accepted": false, "reason": "invalid_event" })));
    let tool: (String, String, String) =
        sqlx::query_as("SELECT kind, event_key, text FROM turn_activities WHERE turn_id = $1::uuid AND kind = 'tool'")
            .bind(&waiting)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tool, ("tool".to_string(), "developer-tool:browser:1".to_string(), "브라우저 여는 중".to_string()));

    let (status, body) = call(&state, "POST", "/api/maintenance/drain", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&body["draining"], &body["acceptingJobs"], &body["activeTurnCount"], &body["idle"]),
        (&json!(true), &json!(false), &json!(1), &json!(false))
    );
    let (status, body) = start(&state, json!({ "characterId": "codex-a", "prompt": "MOCK_OK" })).await;
    assert_eq!(
        (status, body),
        (StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "백엔드가 안전한 전환을 준비 중이라 새 업무를 받지 않습니다." }))
    );
    assert_eq!(availability(&state, "codex-a").await["state"], "draining");
    let (status, body) = call(&state, "DELETE", "/api/maintenance/drain", Some(json!({}))).await;
    assert_eq!((status, &body["draining"]), (StatusCode::OK, &json!(false)));

    let claude_settings = json!({ "backend": "claude", "model": "claude-opus-5", "effort": "high", "fastMode": true, "permission": "acceptEdits" });
    let (status, body) = call(&state, "PUT", "/api/characters/codex-wait/settings", Some(claude_settings)).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "error": "이 직원의 실행 중인 업무가 끝난 뒤 CLI를 변경하세요." })));

    let pid: i32 = fixture.read("wait.pid").trim().parse().unwrap();
    assert!(process_alive(pid));
    let (status, body) = call(&state, "DELETE", "/api/agent-jobs/codex-wait", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "turnId": waiting, "status": "interrupted" })));
    assert!(wait_for_exit(pid).await, "중단한 모의 CLI가 끝나지 않았습니다.");
    let (status, body) = call(&state, "DELETE", "/api/agent-jobs/codex-wait", None).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "실행 중인 업무를 찾을 수 없습니다." })));
    // 끊긴 프로세스의 뒤처리가 중단 기록을 덮지 않는다.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        wait_for_turn(&state, &waiting, "codex-wait").await,
        ("interrupted".to_string(), Some("사용자가 업무를 중단했습니다.".to_string()))
    );
    let tool_status: String = sqlx::query_scalar("SELECT status FROM turn_activities WHERE turn_id = $1::uuid AND kind = 'tool'")
        .bind(&waiting)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tool_status, "failed");
    assert_eq!(availability(&state, "codex-wait").await["state"], "ready");

    // Claude는 지속 세션으로 한 턴을 끝내고, CLI를 바꾸면 그 세션을 닫는다.
    let (status, job) = start(&state, json!({ "characterId": "claude-a", "prompt": "클로드 업무" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let claude_turn = job["turnId"].as_str().unwrap().to_string();
    assert_eq!(wait_for_turn(&state, &claude_turn, "claude-a").await, ("completed".to_string(), None));
    assert_eq!(
        messages(&pool, &claude_turn).await,
        [("assistant".to_string(), "클로드 답변입니다.".to_string()), ("user".to_string(), "클로드 업무".to_string())]
    );
    let (input, output, _, cost) = usage(&pool, &claude_turn).await.expect("Claude 사용량 기록");
    assert_eq!((input, output, cost), (Some(20), Some(7), Some(0.1)));
    let resolved: Option<String> = sqlx::query_scalar("SELECT resolved_model FROM turns WHERE id = $1::uuid")
        .bind(&claude_turn)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(resolved.as_deref(), Some("claude-opus-5"));
    wait_for_external_id(&pool, &claude_turn, "claude-session").await;
    let claude_pid: i32 = fixture.read("claude.pid").trim().parse().unwrap();
    assert!(process_alive(claude_pid), "지속 세션은 턴이 끝나도 살아 있어야 합니다.");
    assert_eq!(availability(&state, "claude-a").await["sessionOpen"], true);

    let codex_settings = json!({ "backend": "codex", "model": "gpt-5.6-sol", "effort": "high", "fastMode": false, "permission": "workspace-write" });
    let (status, body) = call(&state, "PUT", "/api/characters/claude-a/settings", Some(codex_settings)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(wait_for_exit(claude_pid).await, "CLI를 바꿨는데 Claude 지속 세션이 닫히지 않았습니다.");
    let (active, open): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM active_cli_sessions WHERE character_id = 'claude-a'),
                (SELECT count(*) FROM cli_sessions WHERE character_id = 'claude-a' AND ended_at IS NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((active, open), (0, 0));
    assert_eq!(availability(&state, "claude-a").await["sessionOpen"], false);

    // 재시작 복구: 실행 중으로 남은 턴과 진행 중 활동을 닫는다.
    let stale: String = sqlx::query_scalar(
        "WITH conversation AS (INSERT INTO conversations (title, workdir) VALUES ('복구', '/tmp') RETURNING id),
         session AS (INSERT INTO cli_sessions (conversation_id, character_id) SELECT id, 'codex-fail' FROM conversation RETURNING id),
         turn AS (INSERT INTO turns (cli_session_id, prompt, status) SELECT id, '복구', 'running' FROM session RETURNING id)
         INSERT INTO turn_activities (turn_id, seq, kind, text, status) SELECT id, 1, 'tool', '실행 중', 'running' FROM turn
         RETURNING turn_id::text",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(runtime.recover_interrupted_jobs().await.unwrap(), 1);
    let recovered: (String, Option<String>, String) = sqlx::query_as(
        "SELECT turn.status, turn.error_message, activity.status FROM turns turn
         JOIN turn_activities activity ON activity.turn_id = turn.id WHERE turn.id = $1::uuid",
    )
    .bind(&stale)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        recovered,
        (
            "interrupted".to_string(),
            Some("백엔드가 재시작되어 이전 실시간 출력 연결이 종료됐습니다.".to_string()),
            "failed".to_string()
        )
    );

    // 로컬 프로필 직원(Node resolveLocalCharacter). 준비가 4090 정의를 턴에 남기고, 로컬 서비스가 없으면
    // Claude 지속 세션보다 먼저 로컬 실행으로 가서 실패한다. 설정이 프로필과 다르거나 프로필이 꺼졌으면 접수가 500이다.
    let definition = json!({
        "profile": { "id": "local-test", "providerKind": "local", "backend": "claude", "model": "test-model", "endpoint": "http://127.0.0.1:41235",
            "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096,
            "usageProtocol": "anthropic-normalized-v1" },
        "host": { "address": "192.0.2.10", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test-host", "modelKey": "test/model", "comfyPort": 8188 },
    });
    sqlx::query("INSERT INTO local_agent_profiles (id, definition, enabled) VALUES ('local-test', $1, true)")
        .bind(&definition)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO characters (id, name, seat, backend, permission, config, model, effort, fast_mode)
         VALUES ('local-a', '로컬', '상단', 'claude', 'acceptEdits', $1, 'test-model', 'default', false)",
    )
    .bind(json!({ "localProfileId": "local-test", "executablePath": fixture.bin.join("claude").to_str().unwrap() }))
    .execute(&pool)
    .await
    .unwrap();
    let (status, job) = start(&state, json!({ "characterId": "local-a", "prompt": "로컬 업무" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let local_turn = job["turnId"].as_str().unwrap().to_string();
    assert_eq!(
        wait_for_turn(&state, &local_turn, "local-a").await,
        ("failed".to_string(), Some("Local provider service unavailable".to_string()))
    );
    let snapshot: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT provider_kind, provider_snapshot->'profile'->>'id', provider_snapshot->'profile'->>'model' FROM turns WHERE id = $1::uuid",
    )
    .bind(&local_turn)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snapshot, (Some("local".into()), Some("local-test".into()), Some("test-model".into())));
    sqlx::query("UPDATE characters SET effort = 'high' WHERE id = 'local-a'").execute(&pool).await.unwrap();
    let mismatch = json!({ "error": "Local employee settings do not match the profile" });
    assert_eq!(start(&state, json!({ "characterId": "local-a", "prompt": "로컬 업무" })).await, (StatusCode::INTERNAL_SERVER_ERROR, mismatch));
    sqlx::query("UPDATE characters SET effort = 'default' WHERE id = 'local-a'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE local_agent_profiles SET enabled = false WHERE id = 'local-test'").execute(&pool).await.unwrap();
    let disabled = json!({ "error": "Local profile is disabled or missing" });
    assert_eq!(start(&state, json!({ "characterId": "local-a", "prompt": "로컬 업무" })).await, (StatusCode::INTERNAL_SERVER_ERROR, disabled));
    runtime.shutdown();
}
