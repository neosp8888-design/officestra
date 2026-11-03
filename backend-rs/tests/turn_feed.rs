// 기록·피드 조회 경로 통합 테스트. DB가 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use tower::ServiceExt;

fn state(pool: sqlx::PgPool) -> AppState {
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
        roleplay: None,
        voice_follow: Arc::default(),
    }
}

fn state_without_db() -> AppState {
    state(db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap())
}

/// 상태 코드와 응답 원문. 숫자·시각 표기를 바이트로 비교하려고 해석하지 않은 본문도 돌려준다.
async fn call(state: &AppState, method: &str, path: &str) -> (StatusCode, String, Value) {
    let request = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let value = serde_json::from_str(&text).unwrap();
    (status, text, value)
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn 조회_경로는_db_전에_node와_같은_오류를_낸다() {
    let state = state_without_db();
    let (status, _, body) = call(&state, "GET", "/api/turns/not-a-uuid/sources").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "turnId 값은 UUID여야 합니다." }));

    for (query, message) in [
        ("limit=101", "limit 값은 1 이상 100 이하 정수여야 합니다."),
        ("projectId=office", "projectId 값은 UUID여야 합니다."),
        ("kind=wrong", "kind 값이 올바르지 않습니다."),
    ] {
        let (status, _, body) = call(&state, "GET", &format!("/api/work-records?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body, json!({ "error": message }));
    }

    // Node는 분기 전에 경로 조각을 풀어 보므로, 구현하지 않은 경로·메서드도 잘못된 %면 500이다.
    for (method, path) in [
        ("GET", "/api/live-feed/%ZZ"),
        ("GET", "/api/turns/%E0%A4/sources"),
        ("POST", "/api/agent-jobs/%"),
        ("DELETE", "/api/turns/%ZZ"),
        ("GET", "/api/characters/%ZZ/history"),
        ("PUT", "/api/characters/%ZZ/settings"),
        ("POST", "/api/workspace-reviews/%ZZ/approve"),
        ("POST", "/api/terminal-sessions/%ZZ/events"),
    ] {
        let (status, _, body) = call(&state, method, path).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{method} {path}");
        assert_eq!(body, json!({ "error": "URI malformed" }), "{method} {path}");
    }
    // 패턴 밖의 경로는 그대로 404다.
    let (status, _, _) = call(&state, "GET", "/api/turns/%ZZ/other").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Node에서 쓰기 전용인 경로는 GET이 404다(4317 확인).
    for path in ["/api/turns", "/api/developer-tool-events", "/api/turns/6cb7bfb4-9cb9-456b-9447-cccd35571a76/feedback"] {
        let (status, _, body) = call(&state, "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body, json!({ "error": "경로를 찾을 수 없습니다." }), "{path}");
    }
}

#[tokio::test]
async fn 실제_db로_피드와_기록_조회의_키_순서와_표기를_확인한다() {
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
    migrate::run(&pool, std::path::Path::new(&migrations_dir)).await.unwrap();

    // 마이크로초가 있는 시각으로 최상위(Date, ms·Z)와 묶음 안(PG 표기) 차이를 본다.
    let turn_id: String = sqlx::query_scalar(
        "WITH character AS (
           INSERT INTO characters (id, name, seat, backend, permission) VALUES ('boss', '백부장', '상단', 'codex', 'workspace-write')
           RETURNING id
         ), conversation AS (
           INSERT INTO conversations (title, workdir) VALUES ('대화', '/tmp/work') RETURNING id
         ), session AS (
           INSERT INTO cli_sessions (conversation_id, character_id, external_id, started_at)
           SELECT conversation.id, character.id, 'rs-feed-test-no-transcript', '2026-10-31 01:00:00.987654+00'
           FROM conversation, character RETURNING id, character_id
         ), active AS (
           INSERT INTO active_cli_sessions (character_id, cli_session_id) SELECT character_id, id FROM session
         )
         INSERT INTO turns (cli_session_id, prompt, backend, model, status, started_at, ended_at, updated_at)
         SELECT id, '질문', 'codex', 'gpt-5.6-sol', 'completed',
           '2026-10-31 01:02:03.123456+00', '2026-10-31 01:02:09.000999+00', '2026-10-31 01:02:09.5+00'
         FROM session RETURNING id::text",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // 값은 모두 위에서 만든 상수다.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO messages (turn_id, role, text) VALUES ('{turn_id}', 'assistant', '  응답  ');
         INSERT INTO usage_records (turn_id, cost_usd) VALUES ('{turn_id}', 0.00005);
         INSERT INTO turn_activities (turn_id, seq, kind, text, status, occurred_at)
           VALUES ('{turn_id}', 1, 'tool', '도구', 'completed', '2026-10-31 01:02:04.5+00');
         INSERT INTO turn_response_sources (turn_id, ordinal, source_kind, title, locator)
           VALUES ('{turn_id}', 0, 'file', '근거', 'src/a.rs:1');
         INSERT INTO reply_deliveries (source_turn_id, recipient_character_id, status) VALUES ('{turn_id}', 'boss', 'delivered');
         WITH project AS (INSERT INTO projects (repository_root) VALUES ('/tmp/work') RETURNING id)
         INSERT INTO work_records (project_id, record_type, title, recorded_at, updated_at)
           SELECT id, 'decision', '결정', '2026-10-30 10:00:00.123456+00', '2026-10-30 10:00:00+00' FROM project;"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let state = state(pool);

    let (status, _, body) = call(&state, "GET", "/api/active-sessions").await;
    assert_eq!(status, StatusCode::OK);
    let session = &body["sessions"][0];
    assert_eq!(keys(session), ["characterId", "externalSessionId", "conversationId", "startedAt"]);
    assert_eq!(session["startedAt"], "2026-10-31T01:00:00.987Z");

    let (status, raw, body) = call(&state, "GET", "/api/live-feed").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(keys(&body), ["turns", "costSummary"]);
    let turn = &body["turns"][0];
    assert_eq!(
        keys(turn),
        [
            "id", "characterId", "characterName", "characterBackend", "backend", "model", "resolvedModel", "effort",
            "fastMode", "origin", "promptSender", "requestSource", "providerKind", "providerContextWindow",
            "externalSessionId", "conversationWorkdir", "prompt", "status", "needsInput", "errorMessage",
            "responseSourceWarning", "wikiProposalWarning", "feedback", "startedAt", "endedAt", "updatedAt",
            "estimatedCostUsd", "workspace", "response", "activities", "sources", "replyDeliveries", "replyDelivery",
            "sessionContext",
        ]
    );
    assert_eq!(turn["startedAt"], "2026-10-31T01:02:03.123Z");
    assert_eq!(turn["endedAt"], "2026-10-31T01:02:09.000Z");
    assert_eq!(turn["updatedAt"], "2026-10-31T01:02:09.500Z");
    assert_eq!(turn["activities"][0]["occurredAt"], "2026-10-31T01:02:04.5+00:00");
    assert_eq!(turn["response"], "응답");
    assert_eq!(turn["replyDelivery"], json!({ "sourceTurnId": turn_id, "recipientId": "boss", "recipientName": "백부장", "status": "delivered", "error": null }));
    assert_eq!(turn["sessionContext"], Value::Null);
    assert!(raw.contains(r#""estimatedCostUsd":0.00005,"#), "{raw}");
    assert_eq!(keys(&body["costSummary"]), ["version", "characters"]);
    assert_eq!(
        keys(&body["costSummary"]["characters"][0]),
        [
            "characterId", "finishedTurnCount", "failedTurnCount", "interruptedTurnCount", "pricedTurnCount",
            "totalCostUsd", "timedCostUsd", "totalDurationSeconds", "likedCount", "dislikedCount",
        ]
    );
    assert!(raw.contains(r#""totalCostUsd":0.00005,"timedCostUsd":0.00005,"totalDurationSeconds":5.877543,"#), "{raw}");

    // 실시간 한 건과 Node의 오류 문구(pg 메시지만).
    let (status, _, body) = call(&state, "GET", &format!("/api/live-feed/{turn_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["turn", "costSummary"]);
    let (status, _, body) = call(&state, "GET", "/api/live-feed/00000000-0000-4000-8000-000000000000").await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "대화를 찾을 수 없습니다." })));
    let (status, _, body) = call(&state, "GET", "/api/live-feed/not-a-uuid").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({ "error": "invalid input syntax for type uuid: \"not-a-uuid\"" }));
    let (status, _, body) = call(&state, "GET", "/api/live-feed?limit=abc").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({ "error": "invalid input syntax for type bigint: \"NaN\"" }));

    let (status, _, body) = call(&state, "GET", "/api/archive-feed?query=%EC%9D%91%EB%8B%B5&offset=abc").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["turns", "total"]);
    assert_eq!(body["total"], 1);
    assert_eq!(body["turns"][0]["id"], turn_id.as_str());
    let (_, _, body) = call(&state, "GET", "/api/archive-feed?query=%20%20").await;
    assert_eq!(body["total"], 1);
    let (_, _, body) = call(&state, "GET", "/api/archive-feed?query=nothing-matches").await;
    assert_eq!(body, json!({ "turns": [], "total": 0 }));

    let (status, _, body) = call(&state, "GET", "/api/history?characterId=boss").await;
    assert_eq!(status, StatusCode::OK);
    let history = &body["turns"][0];
    assert_eq!(history["backend"], "codex");
    assert_eq!(history["startedAt"], "2026-10-31T01:02:03.123Z");
    assert_eq!(history["sources"][0]["locator"], "src/a.rs:1");
    assert!(history.get("sessionContext").is_none());
    let (status, _, body) = call(&state, "GET", "/api/history?from=garbage").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({ "error": "invalid input syntax for type timestamp with time zone: \"garbage\"" }));
    let (_, _, body) = call(&state, "GET", "/api/history?from=2026-11-01").await;
    assert_eq!(body, json!({ "turns": [] }));

    let (status, _, body) = call(&state, "GET", &format!("/api/turns/{}/sources", turn_id.to_uppercase())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["sources", "warning"]);
    assert_eq!(keys(&body["sources"][0]), ["id", "sourceKind", "title", "locator", "excerpt", "ragDocumentId", "workRecordId"]);
    let (status, _, _) = call(&state, "GET", "/api/turns/00000000-0000-4000-8000-000000000000/sources").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, body) = call(&state, "GET", "/api/work-records?kind=decision&limit=0x2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys(&body), ["records", "lifecycleStateMeaning", "total", "limit", "offset"]);
    assert_eq!((body["total"].clone(), body["limit"].clone(), body["offset"].clone()), (json!(1), json!(2), json!(0)));
    let record = &body["records"][0];
    assert_eq!(record["recordedAt"], "2026-10-30T10:00:00.123456+00:00");
    assert_eq!(record["items"], json!([]));
    assert_eq!(record["score"], Value::Null);
}
