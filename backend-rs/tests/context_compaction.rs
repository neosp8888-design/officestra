// 컨텍스트 압축·직원 이름·역할 지침·자동 압축 기준·직원 이력 경로 통합 시험. 모의 codex app-server와 claude 지속 세션으로
// Node와 같은 응답·방송·자동 압축 기준을 본다.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::launch::LaunchPaths;
use officestra_backend::agent_runtime::service::{RuntimeServices, SessionRoots};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::js::to_js_json;
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

/// Node claude-persistent-worker.mjs의 COMPACT_PROMPT.
const COMPACT_PROMPT: &str =
    "/compact OFFICESTRA 직원 이름과 직급 호칭을 쓰지 말고, 사용자는 '사용자'로 표현하며 요청·결정·진행 상태만 내용 중심으로 요약해 주세요.";
const BUSY: &str = "이 직원의 현재 업무와 후속 처리가 끝난 뒤 압축하세요.";
const NOT_FOUND: &str = "캐릭터를 찾을 수 없습니다.";
const UNSUPPORTED: &str = "상태 변경 요청은 application/json 형식이어야 합니다.";

/// 업무 턴과 압축 모두 app-server다. 압축 시작을 받은 프로세스만 인자·폴더·받은 입력을 남기고,
/// 기록에 압축 뒤 점유를 덧붙인 뒤 compact-release를 기다린다. 업무 턴은 wait 모드면 release를 기다렸다 끝낸다.
/// 시험이 중간에 끊겨 폴더가 지워지면 대기를 끝내 고아 프로세스로 남지 않는다.
const FAKE_CODEX: &str = r##"#!/bin/sh
dir='__DIR__'
mode='__MODE__'
thread='__THREAD__'
turn="turn-$mode"
received="$dir/app-server-$$.log"
field() { printf '%s\n' "$line" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$received"
  id=$(printf '%s\n' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize",'*) echo "{\"id\":$id,\"result\":{\"userAgent\":\"test\"}}" ;;
    *'"method":"thread/start",'*) echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"$thread\"}}}" ;;
    *'"method":"thread/resume",'*)
      thread=$(field threadId)
      echo "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"$thread\"}}}" ;;
    *'"method":"thread/compact/start",'*)
      pwd > "$dir/app-server-cwd.txt"
      echo "$*" > "$dir/app-server-args.txt"
      cat "$received" >> "$dir/app-server-input.log"
      printf '%s\n' '__AFTER__' >> '__ROLLOUT__'
      while [ ! -f "$dir/compact-release" ] && [ -d "$dir" ]; do sleep 0.05; done
      echo "{\"id\":$id,\"result\":{}}"
      echo "{\"method\":\"item/completed\",\"params\":{\"threadId\":\"$thread\",\"turnId\":\"turn-compact\",\"item\":{\"type\":\"contextCompaction\"}}}" ;;
    *'"method":"turn/start",'*)
      echo "{\"id\":$id,\"result\":{\"turn\":{\"id\":\"$turn\"}}}"
      if [ "$mode" = wait ]; then
        while [ ! -f "$dir/release" ] && [ -d "$dir" ]; do sleep 0.05; done
      fi
      echo "{\"method\":\"item/completed\",\"params\":{\"threadId\":\"$thread\",\"turnId\":\"$turn\",\"item\":{\"id\":\"m1\",\"type\":\"agentMessage\",\"text\":\"모의 답변입니다.\"}}}"
      echo "{\"method\":\"thread/tokenUsage/updated\",\"params\":{\"threadId\":\"$thread\",\"turnId\":\"$turn\",\"tokenUsage\":{\"total\":{\"inputTokens\":10,\"cachedInputTokens\":0,\"outputTokens\":5}}}}"
      echo "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"status\":\"completed\"}}}" ;;
  esac
done
rm -f "$received"
"##;

/// /compact 입력에는 실제 CLI처럼 기록에 압축 경계를 남기고, 출력의 압축 경계에는 기록과 다른 토큰 수를 싣는다.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir='__DIR__'
echo $$ >> "$dir/claude-spawns.log"
while IFS= read -r line; do
  case "$line" in
    *'"type":"control_request"'*) ;;
    *'"content":"/compact '*)
      printf '%s\n' "$line" >> "$dir/compact-input.log"
      printf '%s\n' '{"type":"system","subtype":"compact_boundary","timestamp":"2026-10-01T00:00:02.000Z","compact_metadata":{"trigger":"manual","pre_tokens":900000,"post_tokens":45000}}' >> '__TRANSCRIPT__'
      echo '{"type":"system","subtype":"compact_boundary","session_id":"claude-session","compact_metadata":{"trigger":"manual","pre_tokens":910000,"post_tokens":40000}}'
      echo '{"type":"result","subtype":"success","is_error":false,"result":"","session_id":"claude-session"}' ;;
    *)
      echo '{"type":"system","subtype":"init","session_id":"claude-session","model":"claude-sonnet-5"}'
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
        let root = std::env::temp_dir().join(format!("officestra-context-compaction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["bin", "work", "home"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        // macOS의 /var는 /private/var 링크라 실행기의 저장소 비교와 맞추려고 실제 경로를 쓴다.
        let root = std::fs::canonicalize(&root).unwrap();
        let bin = root.join("bin");
        let fixture = Self { root, bin };
        let directory = fixture.root.to_str().unwrap();
        // 실행기는 파일 이름이 codex인 것만 받으므로 대기 모드 스크립트는 하위 폴더에 둔다.
        for (folder, mode, thread) in [("", "ok", "thread-ok"), ("wait", "wait", "thread-wait")] {
            let codex = FAKE_CODEX
                .replace("__DIR__", directory)
                .replace("__MODE__", mode)
                .replace("__THREAD__", thread)
                .replace("__ROLLOUT__", fixture.rollout().to_str().unwrap())
                .replace("__AFTER__", &codex_token_count("2026-10-01T00:00:01.000Z", 31_000));
            std::fs::create_dir_all(fixture.bin.join(folder)).unwrap();
            write_script(&fixture.bin.join(folder).join("codex"), &codex);
        }
        let claude = FAKE_CLAUDE.replace("__DIR__", directory).replace("__TRANSCRIPT__", fixture.transcript().to_str().unwrap());
        write_script(&fixture.bin.join("claude"), &claude);
        fixture
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

    fn rollout(&self) -> PathBuf {
        self.path("home/.codex/sessions/2026/10/01/rollout-2026-10-01T00-00-00-thread-ok.jsonl")
    }

    /// Claude Code가 작업 폴더 이름으로 만드는 기록 위치다.
    fn transcript(&self) -> PathBuf {
        let project = self.path("work").to_str().unwrap().replace(['/', '.'], "-");
        self.path("home/.claude/projects").join(project).join("claude-session.jsonl")
    }

    fn append(&self, path: &Path, line: &str) {
        use std::io::Write;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::write(self.path("release"), "");
        let _ = std::fs::write(self.path("compact-release"), "");
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn codex_token_count(timestamp: &str, input: u64) -> String {
    json!({
        "timestamp": timestamp,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": { "last_token_usage": { "input_tokens": input, "output_tokens": 1 }, "model_context_window": 258_400 },
        },
    })
    .to_string()
}

fn claude_assistant(timestamp: &str, input: u64) -> String {
    json!({
        "type": "assistant",
        "timestamp": timestamp,
        "sessionId": "claude-session",
        "message": {
            "model": "claude-sonnet-5",
            "role": "assistant",
            "usage": { "input_tokens": input, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 1 },
        },
    })
    .to_string()
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

/// 머리말을 고를 수 있는 요청. 본문이 있으면 그대로 보낸다.
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

/// JSON 본문이면 application/json을 붙인다.
async fn call(state: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    match body {
        Some(body) => raw(state, method, path, &[(header::CONTENT_TYPE, "application/json")], &body.to_string()).await,
        None => raw(state, method, path, &[], "").await,
    }
}

/// PUT /api/characters/:id/{setting}.
async fn put(state: &AppState, character_id: &str, setting: &str, body: Value) -> (StatusCode, Value) {
    call(state, "PUT", &format!("/api/characters/{character_id}/{setting}"), Some(body)).await
}

async fn updated_at(pool: &PgPool, character_id: &str) -> String {
    sqlx::query_scalar("SELECT updated_at::text FROM characters WHERE id = $1").bind(character_id).fetch_one(pool).await.unwrap()
}

async fn compact(state: &AppState, character_id: &str) -> (StatusCode, Value) {
    call(state, "POST", &format!("/api/characters/{character_id}/context/compact"), Some(json!({}))).await
}

async fn start(state: &AppState, character_id: &str, prompt: &str) -> String {
    let (status, job) = call(state, "POST", "/api/agent-jobs", Some(json!({ "characterId": character_id, "prompt": prompt }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    job["turnId"].as_str().unwrap().to_string()
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

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("10초 안에 {what} 상태가 되지 않았습니다.");
}

/// 실시간 전송과 같은 JS 숫자 표기로 맞춘다(230000.0 → 230000).
fn js(value: &Value) -> Value {
    serde_json::from_slice(&to_js_json(value).unwrap()).unwrap()
}

/// 받은 방송 중 압축 이벤트만 꺼내고 비운다.
fn take_context_events(events: &Mutex<Vec<Value>>) -> Vec<Value> {
    let mut events = events.lock().unwrap();
    let context = events
        .iter()
        .filter(|event| event["type"].as_str().is_some_and(|kind| kind.starts_with("context.")))
        .map(js)
        .collect();
    events.clear();
    context
}

fn has_event(events: &Mutex<Vec<Value>>, kind: &str, character_id: &str) -> bool {
    events.lock().unwrap().iter().any(|event| event["type"] == kind && event["characterId"] == character_id)
}

fn started(character_id: &str, automatic: bool) -> Value {
    json!({ "type": "context.compaction.started", "characterId": character_id, "automatic": automatic })
}

fn failed(character_id: &str, message: &str) -> Value {
    json!({ "type": "context.compaction.failed", "characterId": character_id, "automatic": false, "errorMessage": message })
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn 실행기가_없으면_압축은_503이고_설정_경로는_본문부터_읽는다() {
    let pool = db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap();
    let state = state(pool, Arc::new(Runtime::new(None, None)));
    let json_type = (header::CONTENT_TYPE, "application/json");

    // 압축은 출처 검사·본문 해석·실행기 확인 순이다.
    let (status, body) = call(&state, "POST", "/api/characters/boss/context/compact", None).await;
    assert_eq!((status, body), (StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": UNSUPPORTED })));
    let evil = [json_type.clone(), (header::ORIGIN, "http://evil.example")];
    let (status, body) = raw(&state, "POST", "/api/characters/boss/context/compact", &evil, "{}").await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, json!({ "error": "신뢰할 수 없는 요청 출처입니다." })));
    let (status, _) = raw(&state, "POST", "/api/characters/boss/context/compact", &[json_type.clone()], "{").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body) = compact(&state, "boss").await;
    assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "CLI 실행기가 준비되지 않았습니다." })));

    // null 본문은 DB에 닿기 전에 Node TypeError 문구로 500이다.
    for (path, key) in [("name", "name"), ("identity-prompt", "identityPrompt"), ("context-settings", "autoCompactPercent")] {
        let (status, body) = call(&state, "PUT", &format!("/api/characters/boss/{path}"), Some(Value::Null)).await;
        let message = format!("Cannot read properties of null (reading '{key}')");
        assert_eq!((status, body), (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": message })), "{path}");
    }
    // 이름·역할 지침은 Node처럼 출처를 보지 않고, 자동 압축 기준만 본다.
    let (status, body) = raw(&state, "PUT", "/api/characters/boss/name", &evil, r#"{"name":"  "}"#).await;
    assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({ "error": "이름은 1자 이상 30자 이하여야 합니다." })));
    let (status, body) = raw(&state, "PUT", "/api/characters/boss/identity-prompt", &[], r#"{"identityPrompt":""}"#).await;
    assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({ "error": "역할·업무 지침은 1자 이상 1,200자 이하여야 합니다." })));
    let (status, body) = raw(&state, "PUT", "/api/characters/boss/context-settings", &evil, r#"{"autoCompactPercent":50}"#).await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, json!({ "error": "신뢰할 수 없는 요청 출처입니다." })));
    let (status, body) = raw(&state, "PUT", "/api/characters/boss/context-settings", &[], r#"{"autoCompactPercent":50}"#).await;
    assert_eq!((status, body), (StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": UNSUPPORTED })));
}

#[tokio::test]
async fn 모의_cli로_압축하고_직원_설정과_이력을_돌려준다() {
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
        ("codex-wait", "대기", "codex", "workspace-write", fixture.config("wait/codex")),
        ("claude-a", "클로드", "claude", "acceptEdits", fixture.config("claude")),
        ("fresh", "새 직원", "codex", "workspace-write", fixture.config("codex")),
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
            runtime_id: "context-compaction-test".into(),
        },
        SessionRoots { home: fixture.path("home").to_str().unwrap().to_string() },
    );
    services.claude_suggestion_grace = Duration::from_millis(50);
    services.claude_interrupt_result_timeout = Duration::from_millis(500);
    assert!(runtime.attach_services(services));
    let state = state(pool.clone(), Arc::clone(&runtime));

    // 직원 이름. 앞뒤 공백을 자르고 UTF-16 길이로 잰다.
    let name = |value: Value| put(&state, "codex-a", "name", json!({ "name": value }));
    assert_eq!(name(json!("  새 코덱스  ")).await, (StatusCode::OK, json!({ "id": "codex-a", "name": "새 코덱스" })));
    let too_long = json!({ "error": "이름은 1자 이상 30자 이하여야 합니다." });
    assert_eq!(name(json!("😀".repeat(16))).await, (StatusCode::BAD_REQUEST, too_long.clone()));
    assert_eq!(name(json!("가".repeat(31))).await, (StatusCode::BAD_REQUEST, too_long));
    assert_eq!(name(json!("😀".repeat(15))).await, (StatusCode::OK, json!({ "id": "codex-a", "name": "😀".repeat(15) })));
    assert_eq!(name(json!("코덱스")).await, (StatusCode::OK, json!({ "id": "codex-a", "name": "코덱스" })));
    let (status, body) = put(&state, "nobody", "name", json!({ "name": "누구" })).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": NOT_FOUND })));

    // 역할 지침. 같은 값이면 갱신 시각을 바꾸지 않는다.
    let identity = |value: Value| put(&state, "codex-a", "identity-prompt", json!({ "identityPrompt": value }));
    let saved = json!({ "id": "codex-a", "identityPrompt": "역할 지침" });
    assert_eq!(identity(json!("  역할 지침  ")).await, (StatusCode::OK, saved.clone()));
    let before = updated_at(&pool, "codex-a").await;
    assert_eq!(identity(json!("역할 지침")).await, (StatusCode::OK, saved));
    assert_eq!(updated_at(&pool, "codex-a").await, before);
    let invalid = json!({ "error": "역할·업무 지침은 1자 이상 1,200자 이하여야 합니다." });
    assert_eq!(identity(json!("가".repeat(1201))).await, (StatusCode::BAD_REQUEST, invalid));
    let (status, body) = put(&state, "nobody", "identity-prompt", json!({ "identityPrompt": "지침" })).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": NOT_FOUND })));

    // 자동 압축 기준. 20~100 정수만 받고 클라우드 모델은 95%까지다.
    let percent = |character_id: &'static str, body: Value| put(&state, character_id, "context-settings", body);
    let range = json!({ "error": "자동 압축 기준은 20% 이상 100% 이하의 정수여야 합니다." });
    for value in [json!({ "autoCompactPercent": 19 }), json!({ "autoCompactPercent": 20.5 }), json!({ "autoCompactPercent": "abc" }), json!({})] {
        assert_eq!(percent("codex-a", value).await, (StatusCode::BAD_REQUEST, range.clone()));
    }
    let cloud = json!({ "error": "클라우드 모델의 자동 압축 기준은 최대 95%입니다." });
    assert_eq!(percent("codex-a", json!({ "autoCompactPercent": 96 })).await, (StatusCode::BAD_REQUEST, cloud));
    assert_eq!(
        percent("codex-a", json!({ "autoCompactPercent": "20" })).await,
        (StatusCode::OK, json!({ "id": "codex-a", "autoCompactPercent": 20 }))
    );
    assert_eq!(
        percent("nobody", json!({ "autoCompactPercent": 50 })).await,
        (StatusCode::NOT_FOUND, json!({ "error": NOT_FOUND }))
    );
    assert_eq!(
        percent("claude-a", json!({ "autoCompactPercent": 90 })).await,
        (StatusCode::OK, json!({ "id": "claude-a", "autoCompactPercent": 90 }))
    );

    // Codex는 기준을 넘어도 턴 뒤 자동 압축하지 않는다.
    fixture.append(&fixture.rollout(), &codex_token_count("2026-10-01T00:00:00.000Z", 230_000));
    let codex_turn = start(&state, "codex-a", "MOCK_OK 첫 업무").await;
    assert_eq!(wait_for_turn(&state, &codex_turn, "codex-a").await, ("completed".to_string(), None));
    wait_for_external_id(&pool, &codex_turn, "thread-ok").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(take_context_events(&events), Vec::<Value>::new());

    // Codex 수동 압축. 진행 중에는 같은 직원 압축을 막고, 새 실시간 연결의 ready에 압축 중 직원을 싣는다.
    let held_state = state.clone();
    let held = tokio::spawn(async move { compact(&held_state, "codex-a").await });
    let compact_line = r#"{"id":3,"method":"thread/compact/start","params":{"threadId":"thread-ok"}}"#;
    wait_until("Codex 압축 대기", || fixture.read("app-server-input.log").lines().any(|line| line == compact_line)).await;
    assert_eq!(runtime.compacting_character_ids(), ["codex-a"]);
    assert_eq!(compact(&state, "codex-a").await, (StatusCode::CONFLICT, json!({ "error": BUSY })));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_state = state.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router(server_state)).await.unwrap() });
    let (mut client, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        Message::text(r#"{"type":"ready","compactingCharacterIds":["codex-a"]}"#)
    );
    drop(client);
    server.abort();
    std::fs::write(fixture.path("compact-release"), "").unwrap();
    let codex_payload = json!({
        "ok": true, "automatic": false, "backend": "codex", "sessionId": "thread-ok",
        "preTokens": 230_000, "postTokens": 31_000, "limitTokens": 258_400,
    });
    assert_eq!(held.await.unwrap(), (StatusCode::OK, codex_payload));
    assert_eq!(fixture.read("app-server-args.txt"), "app-server --stdio\n");
    assert_eq!(fixture.read("app-server-cwd.txt"), format!("{}\n", work.to_str().unwrap()));
    let resume = format!(r#"{{"id":2,"method":"thread/resume","params":{{"threadId":"thread-ok","cwd":"{}"}}}}"#, work.to_str().unwrap());
    assert_eq!(
        fixture.read("app-server-input.log").lines().collect::<Vec<_>>(),
        [
            r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"officestra","title":"OFFICESTRA","version":"1"},"capabilities":{"experimentalApi":false,"requestAttestation":false}}}"#,
            r#"{"method":"initialized"}"#,
            resume.as_str(),
            compact_line,
        ]
    );
    assert_eq!(
        take_context_events(&events),
        [
            started("codex-a", false),
            json!({
                "type": "context.compacted", "characterId": "codex-a", "automatic": false,
                "preTokens": 230_000, "postTokens": 31_000, "limitTokens": 258_400,
            }),
        ]
    );
    assert_eq!(runtime.compacting_character_ids(), Vec::<String>::new());

    // Claude 첫 턴은 기록이 없어 점유를 모르므로 압축하지 않는다.
    let claude_turn = start(&state, "claude-a", "첫 업무").await;
    assert_eq!(wait_for_turn(&state, &claude_turn, "claude-a").await, ("completed".to_string(), None));
    wait_for_external_id(&pool, &claude_turn, "claude-session").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(take_context_events(&events), Vec::<Value>::new());

    // 기준(90%) 바로 아래면 압축하지 않는다.
    fixture.append(&fixture.transcript(), &claude_assistant("2026-10-01T00:00:00.000Z", 899_999));
    let claude_turn = start(&state, "claude-a", "둘째 업무").await;
    assert_eq!(wait_for_turn(&state, &claude_turn, "claude-a").await, ("completed".to_string(), None));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(take_context_events(&events), Vec::<Value>::new());

    // 기준에 닿으면 같은 세션을 자동 압축한다. 토큰 수는 CLI가 보고한 압축 경계 값을 쓴다.
    fixture.append(&fixture.transcript(), &claude_assistant("2026-10-01T00:00:01.000Z", 900_000));
    let claude_turn = start(&state, "claude-a", "셋째 업무").await;
    assert_eq!(wait_for_turn(&state, &claude_turn, "claude-a").await, ("completed".to_string(), None));
    wait_until("Claude 자동 압축 완료", || has_event(&events, "context.compacted", "claude-a")).await;
    let claude_compacted = |automatic: bool| {
        json!({
            "type": "context.compacted", "characterId": "claude-a", "automatic": automatic,
            "preTokens": 910_000, "postTokens": 40_000, "limitTokens": 1_000_000,
        })
    };
    assert_eq!(take_context_events(&events), [started("claude-a", true), claude_compacted(true)]);
    let compact_inputs: Vec<Value> =
        fixture.read("compact-input.log").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(
        compact_inputs,
        [json!({ "type": "user", "message": { "role": "user", "content": COMPACT_PROMPT }, "parent_tool_use_id": null })]
    );

    // Claude 수동 압축.
    assert_eq!(
        compact(&state, "claude-a").await,
        (
            StatusCode::OK,
            json!({
                "ok": true, "automatic": false, "backend": "claude", "sessionId": "claude-session",
                "preTokens": 910_000, "postTokens": 40_000, "limitTokens": 1_000_000,
            })
        )
    );
    assert_eq!(take_context_events(&events), [started("claude-a", false), claude_compacted(false)]);
    assert_eq!(fixture.read("compact-input.log").lines().count(), 2);

    // 대상이 없으면 시작·실패를 방송하고 404다.
    assert_eq!(compact(&state, "nobody").await, (StatusCode::NOT_FOUND, json!({ "error": NOT_FOUND })));
    assert_eq!(take_context_events(&events), [started("nobody", false), failed("nobody", NOT_FOUND)]);
    let no_session = "압축할 활성 CLI 세션이 없습니다.";
    assert_eq!(compact(&state, "fresh").await, (StatusCode::NOT_FOUND, json!({ "error": no_session })));
    assert_eq!(take_context_events(&events), [started("fresh", false), failed("fresh", no_session)]);

    // 업무 중이면 시작 방송 없이 409다.
    let wait_turn = start(&state, "codex-wait", "MOCK_WAIT 대기 업무").await;
    wait_for_external_id(&pool, &wait_turn, "thread-wait").await;
    assert_eq!(compact(&state, "codex-wait").await, (StatusCode::CONFLICT, json!({ "error": BUSY })));
    assert_eq!(take_context_events(&events), Vec::<Value>::new());
    std::fs::write(fixture.path("release"), "").unwrap();
    assert_eq!(wait_for_turn(&state, &wait_turn, "codex-wait").await, ("completed".to_string(), None));
    take_context_events(&events);

    // Antigravity는 수동 압축을 지원하지 않는다.
    sqlx::query("UPDATE characters SET backend = 'antigravity' WHERE id = 'codex-wait'").execute(&pool).await.unwrap();
    let antigravity = "Antigravity CLI는 수동 컨텍스트 압축을 지원하지 않습니다.";
    assert_eq!(compact(&state, "codex-wait").await, (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": antigravity })));
    assert_eq!(take_context_events(&events), [started("codex-wait", false), failed("codex-wait", antigravity)]);

    // 로컬 모델 직원은 수동 압축 대신 CLI 자체 압축을 쓰고, 자동 압축 기준은 100%까지 받는다.
    sqlx::query("UPDATE characters SET config = config || '{\"localProfileId\":\"local-a\"}'::jsonb WHERE id = 'codex-a'")
        .execute(&pool)
        .await
        .unwrap();
    let local = "로컬 AI는 CLI 기본 자동 압축을 사용합니다. 수동 압축은 터미널에서 /compact를 사용하세요.";
    assert_eq!(compact(&state, "codex-a").await, (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": local })));
    assert_eq!(take_context_events(&events), [started("codex-a", false), failed("codex-a", local)]);
    for value in [96, 100] {
        assert_eq!(
            percent("codex-a", json!({ "autoCompactPercent": value })).await,
            (StatusCode::OK, json!({ "id": "codex-a", "autoCompactPercent": value }))
        );
    }
    assert_eq!(percent("codex-a", json!({ "autoCompactPercent": 101 })).await, (StatusCode::BAD_REQUEST, range));

    // 전환 준비 중이면 시작 방송 없이 503이다.
    runtime.begin_drain();
    let draining = "백엔드가 안전한 전환을 준비 중이라 컨텍스트를 압축하지 않습니다.";
    assert_eq!(compact(&state, "claude-a").await, (StatusCode::SERVICE_UNAVAILABLE, json!({ "error": draining })));
    assert_eq!(take_context_events(&events), Vec::<Value>::new());
    runtime.cancel_drain();
    let (status, body) = call(&state, "POST", "/api/characters/claude-a/context/compact", None).await;
    assert_eq!((status, body), (StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": UNSUPPORTED })));

    // 직원 이력. 세션은 최근 순, 턴도 최근 순이다.
    let (status, history) = call(&state, "GET", "/api/characters/codex-a/history", None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(keys(&history), ["character", "sessions"]);
    assert_eq!(history["character"], json!({ "id": "codex-a", "name": "코덱스", "seat": "상단", "backend": "codex" }));
    let sessions = history["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(keys(&sessions[0]), ["id", "externalId", "startedAt", "endedAt", "turns"]);
    assert_eq!((&sessions[0]["externalId"], &sessions[0]["endedAt"]), (&json!("thread-ok"), &Value::Null));
    let turns = sessions[0]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        keys(&turns[0]),
        [
            "id", "sessionId", "executionBackend", "executionModel", "resolvedModel", "executionEffort", "executionFastMode",
            "origin", "promptSender", "requestSource", "providerKind", "providerContextWindow", "conversationWorkdir", "prompt",
            "startedAt", "endedAt", "responseSourceWarning", "wikiProposalWarning", "response", "sources",
        ]
    );
    assert_eq!(turns[0]["id"], codex_turn.as_str());
    assert_eq!(turns[0]["sessionId"], sessions[0]["id"]);
    assert_eq!(turns[0]["executionBackend"], "codex");
    assert_eq!(turns[0]["prompt"], "MOCK_OK 첫 업무");
    assert_eq!(turns[0]["response"], "모의 답변입니다.");
    let (status, history) = call(&state, "GET", "/api/characters/claude-a/history", None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let prompts: Vec<&Value> = history["sessions"][0]["turns"].as_array().unwrap().iter().map(|turn| &turn["prompt"]).collect();
    assert_eq!(prompts, [&json!("셋째 업무"), &json!("둘째 업무"), &json!("첫 업무")]);
    let (status, body) = call(&state, "GET", "/api/characters/nobody/history", None).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": NOT_FOUND })));

    runtime.shutdown();
}
