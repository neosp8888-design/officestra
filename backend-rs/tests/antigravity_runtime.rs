// Antigravity 실행 흐름 통합 시험. 모의 agy 셸 스크립트와 임시 대화 SQLite로 접수·완료·실패·중단, 추론 모니터, 터미널 후속 갱신을 Node 시험과 같은 기대값으로 본다.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::launch::LaunchPaths;
use officestra_backend::agent_runtime::service::{RuntimeServices, SessionRoots};
use officestra_backend::antigravity_model_selection::antigravity_model_arguments;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::terminal_sessions::{TerminalRuntime, TerminalTurnCompletion, TerminalTurnStart};
use officestra_backend::{db, migrate};
use rusqlite::Connection;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const MODEL: &str = "gemini-3.7-flash";
const FAKE_NODE: &str = "/opt/officestra-test/node";
const STALE_QUOTA_ERROR: &str =
    "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 154h37m12s.";
const STEPS_CONVERSATION: &str = "a1111111-2222-4333-8444-000000000001";
const REASON_CONVERSATION: &str = "a1111111-2222-4333-8444-000000000002";
const CORRUPT_CONVERSATION: &str = "a1111111-2222-4333-8444-000000000003";
const USAGE_CONVERSATION: &str = "a1111111-2222-4333-8444-000000000004";

/// 실행 프롬프트의 SCENARIO·CONV 표식으로 동작을 고른다. 받은 인자는 NUL로 나눠 남기고, Playwright 환경 두 값은 줄로 남긴다.
/// 응답 단계 줄은 Node 시험(agent-runtime.test.mjs)과 실제 agy stream-json 순서를 따른다.
const FAKE_AGY: &str = r#"#!/bin/sh
dir='__DIR__'
scenario=$(printf '%s\n' "$2" | sed -n 's/.*SCENARIO=\([a-z-]*\).*/\1/p' | head -n 1)
conv=$(printf '%s\n' "$2" | sed -n 's/.*CONV=\([0-9a-f-]*\).*/\1/p' | head -n 1)
for argument; do printf '%s\0' "$argument"; done > "$dir/$scenario.args"
printf '%s\n%s\n' "${PLAYWRIGHT_DRIVER_PATH-unset}" "${PLAYWRIGHT_NODEJS_PATH-unset}" > "$dir/$scenario.env"
step() { printf '{"event":"step_update","step_update":{"conversation_id":"%s","step_index":%s,%s}}\n' "$conv" "$1" "$2"; }
answer() { step "$1" '"state":"DONE","step_type":"agent_response","text_delta":"'"$2"'\n"'; }
tool() { step "$1" '"state":"'"$2"'","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"'"$3"'"}}'; }
result() { printf '{"event":"result","result":{%s}}\n' "$1"; }
wait_for() { while [ ! -f "$dir/$1" ] && [ -d "$dir" ]; do sleep 0.05; done; }
[ -n "$conv" ] && printf '{"event":"init","conversation_id":"%s","init":{"model":"gemini-3.7-flash"}}\n' "$conv"
case "$scenario" in
  steps)
    step 1 '"state":"ACTIVE","step_type":"agent_response","text_delta":"첫 번째 메시지입니다."'
    answer 1 ''
    tool 2 ACTIVE 'echo hi'
    tool 2 DONE 'echo hi'
    answer 3 '두 번째 메시지입니다.'
    tool 4 DONE 'echo bye'
    answer 5 '끝났습니다.'
    step 6 '"state":"DONE","step_type":"agent_response","usage":{"input_tokens":10,"output_tokens":1}'
    result '"conversation_id":"'"$conv"'","status":"SUCCESS","response":"첫 번째 메시지입니다.\n두 번째 메시지입니다.\n끝났습니다.\n"' ;;
  reason)
    answer 3 '중간 메시지입니다.'
    : > "$dir/reason.ready"
    wait_for reason.answer
    answer 9 '최종 답변입니다.'
    : > "$dir/reason.answered"
    wait_for reason.finish
    result '"status":"SUCCESS","response":"중간 메시지입니다.\n최종 답변입니다.\n"' ;;
  resume)
    answer 10 '재개 답변입니다.'
    result '"status":"SUCCESS","response":"재개 답변입니다.\n"' ;;
  corrupt)
    answer 1 '손상된 기록에도 끝납니다.'
    sleep 0.6
    result '"status":"SUCCESS","response":"손상된 기록에도 끝납니다.\n"' ;;
  usage-first)
    answer 1 '첫 업무를 마쳤습니다.'
    result '"status":"SUCCESS","response":"첫 업무를 마쳤습니다.\n"' ;;
  usage-wait)
    echo $$ > "$dir/usage-wait.pid"
    : > "$dir/usage-wait.ready"
    wait_for never ;;
  exit-success)
    result '"status":"SUCCESS","response":"성공한 응답입니다"'
    exit 1 ;;
  stderr)
    printf '{"event":"warning","warning":"이전 쿼터 경고"}\n'
    echo '진짜 프로세스 에러' >&2
    exit 1 ;;
  stale-ok)
    result '"status":"ERROR","response":"4\n","error":"__STALE__"'
    exit 0 ;;
  stale-fail)
    result '"status":"ERROR","response":"중간까지 쓴 답","error":"__STALE__"'
    exit 3 ;;
esac
"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("officestra-agy-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["bin", "work", "home/.gemini/antigravity-cli/conversations", "driver/package"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        // macOS의 /var는 /private/var 링크라 실행기의 저장소 비교와 맞추려고 실제 경로를 쓴다.
        let root = std::fs::canonicalize(&root).unwrap();
        std::fs::write(root.join("driver/package/cli.js"), "").unwrap();
        let script = FAKE_AGY.replace("__DIR__", root.to_str().unwrap()).replace("__STALE__", STALE_QUOTA_ERROR);
        let agy = root.join("bin/agy");
        std::fs::write(&agy, script).unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn text(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_string()
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.path(name), "").unwrap();
    }

    async fn wait_file(&self, name: &str) {
        for _ in 0..200 {
            if self.path(name).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("모의 agy가 {name} 파일을 만들지 않았습니다.");
    }

    /// 모의 agy가 받은 인자. NUL로 나눠 남겼다.
    fn arguments(&self, scenario: &str) -> Vec<String> {
        let bytes = std::fs::read(self.path(&format!("{scenario}.args"))).unwrap();
        bytes.split(|byte| *byte == 0).filter(|part| !part.is_empty()).map(|part| String::from_utf8(part.to_vec()).unwrap()).collect()
    }

    fn environment(&self, scenario: &str) -> String {
        std::fs::read_to_string(self.path(&format!("{scenario}.env"))).unwrap()
    }

    /// agy 대화 SQLite. 추론(step_payload)과 사용량(metadata)을 읽는 두 열을 함께 둔다.
    fn conversation(&self, id: &str) -> Connection {
        let database = Connection::open(self.path(&format!("home/.gemini/antigravity-cli/conversations/{id}.db"))).unwrap();
        database
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE IF NOT EXISTS steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, step_payload BLOB, metadata BLOB);",
            )
            .unwrap();
        database
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value > 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            return bytes;
        }
    }
}

fn number_field(field: u64, value: u64) -> Vec<u8> {
    [varint(field << 3), varint(value)].concat()
}

fn bytes_field(field: u64, value: &[u8]) -> Vec<u8> {
    [varint((field << 3) | 2), varint(value.len() as u64), value.to_vec()].concat()
}

/// 응답 단계 payload. 20번 필드 안 3번 필드가 추론이다.
fn reasoning_payload(text: &str) -> Vec<u8> {
    bytes_field(20, &bytes_field(3, text.as_bytes()))
}

/// 응답 단계 metadata. 1번은 시각(초), 9번은 사용량(입력 2·출력 3·캐시 5·추론 9), 12번은 요청 ID다.
fn usage_metadata(execution_id: &str, seconds: u64, input: u64, output: u64, cached: u64, reasoning: u64) -> Vec<u8> {
    let usage = [number_field(2, input), number_field(3, output), number_field(5, cached), number_field(9, reasoning)].concat();
    [bytes_field(1, &number_field(1, seconds)), bytes_field(9, &usage), bytes_field(12, execution_id.as_bytes())].concat()
}

fn app_state(pool: PgPool, runtime: Arc<Runtime>) -> AppState {
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

/// 잡을 보내고 턴 ID를 돌려준다. 프롬프트에 모의 agy 표식을 넣는다.
async fn start(state: &AppState, character_id: &str, scenario: &str, conversation: Option<&str>) -> String {
    let prompt = match conversation {
        Some(conversation) => format!("SCENARIO={scenario} CONV={conversation}"),
        None => format!("SCENARIO={scenario}"),
    };
    let (status, job) =
        call(state, "POST", "/api/agent-jobs", Some(json!({ "characterId": character_id, "prompt": prompt }))).await;
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

fn completed() -> (String, Option<String>) {
    ("completed".to_string(), None)
}

fn failed(message: &str) -> (String, Option<String>) {
    ("failed".to_string(), Some(message.to_string()))
}

type ActivityRow = (String, String, String);

async fn activities(pool: &PgPool, turn_id: &str) -> Vec<ActivityRow> {
    sqlx::query_as("SELECT kind, text, status FROM turn_activities WHERE turn_id = $1::uuid ORDER BY seq")
        .bind(turn_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

fn rows(expected: &[(&str, &str, &str)]) -> Vec<ActivityRow> {
    expected.iter().map(|(kind, text, status)| (kind.to_string(), text.to_string(), status.to_string())).collect()
}

/// 400ms 감시가 활동 표를 기대값으로 바꿀 때까지 기다린다.
async fn wait_for_activities(pool: &PgPool, turn_id: &str, expected: &[(&str, &str, &str)]) {
    let expected = rows(expected);
    let mut current = Vec::new();
    for _ in 0..200 {
        current = activities(pool, turn_id).await;
        if current == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("활동 표가 기대값이 되지 않았습니다.\n현재 {current:?}\n기대 {expected:?}");
}

async fn assistant_text(pool: &PgPool, turn_id: &str) -> String {
    sqlx::query_scalar("SELECT text FROM messages WHERE turn_id = $1::uuid AND role = 'assistant'")
        .bind(turn_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wait_for_assistant_text(pool: &PgPool, turn_id: &str, expected: &str) {
    let mut current = String::new();
    for _ in 0..200 {
        current = assistant_text(pool, turn_id).await;
        if current == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("응답 초안이 {expected:?}가 되지 않았습니다. 현재 {current:?}");
}

async fn usage(pool: &PgPool, turn_id: &str) -> Option<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
    sqlx::query_as(
        "SELECT input_tokens, output_tokens, cached_input_tokens, reasoning_output_tokens FROM usage_records WHERE turn_id = $1::uuid",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// 실행 인자에서 프롬프트 뒤 고정 부분. Node antigravityArguments 순서다.
fn expected_tail(work: &str, conversation: Option<&str>) -> Vec<String> {
    let mut tail: Vec<String> =
        ["--output-format", "stream-json", "--print-timeout", "24h"].iter().map(|text| text.to_string()).collect();
    tail.extend(antigravity_model_arguments(MODEL, "high"));
    if let Some(conversation) = conversation {
        tail.extend(["--conversation".to_string(), conversation.to_string()]);
    }
    tail.extend(["--add-dir".to_string(), work.to_string()]);
    tail.extend(["--mode", "accept-edits", "--sandbox", "--dangerously-skip-permissions"].iter().map(|text| text.to_string()));
    tail
}

struct Context {
    pool: PgPool,
    runtime: Arc<Runtime>,
    state: AppState,
    fixture: Fixture,
}

async fn setup() -> Option<Context> {
    let Ok(url) = std::env::var("OFFICE_TEST_DATABASE_URL") else {
        eprintln!("OFFICE_TEST_DATABASE_URL이 없어 건너뜀");
        return None;
    };
    let Ok(migrations_dir) = std::env::var("OFFICE_MIGRATIONS_DIR") else {
        eprintln!("OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return None;
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
    let agy = json!({ "executablePath": fixture.text("bin/agy") });
    for (id, backend, model, effort, permission, config) in [
        ("agy-steps", "antigravity", MODEL, "high", "accept-edits", agy.clone()),
        ("agy-reason", "antigravity", MODEL, "high", "accept-edits", agy.clone()),
        ("agy-corrupt", "antigravity", MODEL, "high", "accept-edits", agy.clone()),
        ("agy-usage", "antigravity", MODEL, "high", "accept-edits", agy.clone()),
        ("agy-exit", "antigravity", MODEL, "high", "accept-edits", agy.clone()),
        ("agy-terminal", "antigravity", MODEL, "high", "accept-edits", agy),
        ("claude-terminal", "claude", "claude-opus-5", "high", "acceptEdits", json!({})),
    ] {
        sqlx::query(
            "INSERT INTO characters (id, name, seat, backend, model, effort, permission, config)
             VALUES ($1, $1, '상단', $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(backend)
        .bind(model)
        .bind(effort)
        .bind(permission)
        .bind(config)
        .execute(&pool)
        .await
        .unwrap();
    }

    let work = fixture.path("work");
    let runtime = Arc::new(Runtime::new(Some(work.clone()), Some(work)));
    let services = RuntimeServices::new(
        pool.clone(),
        Arc::new(|_event| {}),
        LaunchPaths {
            result_tool_directory: fixture.text("tools"),
            node_executable: Some(FAKE_NODE.into()),
            playwright_driver_directory: fixture.text("driver"),
            runtime_id: "antigravity-runtime-test".into(),
        },
        SessionRoots { home: fixture.text("home") },
    );
    assert!(runtime.attach_services(services));
    let state = app_state(pool.clone(), Arc::clone(&runtime));
    Some(Context { pool, runtime, state, fixture })
}

#[tokio::test]
async fn 모의_agy로_antigravity_실행_흐름을_node처럼_처리한다() {
    let Some(context) = setup().await else {
        return;
    };
    steps_and_playwright(&context).await;
    reasoning_monitor_and_resume(&context).await;
    unreadable_conversation(&context).await;
    interrupted_resume_usage(&context).await;
    exit_codes(&context).await;
    terminal_refresh(&context).await;
}

/// Node "Antigravity 도구 사이의 중간 메시지는 발생한 자리에 message 활동으로 남는다"와 Playwright 환경 전달.
async fn steps_and_playwright(context: &Context) {
    let Context { pool, state, fixture, .. } = context;
    let turn = start(state, "agy-steps", "steps", Some(STEPS_CONVERSATION)).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-steps").await, completed());
    // 끝 줄바꿈을 뗀 문장이 발생한 자리에 남고, 마지막 메시지는 활동으로 올리지 않는다. 사용량만 있는 빈 단계는 앞 문장을 다시 쓰지 않는다.
    assert_eq!(
        activities(pool, &turn).await,
        rows(&[
            ("message", "첫 번째 메시지입니다.", "completed"),
            ("command", "echo hi", "completed"),
            ("message", "두 번째 메시지입니다.", "completed"),
            ("command", "echo bye", "completed"),
        ])
    );
    // 결과 이벤트의 이어 붙인 전문은 쓰지 않고, 단계 메시지를 조각별로 잇는다(Node completedResponseText).
    assert_eq!(assistant_text(pool, &turn).await, "첫 번째 메시지입니다.\n\n두 번째 메시지입니다.\n\n끝났습니다.");
    let (input, output, _, _) = usage(pool, &turn).await.expect("사용량 기록");
    assert_eq!((input, output), (Some(10), Some(1)));

    let arguments = fixture.arguments("steps");
    assert_eq!(arguments[0], "-p");
    let request = format!("<user_request>\nSCENARIO=steps CONV={STEPS_CONVERSATION}\n</user_request>");
    assert!(arguments[1].ends_with(&request), "{}", arguments[1]);
    assert_eq!(&arguments[2..], expected_tail(&fixture.text("work"), None).as_slice());
    // 드라이버 폴더에 package/cli.js가 있으면 드라이버와 Node 경로를 자식 환경에 넣는다.
    assert_eq!(fixture.environment("steps"), format!("{}\n{FAKE_NODE}\n", fixture.text("driver")));
}

/// Node 추론 모니터 시험(자라는 추론, 끝나면 완료 확정, 뒤 단계 추론은 답변을 먼저 올림, 같은 단계 추론은 올리지 않음)과 재개 턴.
async fn reasoning_monitor_and_resume(context: &Context) {
    let Context { pool, state, fixture, .. } = context;
    let database = fixture.conversation(REASON_CONVERSATION);
    let insert = "INSERT INTO steps (idx, step_type, status, step_payload) VALUES (?, 15, ?, ?)";
    let turn = start(state, "agy-reason", "reason", Some(REASON_CONVERSATION)).await;
    fixture.wait_file("reason.ready").await;
    // 단계 3의 답변은 다음 활동이 올 때까지 대기 중이다.
    wait_for_assistant_text(pool, &turn, "중간 메시지입니다.").await;
    assert_eq!(activities(pool, &turn).await, rows(&[]));

    let middle = ("message", "중간 메시지입니다.", "completed");
    database.execute(insert, (5, 8, reasoning_payload("상자 수를 센다."))).unwrap();
    wait_for_activities(pool, &turn, &[middle, ("thinking", "상자 수를 센다.", "running")]).await;
    database.execute("UPDATE steps SET step_payload = ? WHERE idx = 5", [reasoning_payload("상자 수를 센다. 검산한다.")]).unwrap();
    wait_for_activities(pool, &turn, &[middle, ("thinking", "상자 수를 센다. 검산한다.", "running")]).await;
    database.execute("UPDATE steps SET status = 3 WHERE idx = 5", []).unwrap();
    let counted = ("thinking", "상자 수를 센다. 검산한다.", "completed");
    wait_for_activities(pool, &turn, &[middle, counted]).await;
    database.execute(insert, (7, 8, reasoning_payload("끝내 완료 표시가 오지 않았다."))).unwrap();
    wait_for_activities(pool, &turn, &[middle, counted, ("thinking", "끝내 완료 표시가 오지 않았다.", "running")]).await;

    // 같은 단계(9)의 답변보다 늦게 읽힌 추론은 답변을 활동으로 먼저 올리지 않는다.
    fixture.touch("reason.answer");
    fixture.wait_file("reason.answered").await;
    wait_for_assistant_text(pool, &turn, "중간 메시지입니다.\n\n최종 답변입니다.").await;
    database.execute(insert, (9, 3, reasoning_payload("단계가 끝날 때 기록된 추론"))).unwrap();
    let late = ("thinking", "단계가 끝날 때 기록된 추론", "completed");
    wait_for_activities(pool, &turn, &[middle, counted, ("thinking", "끝내 완료 표시가 오지 않았다.", "running"), late]).await;

    fixture.touch("reason.finish");
    assert_eq!(wait_for_turn(state, &turn, "agy-reason").await, completed());
    // 끝내 완료 표시가 오지 않은 추론도 턴이 끝나면 완료로 확정한다.
    assert_eq!(
        activities(pool, &turn).await,
        rows(&[middle, counted, ("thinking", "끝내 완료 표시가 오지 않았다.", "completed"), late])
    );
    assert_eq!(assistant_text(pool, &turn).await, "중간 메시지입니다.\n\n최종 답변입니다.");

    // 재개 턴은 같은 대화를 --conversation으로 잇고, 시작 전 마지막 단계(9)까지의 추론을 다시 올리지 않는다.
    let resumed = start(state, "agy-reason", "resume", Some(REASON_CONVERSATION)).await;
    assert_eq!(wait_for_turn(state, &resumed, "agy-reason").await, completed());
    assert_eq!(activities(pool, &resumed).await, rows(&[]));
    assert_eq!(assistant_text(pool, &resumed).await, "재개 답변입니다.");
    let arguments = fixture.arguments("resume");
    assert_eq!(&arguments[2..], expected_tail(&fixture.text("work"), Some(REASON_CONVERSATION)).as_slice());
}

/// Node "추론 읽기가 실패해도 턴 종료를 막지 않는다". 대화 파일이 SQLite가 아니어도 감시·사용량 읽기가 턴을 막지 않는다.
async fn unreadable_conversation(context: &Context) {
    let Context { pool, state, fixture, .. } = context;
    let path = fixture.path(&format!("home/.gemini/antigravity-cli/conversations/{CORRUPT_CONVERSATION}.db"));
    std::fs::write(path, b"not a sqlite database").unwrap();
    let turn = start(state, "agy-corrupt", "corrupt", Some(CORRUPT_CONVERSATION)).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-corrupt").await, completed());
    assert_eq!(assistant_text(pool, &turn).await, "손상된 기록에도 끝납니다.");
    assert_eq!(activities(pool, &turn).await, rows(&[]));
}

/// Node "중단된 Antigravity 재개 턴은 로컬 누적값에서 현재 턴만 복구한다".
async fn interrupted_resume_usage(context: &Context) {
    let Context { pool, state, fixture, .. } = context;
    let database = fixture.conversation(USAGE_CONVERSATION);
    let insert = "INSERT INTO steps (idx, step_type, status, metadata) VALUES (?, 15, 3, ?)";
    database.execute(insert, (1, usage_metadata("execution-1", 1_000, 1_000, 100, 5_000, 50))).unwrap();
    let first = start(state, "agy-usage", "usage-first", Some(USAGE_CONVERSATION)).await;
    assert_eq!(wait_for_turn(state, &first, "agy-usage").await, completed());

    let resumed = start(state, "agy-usage", "usage-wait", Some(USAGE_CONVERSATION)).await;
    fixture.wait_file("usage-wait.ready").await;
    // 재개 턴이 도는 동안 agy가 남긴 누적값은 1,200·130·5,900·62가 된다. stdout에는 사용량이 오지 않았다.
    database.execute(insert, (2, usage_metadata("execution-2", 2_000, 200, 30, 900, 12))).unwrap();
    let (status, body) = call(state, "DELETE", "/api/agent-jobs/agy-usage", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "turnId": resumed, "status": "interrupted" })));
    // 끊긴 프로세스의 뒤처리가 중단 기록을 덮지 않는다.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        wait_for_turn(state, &resumed, "agy-usage").await,
        ("interrupted".to_string(), Some("사용자가 업무를 중단했습니다.".to_string()))
    );
    // 이전 턴까지의 누적값(1,000·100·5,000·50)을 빼 현재 턴만 남긴다.
    assert_eq!(usage(pool, &resumed).await, Some((Some(200), Some(30), Some(900), Some(12))));
}

/// Node executeSingleProcess의 Antigravity 종료 코드 시험 넷.
async fn exit_codes(context: &Context) {
    let Context { pool, state, .. } = context;
    // 응답이 정상 확보되면 종료 코드가 0이 아니어도 완료한다.
    let turn = start(state, "agy-exit", "exit-success", None).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-exit").await, completed());
    assert_eq!(assistant_text(pool, &turn).await, "성공한 응답입니다");
    // 응답 없이 비정상 종료하면 앞선 경고보다 표준 오류가 실패 사유다.
    let turn = start(state, "agy-exit", "stderr", None).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-exit").await, failed("진짜 프로세스 에러"));
    // 이전 한도 오류를 ERROR로 보내도 정상 종료하고 답이 있으면 완료한다.
    let turn = start(state, "agy-exit", "stale-ok", None).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-exit").await, completed());
    assert_eq!(assistant_text(pool, &turn).await, "4");
    // ERROR와 함께 비정상 종료하면 답이 있어도 실패한다.
    let turn = start(state, "agy-exit", "stale-fail", None).await;
    assert_eq!(wait_for_turn(state, &turn, "agy-exit").await, failed(STALE_QUOTA_ERROR));
}

async fn begin_terminal(runtime: &Runtime, character_id: &str) -> String {
    let launch = runtime.prepare_terminal_launch(character_id).await.unwrap();
    runtime
        .begin_terminal_turn(TerminalTurnStart {
            character_id: character_id.into(),
            session_id: launch.session_id,
            prompt: "질문".into(),
            started_at: None,
            execution: launch.character,
            sender_character_id: None,
            request_source: None,
            delivery_id: None,
        })
        .await
        .unwrap()
}

fn completion(character_id: &str, turn_id: &str, response: &str, refresh_completed: bool) -> TerminalTurnCompletion {
    TerminalTurnCompletion {
        character_id: character_id.into(),
        turn_id: turn_id.into(),
        response: response.into(),
        refresh_completed,
        ..Default::default()
    }
}

/// Node "완료 턴 후속 갱신은 Antigravity 터미널에만 허용한다"와 Antigravity 쪽 후속 갱신.
async fn terminal_refresh(context: &Context) {
    let Context { pool, runtime, .. } = context;
    let claude = begin_terminal(runtime, "claude-terminal").await;
    let error = runtime.complete_terminal_turn(completion("claude-terminal", &claude, "후속 응답", true)).await.unwrap_err();
    assert_eq!(error.to_string(), "Antigravity 터미널 후속 응답만 갱신할 수 있습니다.");
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1::uuid")
        .bind(&claude)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(status, "running", "거부된 갱신은 실행 중 턴을 건드리지 않는다");
    // 거부 뒤에도 같은 턴은 일반 완료로 끝난다. 뒤 시험 파일이 진행 중 턴을 보지 않게 정리도 겸한다.
    runtime.complete_terminal_turn(completion("claude-terminal", &claude, "클로드 응답", false)).await.unwrap();
    assert_eq!(assistant_text(pool, &claude).await, "클로드 응답");

    let agy = begin_terminal(runtime, "agy-terminal").await;
    runtime.complete_terminal_turn(completion("agy-terminal", &agy, "첫 응답", false)).await.unwrap();
    assert_eq!(assistant_text(pool, &agy).await, "첫 응답");
    // 완료된 Antigravity 터미널 턴은 같은 질문의 후속 응답으로 다시 저장하고, 갱신 표시 없이는 찾지 않는다.
    runtime.complete_terminal_turn(completion("agy-terminal", &agy, "후속 응답", true)).await.unwrap();
    assert_eq!(assistant_text(pool, &agy).await, "후속 응답");
    let error = runtime.complete_terminal_turn(completion("agy-terminal", &agy, "또 다른 응답", false)).await.unwrap_err();
    assert_eq!(error.to_string(), "완료할 터미널 턴을 찾을 수 없습니다.");
}
