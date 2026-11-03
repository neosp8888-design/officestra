// /api/usage-summary 경로 시험. 실제 공급자 대신 가짜 읽기 함수를 넣고, DB 설정 조회는 OFFICE_TEST_DATABASE_URL이 있을 때만 본다.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::FutureExt;
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::usage_summary::{
    CodexLimits, LimitWindow, ProviderActivity, UsageActivity, UsageProviders, UsageSummaryOptions, UsageSummaryReader,
    configured_antigravity_settings, configured_codex_executable,
};
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

fn dead_pool() -> PgPool {
    db::connect_lazy("postgres://officestra@127.0.0.1:9/unused", Duration::from_millis(100)).unwrap()
}

fn state(reader: Option<Arc<UsageSummaryReader>>) -> AppState {
    AppState {
        pool: dead_pool(),
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: reader,
        roleplay: None,
        voice_follow: Arc::default(),
    }
}

/// Codex만 값을 주는 가짜 요약기. Codex 읽기 횟수를 센다.
fn reader(codex_reads: &Arc<AtomicUsize>, activity_fails: bool) -> Arc<UsageSummaryReader> {
    let reads = Arc::clone(codex_reads);
    let providers = UsageProviders {
        codex: Arc::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            let limits = CodexLimits {
                five_hour: Some(LimitWindow { remaining: Some(75), reset_at: Some("2027-01-15T08:00:00.000Z".into()) }),
                plan: Some("Pro 20x".into()),
                ..CodexLimits::default()
            };
            async move { Ok(Some(limits)) }.boxed()
        }),
        claude: Arc::new(|| async { Err("Claude Code 로그인 정보를 찾을 수 없습니다.".to_string()) }.boxed()),
        antigravity: Arc::new(|| async { Ok(None) }.boxed()),
        activity: Arc::new(move |_| {
            async move {
                if activity_fails {
                    return Err("relation \"usage_records\" does not exist".to_string());
                }
                let codex = ProviderActivity { today_cost_usd: 1.25, recent_tokens: 1234.0, ..ProviderActivity::default() };
                Ok(UsageActivity { codex, ..UsageActivity::default() })
            }
            .boxed()
        }),
    };
    let options = UsageSummaryOptions { now: Arc::new(|| 1_786_838_400_000.0), ..UsageSummaryOptions::default() };
    UsageSummaryReader::new(providers, options)
}

async fn call(state: &AppState, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).map(|value| value.to_str().unwrap().to_string());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, content_type, String::from_utf8(bytes.to_vec()).unwrap())
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn usage_summary_route_follows_node() {
    let codex_reads = Arc::new(AtomicUsize::new(0));
    let state = state(Some(reader(&codex_reads, false)));

    let (status, content_type, body) = call(&state, get("/api/usage-summary")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/json; charset=utf-8"));
    assert!(body.starts_with(r#"{"codexFiveHour":75,"codexFiveHourResetAt":"2027-01-15T08:00:00.000Z","codexWeekly":null,"#));
    assert!(body.contains(r#""claudeLimitError":"Claude Code 로그인 정보를 찾을 수 없습니다.","#));
    assert!(body.contains(r#""fetchedAt":"2026-08-16T00:00:00.000Z","codexActivity":{"todayCostUSD":1.25,"recentTokens":1234,"#));
    let summary: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(summary["codexPlan"], "Pro 20x");
    assert_eq!(summary["antigravityQuotaGroups"], json!([]));
    assert_eq!(codex_reads.load(Ordering::SeqCst), 1);

    // force는 대소문자를 무시한 "1"·"true"만 강제 갱신이다.
    for (query, reads) in [("?force=yes", 1), ("?force=0", 1), ("?force=TRUE", 2), ("?force=1", 3), ("?force=True&force=0", 4)] {
        let (status, _, _) = call(&state, get(&format!("/api/usage-summary{query}"))).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(codex_reads.load(Ordering::SeqCst), reads, "{query}");
    }

    // Node는 GET만 연결하고 다른 메서드는 404다.
    let post = Request::post("/api/usage-summary").body(Body::empty()).unwrap();
    assert_eq!(call(&state, post).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn usage_summary_route_reports_failures() {
    let (status, _, body) = call(&state(None), get("/api/usage-summary")).await;
    assert_eq!((status, body.as_str()), (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"사용량 요약기가 준비되지 않았습니다."}"#));

    let failing = state(Some(reader(&Arc::default(), true)));
    let (status, content_type, body) = call(&failing, get("/api/usage-summary")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some("application/json; charset=utf-8"));
    assert_eq!(body, r#"{"error":"relation \"usage_records\" does not exist"}"#);
}

#[tokio::test]
async fn configured_executables_follow_the_latest_character() {
    let Some(pool) = test_pool().await else { return };
    let ids = ["usage-summary-test-codex", "usage-summary-test-codex-blank", "usage-summary-test-agy"];
    let cleanup = || async {
        sqlx::query("DELETE FROM characters WHERE id = ANY($1)").bind(&ids[..]).execute(&pool).await.unwrap();
    };
    cleanup().await;
    let checked = async {
        let insert = "INSERT INTO characters (id, name, seat, backend, config, model, permission, updated_at)
                      VALUES ($1, '시험', 'test', $2, $3, $4, 'default', now() + $5::interval)";
        // 실행 경로가 빈 Codex 직원은 더 최근이어도 고르지 않는다.
        for (id, backend, config, model, ahead) in [
            (ids[0], "codex", json!({"executablePath": "  /opt/test/bin/codex "}), None, "1 day"),
            (ids[1], "codex", json!({"executablePath": ""}), None, "2 days"),
            (ids[2], "antigravity", json!({}), Some(" gemini-4-argon "), "1 day"),
        ] {
            sqlx::query(insert)
                .bind(id)
                .bind(backend)
                .bind(config)
                .bind(model)
                .bind(ahead)
                .execute(&pool)
                .await
                .map_err(|error| error.to_string())?;
        }
        let codex = configured_codex_executable(&pool).await?;
        let antigravity = configured_antigravity_settings(&pool).await?;
        Ok::<_, String>((codex, antigravity))
    }
    .await;
    cleanup().await;
    let (codex, antigravity) = checked.unwrap();
    assert_eq!(codex, "/opt/test/bin/codex");
    assert_eq!(antigravity, ("agy".to_string(), "gemini-4-argon".to_string()));
}
