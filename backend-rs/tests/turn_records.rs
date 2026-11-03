// 완료 턴 업무 기록 저장·재조정과 대화 출처 교체·평가·삭제 HTTP 경로 통합 테스트. 기대값은 같은 시드에서 Node가 남긴 결과다.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use officestra_backend::db::{self, with_transaction};
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::migrate;
use officestra_backend::realtime::Realtime;
use officestra_backend::wiki_knowledge::{TurnWikiProposals, persist_turn_wiki_proposals};
use officestra_backend::work_record_memory::{
    CompletedTurnRecord, WorkRecordError, persist_completed_turn_work_record, reconcile_terminal_work_record_reviews,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const T1: &str = "00000000-0000-4000-8000-0000000000d1";
const T2: &str = "00000000-0000-4000-8000-0000000000d2";
const T3: &str = "00000000-0000-4000-8000-0000000000d3";
const T4: &str = "00000000-0000-4000-8000-0000000000d4";
const T5: &str = "00000000-0000-4000-8000-0000000000d5";
const T6: &str = "00000000-0000-4000-8000-0000000000d6";
const W1: &str = "00000000-0000-4000-8000-0000000000b1";
const MISSING: &str = "99999999-9999-4999-8999-999999999999";
const ROOT: &str = "/repo/office";
const READY: &str = r#"{"type":"ready","compactingCharacterIds":[]}"#;

/// 세션 a1(boss, 활성)·a2(left-woman, 활성)·a3(boss, 비활성)에 완료·진행·실패 턴을 둔다. T1은 병합된 작업 공간의 검토 턴이다.
const SEED: &str = r#"
INSERT INTO characters (id, name, seat, backend, permission) VALUES
  ('boss', '보스', '상단', 'codex', 'workspace-write'),
  ('left-woman', '왼쪽', '왼쪽', 'antigravity', 'workspace-write'),
  ('c', '셋째', '오른쪽', 'claude', 'workspace-write');
INSERT INTO conversations (id, title, workdir) VALUES ('00000000-0000-4000-8000-0000000000c1', '대화', '/repo/office');
INSERT INTO cli_sessions (id, conversation_id, character_id, started_at) VALUES
  ('00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-0000000000c1', 'boss', '2020-01-01 00:00:01+00'),
  ('00000000-0000-4000-8000-0000000000a2', '00000000-0000-4000-8000-0000000000c1', 'left-woman', '2020-01-01 00:00:02+00'),
  ('00000000-0000-4000-8000-0000000000a3', '00000000-0000-4000-8000-0000000000c1', 'boss', '2020-01-01 00:00:03+00');
INSERT INTO active_cli_sessions (character_id, cli_session_id) VALUES
  ('boss', '00000000-0000-4000-8000-0000000000a1'),
  ('left-woman', '00000000-0000-4000-8000-0000000000a2');
INSERT INTO turns (id, cli_session_id, prompt, backend, status, needs_input, origin, started_at) VALUES
  ('00000000-0000-4000-8000-0000000000d1', '00000000-0000-4000-8000-0000000000a3', '작업', 'codex', 'completed', false, 'gui', '2020-01-01 00:01:01+00'),
  ('00000000-0000-4000-8000-0000000000d2', '00000000-0000-4000-8000-0000000000a3', '확인', 'claude', 'completed', true, 'gui', '2020-01-01 00:01:02+00'),
  ('00000000-0000-4000-8000-0000000000d3', '00000000-0000-4000-8000-0000000000a2', '터미널', 'antigravity', 'completed', false, 'terminal', '2020-01-01 00:01:03+00'),
  ('00000000-0000-4000-8000-0000000000d4', '00000000-0000-4000-8000-0000000000a2', '진행', 'antigravity', 'running', false, 'gui', '2020-01-01 00:01:04+00'),
  ('00000000-0000-4000-8000-0000000000d5', '00000000-0000-4000-8000-0000000000a1', '지울 턴', 'codex', 'completed', false, 'gui', '2020-01-01 00:01:05+00'),
  ('00000000-0000-4000-8000-0000000000d6', '00000000-0000-4000-8000-0000000000a3', '실패', 'codex', 'failed', false, 'gui', '2020-01-01 00:01:06+00');
INSERT INTO task_workspaces (id, cli_session_id, status, repository_root, source_workdir, worktree_path, execution_workdir,
  branch_name, base_branch, base_commit, review_turn_id, review_tree, head_commit, changed_files, task_commit, merged_commit,
  created_at, updated_at, merged_at)
VALUES ('00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a3', 'merged', '/repo/office', '/repo/office',
  '/repo/worktrees/w1', '/repo/worktrees/w1', 'office/w1', 'main', 'base1', '00000000-0000-4000-8000-0000000000d1', 'tree1', 'head1',
  '[{"path":"a.rs","status":"M"}]', 'task1', 'merge1', '2020-01-01 00:02:00+00', '2020-01-01 00:02:00+00', '2020-01-01 00:03:00+00');
UPDATE turns SET task_workspace_id = '00000000-0000-4000-8000-0000000000b1' WHERE id = '00000000-0000-4000-8000-0000000000d1';
INSERT INTO messages (turn_id, role, text, received_at) VALUES
  ('00000000-0000-4000-8000-0000000000d1', 'assistant', '결과입니다', '2020-01-01 00:01:11+00'),
  ('00000000-0000-4000-8000-0000000000d5', 'assistant', '곧 지움', '2020-01-01 00:01:15+00');
"#;

fn base<'a>(turn_id: &'a str, character_id: &'a str, prompt: &'a str, response: &'a str) -> CompletedTurnRecord<'a> {
    CompletedTurnRecord { repository_root: ROOT, turn_id, character_id, prompt, response, ..Default::default() }
}

async fn persist(pool: &PgPool, record: CompletedTurnRecord<'_>) -> Result<Option<String>, WorkRecordError> {
    with_transaction(pool, async |connection| persist_completed_turn_work_record(connection, &record).await).await
}

async fn reconcile(pool: &PgPool, root: &str) -> Vec<String> {
    with_transaction(pool, async |connection| reconcile_terminal_work_record_reviews(connection, Some(root)).await)
        .await
        .unwrap()
}

async fn record_row(pool: &PgPool, turn: &str) -> Option<(String, String, Value)> {
    sqlx::query_as("SELECT title, body, metadata FROM work_records WHERE source_turn_id = $1::uuid")
        .bind(turn)
        .fetch_optional(pool)
        .await
        .unwrap()
}

type EventRow = (i32, String, String, Option<String>, Option<String>);

async fn events(pool: &PgPool, turn: &str) -> Vec<EventRow> {
    sqlx::query_as(
        "SELECT event.record_version, event.event_type, event.actor_type, event.actor_character_id, event.idempotency_key
         FROM work_record_events AS event JOIN work_records AS record ON record.id = event.record_id
         WHERE record.source_turn_id = $1::uuid ORDER BY event.record_version",
    )
    .bind(turn)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn event(version: i32, kind: &str, actor: &str, character: Option<&str>, key: Option<&str>) -> EventRow {
    (version, kind.into(), actor.into(), character.map(Into::into), key.map(Into::into))
}

async fn count(pool: &PgPool, sql: &'static str, turn: &str) -> i64 {
    sqlx::query_scalar(sql).bind(turn).fetch_one(pool).await.unwrap()
}

async fn call(state: &AppState, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, String) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router(state.clone()).oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn failure(status: StatusCode, message: &str) -> (StatusCode, String) {
    (status, json!({ "error": message }).to_string())
}

async fn next_event(socket: &mut Socket) -> String {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next()).await.expect("방송이 오지 않았다");
    match message.unwrap().unwrap() {
        Message::Text(text) => text.to_string(),
        other => panic!("문자 방송이 아니다: {other:?}"),
    }
}

const JSON: (&str, &str) = ("content-type", "application/json");

#[tokio::test]
async fn 실제_db로_완료_턴_기록과_출처_평가_삭제_경로를_확인한다() {
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
    sqlx::raw_sql(SEED).execute(&pool).await.unwrap();

    // 첫 저장은 프로젝트를 만들고 제목·본문·metadata·기록 시각·생성 사건을 남긴다.
    let changed = json!([{ "path": "a.rs", "status": "M" }]);
    let first = persist(
        &pool,
        CompletedTurnRecord {
            workspace_id: Some(W1),
            backend: Some("codex"),
            model: Some("gpt-5.6-sol"),
            response_source_count: 1,
            review_status: Some("awaiting_approval"),
            review_tree: Some("tree1"),
            head_commit: Some("head1"),
            changed_files: Some(&changed),
            recorded_at: Some("2020-02-02 02:02:02.123456+00"),
            ..base(T1, "boss", "  첫 줄 요청  \n둘째 줄", " 결과입니다 ")
        },
    )
    .await
    .unwrap()
    .expect("새 기록 ID");
    let t1_body = "요청\n첫 줄 요청  \n둘째 줄\n\n결과\n결과입니다";
    assert_eq!(
        record_row(&pool, T1).await.unwrap(),
        (
            "첫 줄 요청".to_string(),
            t1_body.to_string(),
            json!({
                "source": "completed_turn",
                "needsInput": false,
                "responseSourceCount": 1,
                "review": { "status": "awaiting_approval", "reviewTree": "tree1", "headCommit": "head1", "changedFiles": changed },
                "backend": "codex",
                "model": "gpt-5.6-sol",
            })
        )
    );
    let stored: (String, String, Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT record.record_type, record.attribution, record.character_id, record.source_workspace_id::text,
           record.recorded_at = '2020-02-02 02:02:02.123456+00'::timestamptz
         FROM work_records AS record WHERE record.id = $1::uuid",
    )
    .bind(&first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, ("result".into(), "character".into(), Some("boss".into()), Some(W1.into()), true));
    let project: (String, Option<String>) =
        sqlx::query_as("SELECT repository_root, name FROM projects").fetch_one(&pool).await.unwrap();
    assert_eq!(project, (ROOT.into(), Some("office".into())));
    let t1_created = event(1, "created", "character", Some("boss"), Some(&format!("completed-turn:{T1}")));
    assert_eq!(events(&pool, T1).await, vec![t1_created.clone()]);

    // 같은 턴을 다시 저장해도, 터미널 갱신을 요청해도 터미널 턴이 아니면 기록이 바뀌지 않는다.
    let again = persist(&pool, CompletedTurnRecord { workspace_id: Some(W1), ..base(T1, "boss", "다른 요청", "다른 결과") });
    assert_eq!(again.await.unwrap().as_deref(), Some(first.as_str()));
    let refresh = CompletedTurnRecord { refresh_terminal_result: true, ..base(T1, "boss", "갱신 시도", "바뀌면 안 됨") };
    assert_eq!(persist(&pool, refresh).await.unwrap().as_deref(), Some(first.as_str()));
    assert_eq!(record_row(&pool, T1).await.unwrap().1, t1_body);
    assert_eq!(events(&pool, T1).await, vec![t1_created.clone()]);

    // 터미널 턴은 최종 결과로 한 번만 갱신 사건을 남긴다.
    let terminal = |response: &'static str| CompletedTurnRecord { backend: Some("antigravity"), ..base(T3, "left-woman", "터미널", response) };
    let t3 = persist(&pool, terminal("대기 문구")).await.unwrap().unwrap();
    for _ in 0..2 {
        let refreshed = persist(&pool, CompletedTurnRecord { refresh_terminal_result: true, ..terminal("최종 결과") });
        assert_eq!(refreshed.await.unwrap().as_deref(), Some(t3.as_str()));
    }
    assert_eq!(record_row(&pool, T3).await.unwrap().1, "요청\n터미널\n\n결과\n최종 결과");
    assert_eq!(
        events(&pool, T3).await,
        vec![
            event(1, "created", "character", Some("left-woman"), Some(&format!("completed-turn:{T3}"))),
            event(2, "updated", "character", Some("left-woman"), None),
        ]
    );

    // 저장소 경로가 비면 거절하고 아무것도 남기지 않는다.
    let blank = persist(&pool, CompletedTurnRecord { repository_root: "  ", ..base(T6, "boss", "x", "y") }).await;
    assert_eq!(blank.unwrap_err().to_string(), "작업 기록의 저장소 경로가 필요합니다.");
    assert!(record_row(&pool, T6).await.is_none());
    let t5 = persist(&pool, base(T5, "boss", "지울 턴", "곧 지움")).await.unwrap().unwrap();

    // 병합된 작업 공간의 검토 대기 기록은 기동 재조정에서 merged로 한 번만 넘어간다.
    assert_eq!(reconcile(&pool, ROOT).await, vec![first.clone()]);
    assert!(reconcile(&pool, ROOT).await.is_empty());
    assert!(reconcile(&pool, "/elsewhere").await.is_empty());
    assert_eq!(
        record_row(&pool, T1).await.unwrap().2["review"],
        json!({
            "status": "merged", "headCommit": "head1", "reviewTree": "tree1", "taskCommit": "task1",
            "changedFiles": changed, "errorMessage": null, "mergedCommit": "merge1",
        })
    );
    assert_eq!(
        events(&pool, T1).await,
        vec![t1_created, event(2, "state_changed", "system", None, Some("review:merged:task1"))]
    );

    // 완료 턴의 위키 수정안은 하나씩 SAVEPOINT로 저장하고, 실패한 것만 경고 문구로 모은다. 다시 저장해도 늘지 않는다.
    let good = |page_key: &str, tier: &str| {
        json!({ "pageKey": page_key, "kind": "decision", "approvalTier": tier, "title": format!("{page_key} 제목"), "body": "본문" })
    };
    let mut odd_kind = good("odd-kind", "auto");
    odd_kind["kind"] = json!("note");
    let batch = json!([good("auto-page", "auto"), good("user-page", "user"), good("peer-page", "peer"), good("Bad Key", "auto"), odd_kind]);
    let failures = "위키 수정안 4번을 저장하지 못했습니다: pageKey는 영문 소문자·숫자로 시작하는 1~80자의 슬러그여야 합니다. \
                    위키 수정안 5번을 저장하지 못했습니다: 알 수 없는 제안 종류입니다. note";
    for (warning, expected) in [(Some("해석 경고"), format!("해석 경고 {failures}")), (None, failures.to_string())] {
        let saved = with_transaction(&pool, async |connection| {
            persist_turn_wiki_proposals(
                connection,
                TurnWikiProposals {
                    repository_root: ROOT,
                    work_record_id: Some(&first),
                    turn_id: T1,
                    character_id: "boss",
                    proposals: Some(&batch),
                    parser_warning: warning.map(str::to_string),
                    needs_input: false,
                },
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(saved, Some(expected));
    }
    let proposals: Vec<(i32, String, String, String)> = sqlx::query_as(
        "SELECT ordinal, page_key, state, approval_grade FROM wiki_proposals WHERE author_turn_id = $1::uuid ORDER BY ordinal",
    )
    .bind(T1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        proposals,
        [
            (0, "auto-page".into(), "drafted".into(), "auto".into()),
            (1, "user-page".into(), "pending_user".into(), "user".into()),
            (2, "peer-page".into(), "drafted".into(), "peer".into()),
        ]
    );

    // HTTP 경로는 실제 웹소켓 연결로 방송까지 본다.
    let realtime = Arc::new(Realtime::default());
    let state = AppState {
        pool: pool.clone(),
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::clone(&realtime),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let (mut socket, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
    assert_eq!(next_event(&mut socket).await, READY);

    // 출처 교체. 형식·출처·UUID·본문·턴 상태를 차례로 검사하고, 참조가 없으면 기존 출처를 그대로 둔다.
    let sources = format!("/api/turns/{T1}/sources");
    let file_and_web = json!({ "sources": [
        { "kind": "file", "title": "근거", "locator": "src/a.rs:1", "excerpt": "발췌" },
        { "kind": "web", "title": "웹", "locator": "https://example.com/x" },
        { "kind": "database", "title": "지울 기록", "locator": format!("work_records/{t5}"), "workRecordId": t5 },
    ] })
    .to_string();
    let untrusted = [JSON, ("origin", "http://evil.example"), ("host", "127.0.0.1:4317")];
    assert_eq!(
        call(&state, "PUT", &sources, &[], &file_and_web).await,
        failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "상태 변경 요청은 application/json 형식이어야 합니다.")
    );
    assert_eq!(
        call(&state, "PUT", &sources, &untrusted, &file_and_web).await,
        failure(StatusCode::FORBIDDEN, "신뢰할 수 없는 요청 출처입니다.")
    );
    assert_eq!(
        call(&state, "PUT", "/api/turns/not-a-uuid/sources", &[JSON], &file_and_web).await,
        failure(StatusCode::BAD_REQUEST, "turnId 값은 UUID여야 합니다.")
    );
    assert_eq!(
        call(&state, "PUT", "/api/turns/%zz/sources", &[JSON], &file_and_web).await,
        failure(StatusCode::INTERNAL_SERVER_ERROR, "URI malformed")
    );
    assert_eq!(
        call(&state, "PUT", &sources, &[JSON], "null").await,
        failure(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read properties of null (reading 'sources')")
    );
    assert_eq!(
        call(&state, "PUT", &format!("/api/turns/{MISSING}/sources"), &[JSON], &file_and_web).await,
        failure(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다.")
    );
    assert_eq!(
        call(&state, "PUT", &format!("/api/turns/{T4}/sources"), &[JSON], &file_and_web).await,
        failure(StatusCode::CONFLICT, "진행 중인 대화의 출처는 완료 후 수정할 수 있습니다.")
    );
    sqlx::query("UPDATE turns SET response_source_warning = '출처 경고' WHERE id = $1::uuid").bind(T1).execute(&pool).await.unwrap();
    let (status, body) = call(&state, "PUT", &sources, &[JSON], &file_and_web).await;
    assert_eq!(status, StatusCode::OK);
    let mut stored: Value = serde_json::from_str(&body).unwrap();
    for source in stored["sources"].as_array_mut().unwrap() {
        assert_eq!(source["id"].as_str().unwrap().len(), 36);
        source["id"] = json!("id");
    }
    assert_eq!(
        stored.to_string(),
        json!({ "sources": [
            { "id": "id", "sourceKind": "file", "title": "근거", "locator": "src/a.rs:1", "excerpt": "발췌", "ragDocumentId": null, "workRecordId": null },
            { "id": "id", "sourceKind": "web", "title": "웹", "locator": "https://example.com/x", "excerpt": null, "ragDocumentId": null, "workRecordId": null },
            { "id": "id", "sourceKind": "database", "title": "지울 기록", "locator": format!("work_records/{t5}"), "excerpt": null, "ragDocumentId": null, "workRecordId": t5 },
        ] })
        .to_string()
    );
    assert_eq!(next_event(&mut socket).await, json!({ "type": "feed.changed", "turnId": T1 }).to_string());
    let warning: Option<String> =
        sqlx::query_scalar("SELECT response_source_warning FROM turns WHERE id = $1::uuid").bind(T1).fetch_one(&pool).await.unwrap();
    assert_eq!(warning, None);
    let missing_reference = json!({ "sources": [
        { "kind": "database", "title": "없는 기록", "locator": format!("work_records/{MISSING}"), "workRecordId": MISSING },
    ] })
    .to_string();
    assert_eq!(
        call(&state, "PUT", &sources, &[JSON], &missing_reference).await,
        failure(StatusCode::BAD_REQUEST, "DB 작업 기록 참조를 찾을 수 없습니다.")
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM turn_response_sources WHERE turn_id = $1::uuid", T1).await, 3);

    // 평가. 완료된 턴만 받고 null은 평가를 지운다.
    let feedback = |turn: &str| format!("/api/turns/{turn}/feedback");
    let invalid = "feedback 값은 liked, disliked 또는 null이어야 합니다.";
    assert_eq!(
        call(&state, "PUT", &feedback(T1), &[], r#"{"feedback":"liked"}"#).await,
        failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "상태 변경 요청은 application/json 형식이어야 합니다.")
    );
    assert_eq!(
        call(&state, "PUT", &feedback("not-a-uuid"), &[JSON], r#"{"feedback":"liked"}"#).await,
        failure(StatusCode::BAD_REQUEST, "turnId 값은 UUID여야 합니다.")
    );
    for body in [r#"{"feedback":"meh"}"#, "{}", "null", r#"{"feedback":1}"#] {
        assert_eq!(call(&state, "PUT", &feedback(T1), &[JSON], body).await, failure(StatusCode::BAD_REQUEST, invalid), "{body}");
    }
    assert_eq!(
        call(&state, "PUT", &feedback(MISSING), &[JSON], r#"{"feedback":"liked"}"#).await,
        failure(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다.")
    );
    for turn in [T4, T6] {
        assert_eq!(
            call(&state, "PUT", &feedback(turn), &[JSON], r#"{"feedback":"liked"}"#).await,
            failure(StatusCode::CONFLICT, "완료된 대화 응답만 평가할 수 있습니다.")
        );
    }
    let feedback_event = |turn: &str| json!({ "type": "feed.changed", "turnId": turn, "feedbackChanged": true }).to_string();
    let stored_feedback = async |turn: &str| -> Option<String> {
        sqlx::query_scalar("SELECT feedback FROM turn_response_feedback WHERE turn_id = $1::uuid")
            .bind(turn)
            .fetch_optional(&pool)
            .await
            .unwrap()
    };
    for (turn, value) in [(T1, "liked"), (T1, "disliked"), (T5, "liked")] {
        let body = json!({ "feedback": value }).to_string();
        assert_eq!(call(&state, "PUT", &feedback(turn), &[JSON], &body).await, (StatusCode::OK, body));
        assert_eq!(next_event(&mut socket).await, feedback_event(turn));
        assert_eq!(stored_feedback(turn).await.as_deref(), Some(value));
    }
    sqlx::query("INSERT INTO turn_response_feedback (turn_id, feedback) VALUES ($1::uuid, 'liked')").bind(T2).execute(&pool).await.unwrap();
    let cleared = r#"{"feedback":null}"#;
    assert_eq!(call(&state, "PUT", &feedback(T2), &[JSON], cleared).await, (StatusCode::OK, cleared.to_string()));
    assert_eq!(next_event(&mut socket).await, feedback_event(T2));
    assert_eq!(stored_feedback(T2).await, None);

    // 삭제. 진행 중인 턴과 그 세션의 다른 턴은 막고, 지우면 파생 기록·출처·활성 세션까지 함께 정리한다.
    let turn_path = |turn: &str| format!("/api/turns/{turn}");
    assert_eq!(
        call(&state, "DELETE", &turn_path(T5), &[], "").await,
        failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "상태 변경 요청은 application/json 형식이어야 합니다.")
    );
    assert_eq!(
        call(&state, "DELETE", &turn_path(T4), &[JSON], "").await,
        failure(StatusCode::CONFLICT, "진행 중인 대화는 삭제할 수 없습니다.")
    );
    assert_eq!(
        call(&state, "DELETE", &turn_path(T3), &[JSON], "").await,
        failure(StatusCode::CONFLICT, "같은 세션에서 대화가 진행 중이라 삭제할 수 없습니다.")
    );
    assert_eq!(
        call(&state, "DELETE", "/api/turns/%20%20", &[JSON], "").await,
        failure(StatusCode::BAD_REQUEST, "삭제할 대화를 찾을 수 없습니다.")
    );
    for turn in [MISSING, "not-a-uuid"] {
        assert_eq!(
            call(&state, "DELETE", &turn_path(turn), &[JSON], "").await,
            failure(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다.")
        );
    }
    let deleted = |turn: &str, count: u64, reset: bool| {
        json!({ "deleted": true, "id": turn, "characterId": "boss", "deletedWorkRecordCount": count, "sessionReset": reset })
            .to_string()
    };
    let delete_event =
        |turn: &str| json!({ "type": "feed.changed", "turnId": turn, "characterId": "boss", "costChanged": true }).to_string();
    assert_eq!(call(&state, "DELETE", &turn_path(T5), &[JSON], "").await, (StatusCode::OK, deleted(T5, 1, true)));
    assert_eq!(next_event(&mut socket).await, delete_event(T5));
    for sql in [
        "SELECT count(*) FROM turns WHERE id = $1::uuid",
        "SELECT count(*) FROM work_records WHERE source_turn_id = $1::uuid",
        "SELECT count(*) FROM turn_response_feedback WHERE turn_id = $1::uuid",
        "SELECT count(*) FROM messages WHERE turn_id = $1::uuid",
    ] {
        assert_eq!(count(&pool, sql, T5).await, 0, "{sql}");
    }
    let active: Vec<String> =
        sqlx::query_scalar("SELECT character_id FROM active_cli_sessions ORDER BY character_id").fetch_all(&pool).await.unwrap();
    assert_eq!(active, ["left-woman"]);
    let ended: Vec<String> = sqlx::query_scalar("SELECT id::text FROM cli_sessions WHERE ended_at IS NOT NULL")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ended, ["00000000-0000-4000-8000-0000000000a1"]);
    let orphan_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_record_events AS event
         WHERE NOT EXISTS (SELECT 1 FROM work_records AS record WHERE record.id = event.record_id)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphan_events, 0);
    // 지운 기록을 가리키던 다른 턴의 출처만 빠지고 나머지 출처는 남는다.
    assert_eq!(count(&pool, "SELECT count(*) FROM turn_response_sources WHERE turn_id = $1::uuid", T1).await, 2);
    assert_eq!(call(&state, "DELETE", &turn_path(T2), &[JSON], "").await, (StatusCode::OK, deleted(T2, 0, false)));
    assert_eq!(next_event(&mut socket).await, delete_event(T2));
    assert_eq!(
        call(&state, "DELETE", &turn_path(T5), &[JSON], "").await,
        failure(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다.")
    );

    // 실패한 요청은 아무것도 방송하지 않았다.
    assert!(tokio::time::timeout(Duration::from_millis(300), socket.next()).await.is_err());
}
