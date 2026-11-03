// /api/cli-updates와 /api/cli-updates/apply 경로 시험. 실제 CLI·npm은 부르지 않고 가짜 실행기만 쓴다.
// 진행 중 업무 확인이 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다. 기대값은 Node 24에서 같은 가짜 출력으로 얻었다.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use officestra_backend::child_process::ExecFileOutput;
use officestra_backend::cli_updates::{CliCommandRunner, CliUpdateChecker};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
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

/// claude·codex는 같은 npm 위치에, agy는 따로 설치된 것처럼 답한다. claude 설치가 성공하면 버전이 오른다.
#[derive(Default)]
struct FakeRunner {
    claude_installed: AtomicBool,
    fail_install: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl FakeRunner {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn installs(&self) -> Vec<String> {
        self.calls().into_iter().filter(|call| call.starts_with("npm install") || call == "agy update").collect()
    }

    fn reply(&self, command: &str, arguments: &[&str]) -> Result<ExecFileOutput, String> {
        let out = |stdout: &str, stderr: &str| Ok(ExecFileOutput { stdout: stdout.into(), stderr: stderr.into() });
        match (command, arguments) {
            ("which", ["agy"]) => out("/opt/agy/bin/agy\n", ""),
            ("which", [name]) => out(&format!("/opt/node/bin/{name}\n"), ""),
            ("readlink", ["-f", "/opt/node/bin/claude"]) => out("/opt/node/lib/node_modules/@anthropic-ai/claude-code/cli.js\n", ""),
            ("readlink", ["-f", "/opt/node/bin/codex"]) => out("/opt/node/lib/node_modules/@openai/codex/bin/codex.js\n", ""),
            ("readlink", ["-f", path]) => out(&format!("{path}\n"), ""),
            ("claude", ["--version"]) if self.claude_installed.load(Ordering::SeqCst) => out("2.1.233 (Claude Code)\n", ""),
            ("claude", ["--version"]) => out("2.1.220 (Claude Code)\n", ""),
            ("codex", ["--version"]) => out("codex-cli 0.148.0\n", ""),
            ("agy", ["--version"]) => out("agy 1.4.0\n", ""),
            ("agy", ["changelog"]) => out("1.5.0 (2026-10-30)\n- 고침\n", ""),
            ("npm", ["view", "@anthropic-ai/claude-code", "version"]) => out("2.1.233\n", ""),
            ("npm", ["view", "@openai/codex", "version"]) => out("0.148.0\n", ""),
            ("npm", ["install", ..]) if self.fail_install.load(Ordering::SeqCst) => {
                Err(format!("Command failed: npm {}\nnpm error code E404", arguments.join(" ")))
            }
            ("npm", ["install", ..]) => {
                if arguments.contains(&"@anthropic-ai/claude-code") {
                    self.claude_installed.store(true, Ordering::SeqCst);
                }
                out("\nchanged 1 package in 2s\n", "")
            }
            ("agy", ["update"]) => out("", "updated agy\n"),
            _ => Err(format!("spawn {command} ENOENT")),
        }
    }
}

impl CliCommandRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        command: &'a str,
        arguments: &'a [&'a str],
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<ExecFileOutput, String>> {
        let call: Vec<&str> = std::iter::once(command).chain(arguments.iter().copied()).collect();
        self.calls.lock().unwrap().push(call.join(" "));
        let reply = self.reply(command, arguments);
        Box::pin(async move { reply })
    }
}

const T0: f64 = 1_700_000_000_123.0;

fn state(pool: PgPool, runner: Arc<FakeRunner>) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::new(CliUpdateChecker::new(runner, Arc::new(|| T0), 1_000.0)),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    }
}

async fn call(state: &AppState, request: Request<Body>) -> (StatusCode, Value) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn apply(body: &str) -> Request<Body> {
    Request::post("/api/cli-updates/apply")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn package<'a>(payload: &'a Value, id: &str) -> &'a Value {
    payload["packages"].as_array().unwrap().iter().find(|entry| entry["id"] == id).unwrap()
}

/// 시험 직원 하나에 실행 중인 turn을 만든다. 돌려준 id로 지운다.
async fn running_turn(pool: &PgPool, backend: &str) -> String {
    let character = format!("rs-cli-update-{backend}");
    sqlx::query("INSERT INTO characters (id, name, seat, backend, permission) VALUES ($1, '갱신 시험', '시험', $2, 'workspace-write')")
        .bind(&character)
        .bind(backend)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query_scalar(
        "WITH conversation AS (
           INSERT INTO conversations (title, workdir) VALUES ('갱신 시험', '/tmp/work') RETURNING id
         ), session AS (
           INSERT INTO cli_sessions (conversation_id, character_id, started_at)
           SELECT conversation.id, $1, now() FROM conversation RETURNING id
         )
         INSERT INTO turns (cli_session_id, prompt, backend, status, started_at, updated_at)
         SELECT id, '갱신 시험', $2, 'running', now(), now() FROM session RETURNING id::text",
    )
    .bind(&character)
    .bind(backend)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn remove_turn(pool: &PgPool, backend: &str) {
    let character = format!("rs-cli-update-{backend}");
    for statement in [
        "DELETE FROM turns WHERE cli_session_id IN (SELECT id FROM cli_sessions WHERE character_id = $1)",
        "DELETE FROM conversations WHERE id IN (SELECT conversation_id FROM cli_sessions WHERE character_id = $1)",
        "DELETE FROM cli_sessions WHERE character_id = $1",
        "DELETE FROM characters WHERE id = $1",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement)).bind(&character).execute(pool).await.unwrap();
    }
}

#[tokio::test]
async fn cli_update_routes_follow_node() {
    let Some(pool) = test_pool().await else { return };
    let busy: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE status IN ('pending', 'running')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(busy, 0, "시험 DB에 진행 중 turn이 남아 있으면 이 시험을 믿을 수 없다");
    let runner = Arc::new(FakeRunner::default());
    let state = state(pool.clone(), runner.clone());

    let (status, payload) = call(&state, get("/api/cli-updates")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload.to_string(),
        r#"{"checkedAt":"2023-11-14T22:13:20.123Z","updateAvailable":true,"checkFailed":false,"packages":[{"id":"claude","label":"Claude Code","packageName":"@anthropic-ai/claude-code","installedVersion":"2.1.220","latestVersion":"2.1.233","installPrefix":"/opt/node","checkFailed":false,"updateAvailable":true},{"id":"codex","label":"Codex","packageName":"@openai/codex","installedVersion":"0.148.0","latestVersion":"0.148.0","installPrefix":"/opt/node","checkFailed":false,"updateAvailable":false},{"id":"antigravity","label":"Antigravity","packageName":"agy","installedVersion":"1.4.0","latestVersion":"1.5.0","installPrefix":null,"checkFailed":false,"updateAvailable":true}]}"#
    );
    // 캐시 안에서는 다시 묻지 않고, force는 1·true를 대소문자 없이 받는다.
    let after_first = runner.calls().len();
    assert_eq!(call(&state, get("/api/cli-updates?force=0")).await.1, payload);
    assert_eq!(runner.calls().len(), after_first);
    call(&state, get("/api/cli-updates?force=TRUE")).await;
    assert_eq!(runner.calls().len(), after_first * 2);

    // 막는 것은 그 CLI를 쓰는 직원의 업무뿐이다. 전체 갱신은 모든 CLI 업무에 막힌다.
    running_turn(&pool, "claude").await;
    let busy = json!({ "error": "진행 중인 업무가 끝난 뒤에 업데이트할 수 있습니다." });
    assert_eq!(call(&state, apply(r#"{"id":"claude"}"#)).await, (StatusCode::CONFLICT, busy.clone()));
    assert_eq!(call(&state, apply("{}")).await, (StatusCode::CONFLICT, busy));
    assert!(runner.installs().is_empty());
    let (status, result) = call(&state, apply(r#"{"id":"codex"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(runner.installs(), ["npm install -g --prefix /opt/node @openai/codex"]);
    let keys: Vec<&String> = result.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["ok", "output", "status"]);
    assert_eq!((&result["ok"], &result["output"]), (&json!(true), &json!("changed 1 package in 2s")));
    remove_turn(&pool, "claude").await;

    // 설치 뒤에는 캐시를 버리고 다시 물어 새 버전을 알려 준다.
    runner.calls.lock().unwrap().clear();
    let (status, result) = call(&state, apply(r#"{"id":"claude"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(runner.installs(), ["npm install -g --prefix /opt/node @anthropic-ai/claude-code"]);
    assert_eq!(package(&result["status"], "claude")["installedVersion"], "2.1.233");
    assert_eq!(package(&result["status"], "claude")["updateAvailable"], false);
    assert_eq!(call(&state, get("/api/cli-updates")).await.1, result["status"]);

    // 자체 갱신 CLI는 npm을 거치지 않고, 표준 출력이 비면 표준 오류를 보여 준다.
    runner.calls.lock().unwrap().clear();
    let (status, result) = call(&state, apply(r#"{"id":"antigravity"}"#)).await;
    assert_eq!((status, &result["output"]), (StatusCode::OK, &json!("updated agy")));
    assert_eq!(runner.installs(), ["agy update"]);

    // 객체가 아닌 본문은 id가 없는 것(undefined)으로 보고 전부 갱신한다.
    runner.calls.lock().unwrap().clear();
    let (status, result) = call(&state, apply("5")).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["output"], "changed 1 package in 2s\nupdated agy");
    assert_eq!(runner.installs(), ["npm install -g --prefix /opt/node @anthropic-ai/claude-code @openai/codex", "agy update"]);

    runner.fail_install.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&state, apply(r#"{"id":"codex"}"#)).await,
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "Command failed: npm install -g --prefix /opt/node @openai/codex\nnpm error code E404" })
        )
    );
}

#[tokio::test]
async fn cli_update_apply_checks_the_identifier_before_the_database() {
    // 업무 확인 전에 끝나는 경우만 본다. DB에 닿으면 연결 실패가 난다.
    let pool = db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap();
    let runner = Arc::new(FakeRunner::default());
    let state = state(pool, runner.clone());

    let plain = Request::post("/api/cli-updates/apply").body(Body::from("{}")).unwrap();
    assert_eq!(
        call(&state, plain).await,
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": "상태 변경 요청은 application/json 형식이어야 합니다." }))
    );
    assert!(runner.calls().is_empty());

    assert_eq!(
        call(&state, apply(r#"{"id":"nope"}"#)).await,
        (StatusCode::BAD_REQUEST, json!({ "error": "알 수 없는 CLI입니다. nope" }))
    );
    // Node도 현재 상태를 먼저 읽고 나서 식별자를 본다.
    assert!(runner.calls().iter().any(|call| call == "claude --version"));
    for (body, message) in [
        (r#"{"id":"Claude"}"#, "알 수 없는 CLI입니다. Claude"),
        (r#"{"id":5}"#, "알 수 없는 CLI입니다. 5"),
        (r#"{"id":false}"#, "알 수 없는 CLI입니다. false"),
        (r#"{"id":["a","b"]}"#, "알 수 없는 CLI입니다. a,b"),
        (r#"{"id":{}}"#, "알 수 없는 CLI입니다. [object Object]"),
    ] {
        assert_eq!(call(&state, apply(body)).await, (StatusCode::BAD_REQUEST, json!({ "error": message })), "{body}");
    }
    assert_eq!(
        call(&state, apply("null")).await,
        (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "Cannot read properties of null (reading 'id')" }))
    );
    assert!(runner.installs().is_empty());
}
