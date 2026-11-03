// 라우터 통합 테스트. DB가 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{configuration, db, migrate};
use serde_json::{Value, json};
use tower::ServiceExt;

fn state_without_db() -> AppState {
    AppState {
        pool: db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap(),
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
    }
}

async fn call(state: AppState, request: Request<Body>) -> (StatusCode, String, Value) {
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, content_type, serde_json::from_slice(&bytes).unwrap())
}

/// Node의 trustedJSONMutation을 통과하는 JSON PUT.
fn trusted_put(path: &str, body: &Value) -> Request<Body> {
    Request::put(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn 모르는_경로는_node와_같은_404_json이다() {
    let request = Request::get("/api/nothing").body(Body::empty()).unwrap();
    let (status, content_type, body) = call(state_without_db(), request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "application/json; charset=utf-8");
    assert_eq!(body, json!({ "error": "경로를 찾을 수 없습니다." }));

    // Node는 메서드가 달라도 404다.
    let request = Request::post("/api/characters").body(Body::empty()).unwrap();
    let (status, _, body) = call(state_without_db(), request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "경로를 찾을 수 없습니다.");
}

#[tokio::test]
async fn 드레인은_형식과_출처를_검사하고_상태를_바꾼다() {
    let state = state_without_db();

    let request = Request::get("/api/maintenance/drain").body(Body::empty()).unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["acceptingJobs"], true);
    assert_eq!(body["draining"], false);

    let request = Request::post("/api/maintenance/drain").body(Body::empty()).unwrap();
    let (status, _, _) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let request = Request::post("/api/maintenance/drain")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "http://evil.example")
        .header(header::HOST, "127.0.0.1:4318")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let request = Request::post("/api/maintenance/drain")
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ORIGIN, "http://127.0.0.1:4318")
        .header(header::HOST, "127.0.0.1:4318")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["draining"], true);
    assert_eq!(body["acceptingJobs"], false);

    let request = Request::delete("/api/maintenance/drain")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["draining"], false);

    let request = Request::put("/api/maintenance/drain").body(Body::empty()).unwrap();
    let (status, _, _) = call(state, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn db가_끊기면_health는_503이다() {
    let request = Request::get("/health").body(Body::empty()).unwrap();
    let (status, _, body) = call(state_without_db(), request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ok"], true);
    assert_eq!(body["database"]["ok"], false);
}

#[tokio::test]
async fn 설정_경로는_db_전에_형식과_검증_오류를_낸다() {
    let state = state_without_db();

    // bulk와 cli-paths는 신뢰된 JSON 요청만 받는다.
    for path in ["/api/characters/settings/bulk", "/api/runtime/cli-paths"] {
        let request = Request::put(path).body(Body::from("{}")).unwrap();
        let (status, _, body) = call(state.clone(), request).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{path}");
        assert_eq!(body["error"], "상태 변경 요청은 application/json 형식이어야 합니다.");
    }

    // 빈 본문은 빈 객체로 읽어 검증 오류가 된다.
    let request = Request::put("/api/characters/settings/bulk")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "updates 배열을 입력하세요.");

    let request = trusted_put("/api/characters/settings/bulk", &json!({ "updates": [{ "characterId": "boss", "backend": "nope" }] }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "지원하지 않는 CLI입니다.");

    let request = trusted_put("/api/runtime/cli-paths", &json!({ "executables": { "codex": "relative/codex" } }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "codex 실행 경로는 절대경로여야 합니다.");

    // 빈 executables는 DB 없이도 성공한다.
    let request = trusted_put("/api/runtime/cli-paths", &json!({ "executables": {} }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true, "updatedCharacterIds": [] }));

    // 개별 설정 PUT은 출처 검사가 없고, 본문이 객체가 아니어도 경로의 직원 ID로 검증한다.
    let request = Request::put("/api/characters/boss/settings").body(Body::from("[]")).unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "지원하지 않는 CLI입니다.");

    // 드레인 중이면 bulk는 409다.
    state.runtime.begin_drain();
    let request = trusted_put("/api/characters/settings/bulk", &json!({ "updates": [codex_settings("boss", "high", false)] }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "백엔드가 이미 다른 안전 전환을 준비 중입니다.");
    assert!(state.runtime.is_draining(), "남의 드레인은 풀지 않는다");
}

fn codex_settings(character_id: &str, effort: &str, fast_mode: bool) -> Value {
    json!({
        "characterId": character_id, "backend": "codex", "model": "gpt-5.6-sol",
        "effort": effort, "fastMode": fast_mode, "permission": "workspace-write"
    })
}

fn character_json(id: &str, name: &str, backend: &str, model: &str, permission: &str, executable_path: Value) -> Value {
    json!({
        "id": id, "name": name, "seat": "상단", "backend": backend, "identityPrompt": format!("{id} 역할"),
        "model": model, "effort": "high", "fastMode": false, "permission": permission,
        "executablePath": executable_path, "hitbox": { "x": 1 }, "bubble": { "side": "left" }
    })
}

struct ExecutableFixture {
    directory: PathBuf,
}

impl ExecutableFixture {
    fn new(label: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("office-rs-cli-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        Self { directory }
    }
    fn executable(&self, name: &str) -> String {
        let path = self.directory.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }
}

impl Drop for ExecutableFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).ok();
    }
}

fn board_request(method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn 업무_보드는_db_전에_node와_같은_순서로_거부한다() {
    let cases = [
        // 조회 경로가 아니면 GET도 형식 검사부터 받는다.
        (Request::get("/api/work-board/unknown").body(Body::empty()).unwrap(), 415,
            "상태 변경 요청은 application/json 형식이어야 합니다."),
        (Request::post("/api/work-board/projects")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap(), 403, "신뢰할 수 없는 요청 출처입니다."),
        (board_request("PUT", "/api/work-board", "{}"), 405, "지원하지 않는 업무 보드 요청입니다."),
        (board_request("PATCH", "/api/work-board/", "{}"), 405, "지원하지 않는 업무 보드 요청입니다."),
        (board_request("POST", "/api/work-board/tickets/a/b", "{}"), 405, "지원하지 않는 업무 보드 요청입니다."),
        (board_request("POST", "/api/work-board/projects", r#"{"projectKey":"TOSS","title":"x"}"#), 400,
            "projectKey는 소문자·숫자·하이픈 2~64자여야 합니다."),
        (board_request("PATCH", "/api/work-board/projects/not-a-uuid", r#"{"title":"x"}"#), 400,
            "projectId는 UUID여야 합니다."),
        (Request::get("/api/work-board/records/not-a-uuid").body(Body::empty()).unwrap(), 400,
            "workRecordId는 UUID여야 합니다."),
        (Request::get("/api/work-board/tickets/%2B/activity").body(Body::empty()).unwrap(), 400,
            "ticketId는 UUID여야 합니다."),
        // decodeURIComponent 실패는 Node의 바깥 처리기가 500으로 낸다.
        (Request::get("/api/work-board/tickets/%ZZ/activity").body(Body::empty()).unwrap(), 500, "URI malformed"),
        (Request::get("/api/work-board/records/%E0%A4").body(Body::empty()).unwrap(), 500, "URI malformed"),
        (Request::get("/api/work-board/records/%+1").body(Body::empty()).unwrap(), 500, "URI malformed"),
    ];
    for (request, status, message) in cases {
        let path = request.uri().to_string();
        let (actual, content_type, body) = call(state_without_db(), request).await;
        assert_eq!((actual.as_u16(), body), (status, json!({ "error": message })), "{path}");
        assert_eq!(content_type, "application/json; charset=utf-8");
    }
    // 본문 해석 실패는 형식 검사 뒤 500이다.
    let (status, _, body) = call(state_without_db(), board_request("POST", "/api/work-board/projects", "{")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn 실제_db로_health와_마이그레이션과_직원_설정_경로를_확인한다() {
    let Ok(url) = std::env::var("OFFICE_TEST_DATABASE_URL") else {
        eprintln!("OFFICE_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Ok(migrations_dir) = std::env::var("OFFICE_MIGRATIONS_DIR") else {
        eprintln!("OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return;
    };
    let pool = db::connect(&url).await.unwrap();
    // 테스트 DB의 public 스키마를 비우고 확장만 다시 만든 뒤 전체 마이그레이션을 적용한다.
    sqlx::raw_sql(
        "DROP SCHEMA public CASCADE; CREATE SCHEMA public;
         CREATE EXTENSION IF NOT EXISTS vector; CREATE EXTENSION IF NOT EXISTS pgcrypto;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let directory = std::path::Path::new(&migrations_dir);
    let expected = migrate::migration_files(directory).unwrap();
    let first = migrate::run(&pool, directory).await.unwrap();
    assert_eq!(first.applied, expected);
    assert_eq!(first.skipped, 0);
    let second = migrate::run(&pool, directory).await.unwrap();
    assert!(second.applied.is_empty());
    assert_eq!(second.skipped, expected.len());
    let recorded: Vec<String> =
        sqlx::query_scalar("SELECT name FROM schema_migrations ORDER BY name").fetch_all(&pool).await.unwrap();
    assert_eq!(recorded, expected);

    // 깨진 파일은 기록 없이 실패하고, 앞선 파일의 기록은 남는다.
    let broken = std::env::temp_dir().join(format!("office-rs-broken-{}", std::process::id()));
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("900_ok.sql"), "CREATE TABLE office_rs_probe (id int);").unwrap();
    std::fs::write(broken.join("901_bad.sql"), "CREATE TABLE office_rs_probe (id int); SELECT nope();").unwrap();
    let error = migrate::run(&pool, &broken).await.unwrap_err();
    std::fs::remove_dir_all(&broken).unwrap();
    assert!(error.to_string().contains("901_bad.sql"), "{error}");
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM schema_migrations WHERE name LIKE '90%' ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(names, vec!["900_ok.sql"]);

    let state = AppState {
        pool: pool.clone(),
        runtime: Arc::new(Runtime::new(Some("/Users/neo/office".into()), Some("/Users/neo/office".into()))),
        release_id: Some("test-release".into()),
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    };
    let request = Request::get("/health").body(Body::empty()).unwrap();
    let (status, content_type, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json; charset=utf-8");
    assert_eq!(
        keys(&body),
        ["ok", "service", "apiVersion", "pid", "releaseID", "acceptingJobs", "draining",
         "activeTurnCount", "idle", "workdir", "repositoryRoot", "database"]
    );
    assert_eq!(body["service"], "officestra-backend");
    assert_eq!(body["releaseID"], "test-release");
    assert_eq!(body["database"]["ok"], true);

    // 직원 동기화: 실행 가능한 경로만 config에 남고, 재동기화는 좌석·실행 경로만 바꾼다.
    let fixture = ExecutableFixture::new("sync");
    let codex_path = fixture.executable("codex");
    let configuration = json!({
        "workdir": "/Users/neo/office",
        "characters": [
            character_json("right-man", "서사원", "claude", "claude-sonnet-5", "auto", json!(null)),
            character_json("boss", "백부장", "codex", "gpt-5.6-sol", "workspace-write", json!(&codex_path)),
            character_json("left-woman", "코대리", "codex", "gpt-5.6-sol", "workspace-write", json!("/nonexistent/codex")),
        ]
    });
    let mut connection = pool.acquire().await.unwrap();
    configuration::sync_characters(&mut connection, &configuration).await.unwrap();
    drop(connection);
    let boss_config: Value = sqlx::query_scalar("SELECT config FROM characters WHERE id = 'boss'").fetch_one(&pool).await.unwrap();
    assert_eq!(boss_config["executablePath"], codex_path);
    assert_eq!(boss_config["hitbox"]["x"], 1);
    let left_config: Value = sqlx::query_scalar("SELECT config FROM characters WHERE id = 'left-woman'").fetch_one(&pool).await.unwrap();
    assert!(left_config.get("executablePath").is_none());
    sqlx::query("UPDATE characters SET identity_prompt = '편집된 역할', seat = '좌측' WHERE id = 'boss'").execute(&pool).await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    configuration::sync_characters(&mut connection, &configuration).await.unwrap();
    drop(connection);
    let (identity_prompt, seat): (String, String) =
        sqlx::query_as("SELECT identity_prompt, seat FROM characters WHERE id = 'boss'").fetch_one(&pool).await.unwrap();
    assert_eq!((identity_prompt.as_str(), seat.as_str()), ("편집된 역할", "상단"));

    // GET /api/characters: 좌석 순서와 Node의 키 순서.
    let request = Request::get("/api/characters").body(Body::empty()).unwrap();
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    let characters = body["characters"].as_array().unwrap();
    assert_eq!(characters.iter().map(|c| c["id"].as_str().unwrap()).collect::<Vec<_>>(), ["boss", "left-woman", "right-man"]);
    assert_eq!(
        keys(&characters[0]),
        ["id", "name", "seat", "backend", "model", "effort", "fastMode", "autoCompactPercent", "permission", "identityPrompt"]
    );
    assert_eq!(characters[0]["autoCompactPercent"], 90);
    assert_eq!(characters[0]["identityPrompt"], "편집된 역할");

    // bulk: 대상이 하나라도 없으면 404와 characterIds, 아무것도 바뀌지 않는다.
    let request = trusted_put(
        "/api/characters/settings/bulk",
        &json!({ "updates": [codex_settings("boss", "xhigh", false), codex_settings("ghost", "high", false)] }),
    );
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "캐릭터를 찾을 수 없습니다: ghost", "characterIds": ["ghost"] }));
    let effort: String = sqlx::query_scalar("SELECT effort FROM characters WHERE id = 'boss'").fetch_one(&pool).await.unwrap();
    assert_eq!(effort, "high");
    assert!(!state.runtime.is_draining());

    // bulk: 바뀐 직원은 config 없이, 그대로인 직원은 config까지 돌려준다. CLI가 바뀌면 실행 경로를 뺀다.
    let request = trusted_put(
        "/api/characters/settings/bulk",
        &json!({ "updates": [
            { "characterId": "boss", "backend": "claude", "model": "claude-opus-5", "effort": "max", "fastMode": true, "permission": "acceptEdits" },
            codex_settings("left-woman", "high", false)
        ] }),
    );
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(keys(&body), ["ok", "characters", "warnings"]);
    assert_eq!(body["warnings"], json!([]));
    let updated = &body["characters"][0];
    assert_eq!(keys(updated), ["id", "name", "backend", "model", "effort", "fastMode", "permission", "identityPrompt"]);
    assert_eq!(updated["permission"], "auto");
    assert_eq!(updated["fastMode"], true);
    let unchanged = &body["characters"][1];
    assert_eq!(keys(unchanged), ["id", "name", "backend", "model", "effort", "fastMode", "permission", "config", "identityPrompt"]);
    assert_eq!(unchanged["config"]["hitbox"]["x"], 1);
    let boss_config: Value = sqlx::query_scalar("SELECT config FROM characters WHERE id = 'boss'").fetch_one(&pool).await.unwrap();
    assert!(boss_config.get("executablePath").is_none());
    assert_eq!(boss_config["hitbox"]["x"], 1);
    assert!(!state.runtime.is_draining());

    // 개별 PUT: 없는 직원 404, 그대로면 config 포함 현재 행, 바뀌면 config 없는 행.
    let request = trusted_put("/api/characters/ghost/settings", &codex_settings("ignored", "high", false));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "캐릭터를 찾을 수 없습니다." }));
    let request = trusted_put("/api/characters/left-woman/settings", &codex_settings("ignored", "high", false));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["id", "backend", "model", "effort", "fastMode", "permission", "config"]);
    assert_eq!(body["id"], "left-woman");
    let request = trusted_put("/api/characters/left-woman/settings", &codex_settings("ignored", "ultra", true));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["id", "backend", "model", "effort", "fastMode", "permission"]);
    assert_eq!(body["effort"], "ultra");
    assert_eq!(body["fastMode"], true);

    // 로컬 프로필이 걸린 직원은 409다.
    sqlx::query("UPDATE characters SET config = config || '{\"localProfileId\": \"local-4090\"}' WHERE id = 'right-man'").execute(&pool).await.unwrap();
    let request = trusted_put("/api/characters/right-man/settings", &codex_settings("ignored", "high", false));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "로컬 4090 메뉴에서 클라우드로 돌아간 뒤 설정을 변경하세요.");

    // cli-paths: 제공자별 실행 경로를 병합하고 바뀐 직원만 돌려준다(boss·right-man은 claude, left-woman은 codex). 두 번째 호출은 빈 목록이다.
    let claude_path = fixture.executable("claude");
    let request = trusted_put("/api/runtime/cli-paths", &json!({ "executables": { "claude": &claude_path, "codex": &codex_path } }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true, "updatedCharacterIds": ["boss", "left-woman", "right-man"] }));
    let right_config: Value = sqlx::query_scalar("SELECT config FROM characters WHERE id = 'right-man'").fetch_one(&pool).await.unwrap();
    assert_eq!(right_config["executablePath"], claude_path);
    assert_eq!(right_config["localProfileId"], "local-4090", "다른 config 키는 유지된다");
    let boss_config: Value = sqlx::query_scalar("SELECT config FROM characters WHERE id = 'boss'").fetch_one(&pool).await.unwrap();
    assert_eq!(boss_config["executablePath"], claude_path);
    assert_eq!(boss_config["hitbox"]["x"], 1);
    let request = trusted_put("/api/runtime/cli-paths", &json!({ "executables": { "claude": &claude_path, "codex": &codex_path } }));
    let (status, _, body) = call(state.clone(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["updatedCharacterIds"], json!([]));

    // 잠금은 모두 풀려 있어야 한다.
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'").fetch_one(&pool).await.unwrap();
    assert_eq!(held, 0);
}
