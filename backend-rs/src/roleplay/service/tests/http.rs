// 롤플레이 HTTP 경로 시험. Node roleplayEndpoint의 분기 순서·상태 코드를 라우터로 확인한다.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::health::Runtime;
use crate::http::{AppState, router};
use crate::local_embedding::LocalEmbeddingService;

fn state(pool: PgPool, roleplay: Option<RoleplayService>) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay,
        voice_follow: Arc::default(),
    }
}

async fn call(state: &AppState, method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<Body>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router(state.clone()).oneshot(request.body(body.into()).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
}

const JSON: &[(&str, &str)] = &[("content-type", "application/json")];

fn failure(status: StatusCode, message: &str) -> (StatusCode, Value) {
    (status, json!({ "error": message }))
}

#[tokio::test]
async fn routes_follow_node_order_without_database() {
    let missing = state(lazy_pool(), None);
    // 직원 ID 해석이 서비스 확인보다 먼저다.
    assert_eq!(call(&missing, "GET", "/api/roleplay/profiles/%E0%A4", &[], "").await, failure(StatusCode::INTERNAL_SERVER_ERROR, "URI malformed"));
    assert_eq!(
        call(&missing, "GET", "/api/roleplay/assets", &[], "").await,
        failure(StatusCode::SERVICE_UNAVAILABLE, "롤플레이 서비스가 준비되지 않았습니다.")
    );

    let keys = key_file("secret-key\n");
    let fetch = FakeFetch::new(|_| respond(200, json!({ "data": [{ "id": "b" }, { "id": "a", "description": "에이" }] })));
    let app = state(lazy_pool(), Some(offline_service(fetch, "http://proxy.test/v1/messages", keys.clone())));
    assert_eq!(
        call(&app, "GET", "/api/roleplay/models", &[], "").await,
        (StatusCode::OK, json!({ "models": [{ "id": "a", "title": "에이" }, { "id": "b", "title": "b" }] }))
    );
    let unsupported = "상태 변경 요청은 application/json 형식이어야 합니다.";
    assert_eq!(call(&app, "GET", "/api/roleplay/other", &[], "").await, failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, unsupported));
    assert_eq!(call(&app, "POST", "/api/roleplay/assets", &[("content-type", "text/plain")], "{}").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(call(&app, "DELETE", "/api/roleplay/profiles/%E0", JSON, "").await, failure(StatusCode::INTERNAL_SERVER_ERROR, "URI malformed"));
    let foreign = [("content-type", "application/json"), ("origin", "http://evil.test"), ("host", "127.0.0.1:4318")];
    assert_eq!(call(&app, "POST", "/api/roleplay/assets", &foreign, "{}").await, failure(StatusCode::FORBIDDEN, "신뢰할 수 없는 요청 출처입니다."));
    assert_eq!(call(&app, "POST", "/api/roleplay/assets", JSON, "{").await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let not_supported = failure(StatusCode::METHOD_NOT_ALLOWED, "지원하지 않는 롤플레이 요청입니다.");
    assert_eq!(call(&app, "PATCH", "/api/roleplay/assets", JSON, "").await, not_supported);
    assert_eq!(call(&app, "POST", "/api/roleplay/", JSON, "").await, not_supported);
    assert_eq!(call(&app, "POST", "/api/roleplay/profiles/a/b", JSON, "").await, not_supported);
    assert_eq!(call(&app, "GET", "/api/roleplay/profiles/a/chats", JSON, "").await, not_supported);
    assert_eq!(call(&app, "GET", "/api/roleplay", &[], "").await, failure(StatusCode::NOT_FOUND, "경로를 찾을 수 없습니다."));
    assert_eq!(call(&app, "POST", "/api/roleplay/assets", JSON, "{}").await, failure(StatusCode::BAD_REQUEST, "파일 내용는 문자열이어야 합니다."));
    // Node readJSON처럼 크기 제한이 없어 2MB를 넘는 가져오기 본문도 서비스까지 간다.
    let large = json!({ "fileName": "큰.risum", "dataBase64": "A".repeat(3_000_000) }).to_string();
    assert_eq!(call(&app, "POST", "/api/roleplay/assets", JSON, large).await.0, StatusCode::BAD_REQUEST);
    std::fs::remove_file(keys).unwrap();
}

#[tokio::test]
async fn routes_round_trip_with_database() {
    use futures_util::FutureExt;

    let Some(pool) = database().await else { return };
    let id = format!("roleplay-http-{}", random_uuid());
    let namespace = "roleplay-http-test";
    let preset_file = risu_preset_file(&synthetic_preset(namespace));
    let module_file = risu_module_file(&synthetic_module(namespace));
    let hashes = vec![sha256_hex(&preset_file), sha256_hex(&module_file)];
    let cleanup = async || {
        sqlx::query("DELETE FROM characters WHERE id = $1").bind(&id).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM roleplay_assets WHERE source_sha256 = ANY($1)").bind(&hashes).execute(&pool).await.unwrap();
    };
    cleanup().await;
    sqlx::query("INSERT INTO characters (id, name, seat, backend, permission, config) VALUES ($1, '코대리', '상단', 'claude', 'acceptEdits', '{}')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let keys = key_file("test-key\n");
    let fetch = FakeFetch::new(|_| respond(200, text_reply("답 123")));
    let service = service_on(pool.clone(), fetch, "http://192.0.2.1:1/v1/messages", keys.clone());
    let app = state(pool.clone(), Some(service.clone()));

    let scenarios = async {
        let import = async |file_name: &str, bytes: &[u8]| {
            let body = json!({ "fileName": file_name, "dataBase64": crate::local_provider::host::base64_encode(bytes) });
            call(&app, "POST", "/api/roleplay/assets", JSON, body.to_string()).await
        };
        let (status, first) = import("합성.risup", &preset_file).await;
        assert_eq!((status, &first["created"]), (StatusCode::CREATED, &json!(true)));
        let (status, again) = import("합성.risup", &preset_file).await;
        assert_eq!((status, &again["created"]), (StatusCode::OK, &json!(false)));
        assert_eq!(import("합성.risum", &module_file).await.0, StatusCode::CREATED);
        let preset_id = first["asset"]["id"].as_str().unwrap().to_string();
        let (status, assets) = call(&app, "GET", "/api/roleplay/assets", &[], "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(assets["assets"].as_array().unwrap().iter().any(|asset| asset["id"] == preset_id));

        assert_eq!(
            call(&app, "GET", "/api/roleplay/profiles/roleplay-missing", &[], "").await,
            failure(StatusCode::NOT_FOUND, "직원을 찾을 수 없습니다.")
        );
        // 경로의 직원 ID는 decodeURIComponent로 푼다.
        let encoded = format!("/api/roleplay/profiles/{}", id.replace('-', "%2D"));
        let (status, profile) = call(&app, "GET", &encoded, &[], "").await;
        assert_eq!((status, &profile["profile"]["characterId"]), (StatusCode::OK, &json!(id)));
        let path = format!("/api/roleplay/profiles/{id}");
        assert_eq!(call(&app, "PUT", &path, JSON, "null").await.0, StatusCode::INTERNAL_SERVER_ERROR);
        let (status, saved) = call(&app, "PUT", &path, JSON, json!({ "presetId": preset_id }).to_string()).await;
        assert_eq!((status, &saved["profile"]["presetId"]), (StatusCode::OK, &json!(preset_id)));

        let (status, accepted) = call(&app, "POST", &format!("{path}/messages"), JSON, json!({ "content": "/안녕" }).to_string()).await;
        assert_eq!((status, &accepted["accepted"]), (StatusCode::ACCEPTED, &json!(true)));
        service.wait_for_reply(&id).await;
        let (_, state_after) = call(&app, "GET", &path, &[], "").await;
        assert_eq!(conversation(&state_after), [("user".into(), "안녕".into()), ("char".into(), "답 [123]".into())]);
        assert_eq!(accepted["chatId"], state_after["profile"]["currentChatId"]);

        let (status, fresh) = call(&app, "POST", &format!("{path}/chats"), JSON, "").await;
        assert_eq!((status, &fresh["messages"]), (StatusCode::CREATED, &json!([])));
        assert_ne!(fresh["profile"]["currentChatId"], accepted["chatId"]);
    };
    let outcome = std::panic::AssertUnwindSafe(scenarios).catch_unwind().await;
    cleanup().await;
    std::fs::remove_file(&keys).unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
