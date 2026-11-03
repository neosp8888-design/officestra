// 모델 카탈로그의 PostgreSQL 저장소와 /api/model-catalog 경로 시험. DB가 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::model_catalog::*;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use tower::ServiceExt;

static READY: OnceCell<()> = OnceCell::const_new();
/// 두 DB 시험이 같은 agent_model_catalogs 줄을 쓰므로 차례로 돌린다.
static TABLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

async fn clear(pool: &PgPool) {
    sqlx::query("DELETE FROM agent_model_catalogs").execute(pool).await.unwrap();
}

fn claim(provider: &str, attempted_at: f64, force: bool) -> ModelCatalogClaim<'_> {
    ModelCatalogClaim { provider, attempted_at, minimum_interval_milliseconds: 1_000.0, force }
}

fn one_model(id: &str) -> ModelCatalog {
    let model = normalized_model(&json!({ "id": id, "title": "시험 모델", "efforts": ["low", "high"] })).unwrap();
    ModelCatalog::new(vec![model])
}

const T0: f64 = 1_700_000_000_123.0;

#[tokio::test]
async fn postgres_store_claims_saves_and_reads_rows() {
    let Some(pool) = test_pool().await else { return };
    let _table = TABLE.lock().await;
    clear(&pool).await;
    let store = PostgresModelCatalogStore::new(pool.clone());

    let first = store.claim_refresh(claim("codex", T0, false)).await.unwrap().unwrap();
    assert_eq!(first.provider, "codex");
    assert_eq!(first.catalog, json!({ "models": [], "version": 1 }));
    assert!(first.excluded_models.is_empty());
    assert_eq!((first.fetched_at, first.last_attempted_at, first.last_error), (None, Some(T0), None));
    // 최소 간격 안에서는 잡지 못하고, 간격이 딱 차거나 force면 잡는다.
    assert!(store.claim_refresh(claim("codex", T0 + 999.0, false)).await.unwrap().is_none());
    assert_eq!(store.claim_refresh(claim("codex", T0 + 1_000.0, false)).await.unwrap().unwrap().last_attempted_at, Some(T0 + 1_000.0));
    assert_eq!(store.claim_refresh(claim("codex", T0 + 1_001.0, true)).await.unwrap().unwrap().last_attempted_at, Some(T0 + 1_001.0));

    // Date는 밀리초 아래를 버린다.
    let catalog = one_model("gpt-test");
    let saved = store.save_success("codex", &catalog, T0 + 2_000.7).await.unwrap().unwrap();
    assert_eq!(saved.catalog, catalog.to_json());
    assert_eq!((saved.fetched_at, saved.last_error.as_deref()), (Some(T0 + 2_000.0), None));

    let failed = store.save_failure("codex", &"가".repeat(1_500)).await.unwrap().unwrap();
    assert_eq!(failed.last_error.as_deref().map(|text| text.chars().count()), Some(1_000));
    assert_eq!((failed.catalog, failed.fetched_at), (catalog.to_json(), Some(T0 + 2_000.0)));

    // 줄이 없으면 대체 목록으로 새로 만들고, 있으면 제외 목록만 바꾼다.
    let fallback = builtin_model_catalog("antigravity").unwrap();
    let created = store.save_exclusions("antigravity", &["x".into(), "y".into()], &fallback).await.unwrap().unwrap();
    assert_eq!(created.catalog, fallback.to_json());
    assert_eq!(created.excluded_models, [Some("x".to_string()), Some("y".to_string())]);
    assert_eq!(created.last_attempted_at, None);
    let kept = store.save_exclusions("codex", &["gpt-test".into()], &fallback).await.unwrap().unwrap();
    assert_eq!((kept.catalog, kept.excluded_models), (catalog.to_json(), vec![Some("gpt-test".to_string())]));

    assert!(store.save_success("claude", &catalog, T0).await.unwrap().is_none());
    assert!(store.save_failure("claude", "오류").await.unwrap().is_none());
    let mut providers: Vec<String> = store.load().await.unwrap().into_iter().map(|row| row.provider).collect();
    providers.sort();
    assert_eq!(providers, ["antigravity", "codex"]);
    clear(&pool).await;
}

/// 제공자 이름을 실행 파일로 쓰고, 정해 둔 출력만 돌려준다. 없는 제공자는 실행 실패다.
#[derive(Default)]
struct FakeHost {
    outputs: HashMap<&'static str, String>,
    calls: Mutex<Vec<String>>,
}

impl ModelCatalogHost for FakeHost {
    fn resolve_executable<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { Ok(provider.to_string()) })
    }

    fn run_command<'a>(
        &'a self,
        executable: &'a str,
        _arguments: &'a [&'a str],
        _input: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push(executable.to_string());
        let output = self.outputs.get(executable).cloned().ok_or_else(|| format!("spawn {executable} ENOENT"));
        Box::pin(async move { output })
    }

    fn executable_changed_at<'a>(&'a self, _executable: &'a str) -> BoxFuture<'a, Option<f64>> {
        Box::pin(async { None })
    }
}

fn state(pool: PgPool, model_catalog: Option<Arc<ModelCatalogService>>) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog,
        cli_updates: Arc::default(),
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

fn put(body: &str) -> Request<Body> {
    Request::put("/api/model-catalog/exclusions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn provider<'a>(snapshot: &'a Value, backend: &str) -> &'a Value {
    snapshot["providers"].as_array().unwrap().iter().find(|entry| entry["backend"] == backend).unwrap()
}

#[tokio::test]
async fn model_catalog_routes_follow_node() {
    let Some(pool) = test_pool().await else { return };
    let _table = TABLE.lock().await;
    clear(&pool).await;
    let codex = json!({ "models": [{
        "slug": "gpt-test", "display_name": "GPT Test", "visibility": "list",
        "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }],
        "default_reasoning_level": "high", "context_window": 272_000,
    }, {
        "slug": "gpt-other", "display_name": "GPT Other", "visibility": "list",
        "supported_reasoning_levels": [{ "effort": "medium" }],
    }] });
    let host = Arc::new(FakeHost { outputs: HashMap::from([("codex", codex.to_string())]), ..FakeHost::default() });
    let options = ModelCatalogOptions { now: Arc::new(|| T0), warn: Arc::new(|_, _| {}), ..ModelCatalogOptions::default() };
    let service = ModelCatalogService::new(Arc::new(PostgresModelCatalogStore::new(pool.clone())), host.clone(), options);
    service.load_cached().await.unwrap();
    let state = state(pool.clone(), Some(service));

    let (status, snapshot) = call(&state, get("/api/model-catalog")).await;
    assert_eq!(status, StatusCode::OK);
    let backends: Vec<&Value> = snapshot["providers"].as_array().unwrap().iter().map(|entry| &entry["backend"]).collect();
    assert_eq!(backends, ["codex", "antigravity", "claude"]);
    assert_eq!(provider(&snapshot, "codex")["fetchedAt"], Value::Null);
    assert!(host.calls.lock().unwrap().is_empty());

    // force=1은 세 제공자를 모두 다시 받는다. 실패한 제공자는 오류만 남기고 응답은 200이다.
    let (status, snapshot) = call(&state, get("/api/model-catalog?force=1")).await;
    assert_eq!(status, StatusCode::OK);
    let mut calls = host.calls.lock().unwrap().clone();
    calls.sort();
    assert_eq!(calls, ["antigravity", "claude", "codex"]);
    let codex = provider(&snapshot, "codex");
    let ids: Vec<&Value> = codex["models"].as_array().unwrap().iter().map(|model| &model["id"]).collect();
    assert_eq!(ids, ["gpt-test", "gpt-other"]);
    assert_eq!(codex["models"][0]["defaultEffort"], "high");
    assert_eq!(codex["models"][0]["contextWindow"], 272_000);
    assert_eq!(codex["fetchedAt"], "2023-11-14T22:13:20.123Z");
    assert_eq!(codex["lastError"], Value::Null);
    assert_eq!(provider(&snapshot, "claude")["lastError"], "spawn claude ENOENT");
    assert_eq!(provider(&snapshot, "claude")["lastAttemptedAt"], "2023-11-14T22:13:20.123Z");

    let (status, snapshot) = call(&state, put(r#"{"provider":"codex","excludedModels":["gpt-other","gpt-other","missing-model",null,""]}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(provider(&snapshot, "codex")["excludedModels"], json!(["gpt-other"]));
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT unnest(excluded_models) FROM agent_model_catalogs WHERE provider = 'codex'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(stored, ["gpt-other"]);

    for (body, status, message) in [
        (r#"{"provider":"nope","excludedModels":[]}"#, StatusCode::BAD_REQUEST, "지원하지 않는 모델 제공자입니다."),
        (r#"{"excludedModels":[]}"#, StatusCode::BAD_REQUEST, "지원하지 않는 모델 제공자입니다."),
        (r#"{"provider":"codex","excludedModels":"x"}"#, StatusCode::BAD_REQUEST, "제외 모델은 배열이어야 합니다."),
        (r#"{"provider":"codex","excludedModels":["a b"]}"#, StatusCode::BAD_REQUEST, "제외 모델 식별자가 올바르지 않습니다."),
        (
            r#"{"provider":"codex","excludedModels":["gpt-test","gpt-other"]}"#,
            StatusCode::BAD_REQUEST,
            "각 CLI에는 표시할 모델을 하나 이상 남겨야 합니다.",
        ),
        ("null", StatusCode::INTERNAL_SERVER_ERROR, "Cannot read properties of null (reading 'provider')"),
        ("5", StatusCode::BAD_REQUEST, "지원하지 않는 모델 제공자입니다."),
    ] {
        assert_eq!(call(&state, put(body)).await, (status, json!({ "error": message })), "{body}");
    }
    clear(&pool).await;
}

#[tokio::test]
async fn model_catalog_routes_wait_for_the_service() {
    let pool = db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap();
    let state = state(pool, None);
    let unavailable = json!({ "error": "모델 목록 수집기가 준비되지 않았습니다." });
    assert_eq!(call(&state, get("/api/model-catalog")).await, (StatusCode::SERVICE_UNAVAILABLE, unavailable.clone()));
    assert_eq!(call(&state, put(r#"{"provider":"codex"}"#)).await, (StatusCode::SERVICE_UNAVAILABLE, unavailable));
    // 형식 검사는 수집기 준비 여부보다 먼저다.
    let plain = Request::put("/api/model-catalog/exclusions").body(Body::from("{}")).unwrap();
    assert_eq!(
        call(&state, plain).await,
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": "상태 변경 요청은 application/json 형식이어야 합니다." }))
    );
}
