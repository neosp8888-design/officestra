// 터미널 세션 통합 시험. 실제 DB·HTTP 경로·빌드된 훅 실행 파일로 외부 터미널 Claude 세션의 등록·추적·API 전달·소유 토큰 검증을 본다.
// 실행 명세에는 소유 토큰이 있으므로 명세 전체를 단언 메시지나 출력에 넣지 않는다.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::launch::LaunchPaths;
use officestra_backend::agent_runtime::service::{RuntimeServices, SessionRoots};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::{Environment, LocalEmbeddingService};
use officestra_backend::terminal_sessions::{TerminalManagerOptions, TerminalSessionManager};
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

const HOOK: &str = env!("CARGO_BIN_EXE_officestra-terminal-hook");
const CLAUDE_SESSION: &str = "01a0c0de-4444-7000-8000-000000000004";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("officestra-terminal-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["bin", "work", "home"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let root = std::fs::canonicalize(&root).unwrap();
        // 앱이 PTY로 띄울 실행 파일 자리. 이 시험은 PTY를 띄우지 않고 훅 실행 파일로 CLI 사건만 흉내 낸다.
        let claude = root.join("bin").join("claude");
        std::fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn text(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

async fn request(state: &AppState, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(body) => Body::from(body.to_string()),
        None => Body::empty(),
    };
    let response = router(state.clone()).oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    request(state, method, path, body, &[(header::CONTENT_TYPE.as_str(), "application/json")]).await
}

async fn availability(state: &AppState) -> Value {
    let (status, body) = call(state, "GET", "/api/agent-jobs?characterId=term-claude", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let employee = &body["employees"][0];
    json!({ "state": employee["state"], "canReceive": employee["canReceive"] })
}

/// 빌드된 훅 실행 파일. Claude는 stdin, Codex는 첫 인자로 사건을 받는다. 어떤 실패든 종료 코드 0이어야 한다.
async fn hook(url: Option<&str>, argument: Option<&str>, stdin: &str) -> (bool, String) {
    let mut command = tokio::process::Command::new(HOOK);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    if let Some(url) = url {
        command.env("OFFICESTRA_TERMINAL_EVENTS_URL", url);
    }
    if let Some(argument) = argument {
        command.arg(argument);
    }
    command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.as_bytes()).await.unwrap();
    drop(input);
    let output = child.wait_with_output().await.unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stderr).into_owned())
}

/// 운영 백엔드(4317)로 새지 않도록 시험 서버 주소만 받는다.
async fn claude_hook(url: &str, port: u16, payload: Value) {
    assert!(url.starts_with(&format!("http://127.0.0.1:{port}/")), "시험 서버가 아닌 주소입니다: {url}");
    let (success, stderr) = hook(Some(url), None, &payload.to_string()).await;
    assert!(success);
    assert!(stderr.is_empty(), "{stderr}");
}

async fn latest_turn(pool: &PgPool) -> (String, String, String, String, Option<String>) {
    sqlx::query_as(
        "SELECT id::text, status, origin, prompt, sender_character_id FROM turns ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn turn_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turns").fetch_one(pool).await.unwrap()
}

async fn wait_for_status(pool: &PgPool, turn_id: &str, expected: &str) {
    for _ in 0..200 {
        let status: String =
            sqlx::query_scalar("SELECT status FROM turns WHERE id = $1::uuid").bind(turn_id).fetch_one(pool).await.unwrap();
        if status == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{turn_id} 턴이 {expected} 상태가 되지 않았습니다.");
}

async fn messages(pool: &PgPool, turn_id: &str) -> Vec<(String, String)> {
    sqlx::query_as("SELECT role, text FROM messages WHERE turn_id = $1::uuid ORDER BY role")
        .bind(turn_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn wait_for_dispatch(events: &Mutex<Vec<Value>>) -> String {
    for _ in 0..200 {
        let found = events
            .lock()
            .unwrap()
            .iter()
            .find(|event| event["type"] == "terminal.dispatch")
            .and_then(|event| event["dispatchId"].as_str().map(str::to_string));
        if let Some(id) = found {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("terminal.dispatch 알림이 오지 않았습니다.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 외부_터미널_claude_세션을_등록_추적하고_api_전달과_소유_토큰을_검증한다() {
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
        ("term-claude", "터미널", "claude", "acceptEdits", json!({ "executablePath": fixture.text("bin/claude") })),
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

    // 훅 실행 파일이 실제 HTTP로 사건을 보내도록 임시 포트에 서버를 띄운다.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 4317);
    let work = fixture.path("work");
    let runtime = Arc::new(Runtime::new(Some(work.clone()), Some(work.clone())));
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&events);
    let paths = LaunchPaths {
        result_tool_directory: fixture.text("tools"),
        node_executable: None,
        playwright_driver_directory: String::new(),
        runtime_id: "terminal-sessions-test".into(),
    };
    let roots = SessionRoots { home: fixture.text("home") };
    let services =
        RuntimeServices::new(pool.clone(), Arc::new(move |event| sink.lock().unwrap().push(event)), paths.clone(), roots.clone());
    assert!(runtime.attach_services(services));
    let environment = Environment::from([("PATH".to_string(), "/usr/bin:/bin".to_string()), ("LANG".to_string(), "ko_KR.UTF-8".to_string())]);
    let options = TerminalManagerOptions { port, ..TerminalManagerOptions::new(roots, paths, environment, HOOK.into()) };
    let manager = Arc::new(TerminalSessionManager::new(Arc::clone(&runtime), options));
    assert!(runtime.attach_terminal_sessions(&manager));
    let state = state(pool.clone(), Arc::clone(&runtime));
    let server_state = state.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router(server_state)).await });

    // 등록. 명세에는 소유 토큰이 있어 상태 오류만 보인다.
    let (status, spec) = call(&state, "POST", "/api/terminal-sessions", Some(json!({ "characterId": "term-claude" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{}", spec["error"]);
    let terminal_session_id = spec["terminalSessionId"].as_str().unwrap().to_string();
    let owner_token = spec["ownerToken"].as_str().unwrap().to_string();
    assert!(owner_token.len() == 64 && owner_token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let events_url = spec["env"]["OFFICESTRA_TERMINAL_EVENTS_URL"].as_str().unwrap().to_string();
    assert_eq!(events_url, format!("http://127.0.0.1:{port}/api/terminal-sessions/term-claude/events"));
    assert_eq!(spec["executable"], fixture.text("bin/claude"));
    assert_eq!(spec["cwd"], work.to_str().unwrap());
    assert_eq!(spec["backend"], "claude");
    let (status, body) = call(&state, "POST", "/api/terminal-sessions", Some(json!({ "characterId": "term-claude" }))).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "error": "터미널 모드에서 사용 중입니다." })));
    let (status, list) = call(&state, "GET", "/api/terminal-sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(list["sessions"][0]["characterId"], "term-claude");
    assert!(!list.to_string().contains(&owner_token), "목록에는 소유 토큰이 없어야 한다");
    assert_eq!(availability(&state).await, json!({ "state": "ready", "canReceive": true }));

    // 외부 터미널에서 직접 입력한 질문을 훅이 등록한다.
    let cwd = work.to_str().unwrap();
    claude_hook(&events_url, port, json!({ "hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "cwd": cwd, "prompt": "터미널 질문" }))
        .await;
    let (first, status, origin, prompt, sender) = latest_turn(&pool).await;
    assert_eq!((status.as_str(), origin.as_str(), prompt.as_str(), sender), ("running", "terminal", "터미널 질문", None));
    let external: Option<String> = sqlx::query_scalar("SELECT external_id FROM cli_sessions WHERE character_id = 'term-claude'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(external.as_deref(), Some(CLAUDE_SESSION));
    assert_eq!(availability(&state).await, json!({ "state": "running", "canReceive": false }));
    // 진행 중인 터미널 턴이 있으면 API 업무는 대기열 없이 409다.
    let (status, body) = call(&state, "POST", "/api/agent-jobs", Some(json!({ "characterId": "term-claude", "prompt": "API 업무" }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(turn_count(&pool).await, 1);
    claude_hook(&events_url, port, json!({ "hook_event_name": "Stop", "session_id": CLAUDE_SESSION, "cwd": cwd, "last_assistant_message": "터미널 답변" }))
        .await;
    wait_for_status(&pool, &first, "completed").await;
    assert_eq!(
        messages(&pool, &first).await,
        [("assistant".to_string(), "터미널 답변".to_string()), ("user".to_string(), "터미널 질문".to_string())]
    );
    assert_eq!(availability(&state).await, json!({ "state": "ready", "canReceive": true }));

    // API 전달. 앱만 소유 토큰으로 전달 본문을 가져갈 수 있고, 같은 CLI 훅이 받아야 202가 된다.
    events.lock().unwrap().clear();
    let job_state = state.clone();
    let job = tokio::spawn(async move {
        call(&job_state, "POST", "/api/agent-jobs", Some(json!({ "characterId": "term-claude", "prompt": "API 업무", "senderCharacterId": "sender" })))
            .await
    });
    let dispatch_id = wait_for_dispatch(&events).await;
    assert_eq!(availability(&state).await, json!({ "state": "pending", "canReceive": false }));
    let control = |extra: Value| {
        let mut body = json!({ "action": "claim", "dispatchId": dispatch_id, "terminalSessionId": terminal_session_id, "ownerToken": owner_token });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        body
    };
    let dispatch_path = "/api/terminal-sessions/term-claude/dispatch";
    let mismatch = json!({ "error": "터미널 소유 세션이 일치하지 않습니다." });
    for wrong in [json!({ "ownerToken": "0".repeat(64) }), json!({ "ownerToken": "" }), json!({ "terminalSessionId": "old-session" })] {
        let (status, body) = call(&state, "POST", dispatch_path, Some(control(wrong))).await;
        assert_eq!((status, body), (StatusCode::CONFLICT, mismatch.clone()));
    }
    let (status, body) = call(&state, "POST", dispatch_path, Some(control(json!({ "dispatchId": "other" })))).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "error": "만료되었거나 이미 처리한 터미널 요청입니다." })));
    assert!(!job.is_finished(), "잘못된 확인은 전달을 끝내지 않는다");
    let (status, claimed) = call(&state, "POST", dispatch_path, Some(control(json!({})))).await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    let wire = claimed["prompt"].as_str().unwrap().to_string();
    assert_eq!(wire, format!("[OFFICESTRA_REQUEST:{dispatch_id}]\nAPI 업무"));
    let (status, _) = call(&state, "POST", dispatch_path, Some(control(json!({})))).await;
    assert_eq!(status, StatusCode::CONFLICT, "같은 전달은 한 번만 가져간다");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!job.is_finished(), "PTY에 쓴 것만으로는 접수가 아니다");
    claude_hook(&events_url, port, json!({ "hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "cwd": cwd, "prompt": wire })).await;
    let (status, accepted) = job.await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let (second, status, origin, prompt, sender) = latest_turn(&pool).await;
    assert_eq!(accepted["turnId"], second);
    assert_eq!(accepted["status"], "running");
    assert_eq!((status.as_str(), origin.as_str(), prompt.as_str(), sender.as_deref()), ("running", "terminal", "API 업무", Some("sender")));
    claude_hook(&events_url, port, json!({ "hook_event_name": "Stop", "session_id": CLAUDE_SESSION, "cwd": cwd, "last_assistant_message": "API 답변" }))
        .await;
    wait_for_status(&pool, &second, "completed").await;

    // 앱의 중단 요청은 진행 중인 터미널 턴을 중단으로 닫는다.
    claude_hook(&events_url, port, json!({ "hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "cwd": cwd, "prompt": "중단할 질문" }))
        .await;
    let (third, status, ..) = latest_turn(&pool).await;
    assert_eq!(status, "running");
    let (status, body) = call(&state, "POST", "/api/terminal-sessions/term-claude/interrupt", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for_status(&pool, &third, "interrupted").await;

    // 사건 경로의 요청 검사. 훅은 Origin 없이 JSON으로만 보낸다.
    let event = json!({ "source": "claude", "payload": { "hook_event_name": "UserPromptSubmit", "prompt": "막힐 질문" } });
    let events_path = "/api/terminal-sessions/term-claude/events";
    let (status, _) = request(&state, "POST", events_path, Some(event.clone()), &[("content-type", "text/plain")]).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let foreign = [("content-type", "application/json"), ("origin", "https://example.test"), ("host", "127.0.0.1:4318")];
    let (status, _) = request(&state, "POST", events_path, Some(event), &foreign).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(turn_count(&pool).await, 3);

    // 훅 실행 파일은 주소가 없거나 본문이 깨졌거나 서버가 없어도 CLI를 막지 않는다(종료 코드 0).
    assert_eq!(hook(None, None, "{}").await, (true, String::new()));
    let (success, stderr) = hook(Some(&events_url), None, "{").await;
    assert!(success && stderr.contains("OFFICESTRA 터미널 이벤트 전달 실패"), "{stderr}");
    let (success, stderr) = hook(Some("http://127.0.0.1:9/api/terminal-sessions/term-claude/events"), None, "{}").await;
    assert!(success && stderr.contains("OFFICESTRA 터미널 이벤트 전달 실패"), "{stderr}");
    assert_eq!(turn_count(&pool).await, 3);

    // 닫기.
    let (status, body) = call(&state, "DELETE", "/api/terminal-sessions/term-claude", Some(json!({}))).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
    let (status, _) = call(&state, "DELETE", "/api/terminal-sessions/term-claude", Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(&state, "GET", "/api/terminal-sessions", None).await;
    assert_eq!(list, json!({ "sessions": [] }));
    let (status, _) = call(&state, "POST", dispatch_path, Some(control(json!({})))).await;
    assert_eq!(status, StatusCode::CONFLICT, "닫힌 터미널의 전달 확인은 거절한다");
    server.abort();
}
