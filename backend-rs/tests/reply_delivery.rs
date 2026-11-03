// 답변 자동 전달 태그·예약·전달 서비스 통합 테스트. Node reply-routes.postgres·reply-delivery 시험의 장면을 실제 DB와 가짜 실행기로 옮겼다.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use officestra_backend::agent_runtime::service::AgentError;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::reply_delivery::{
    DeliveryRuntime, DeliveryStart, ReplyDeliveryError, ReplyDeliveryService, RouteSaveError, cancel_pending_reply_delivery,
    read_reply_routes, record_reply_delivery, save_reply_route,
};
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

#[derive(Default)]
struct FakeState {
    busy: Mutex<HashSet<String>>,
    sent: Mutex<Vec<(DeliveryStart, String)>>,
    /// 다음 접수 가능 조회 직전에 저장할 태그(조회와 확정 사이의 경쟁 재현).
    before_availability: Mutex<Option<(String, Vec<String>, bool)>>,
    /// 다음 시작을 접수 전에 이 오류로 실패시킨다.
    start_error: Mutex<Option<AgentError>>,
    /// 다음 시작은 받는 턴이 전달을 확정한 뒤 이 문구로 실패한다.
    fail_after_accept: Mutex<Option<String>>,
    draining: AtomicBool,
    availability_calls: AtomicUsize,
}

#[derive(Clone)]
struct FakeRuntime {
    pool: PgPool,
    state: Arc<FakeState>,
}

impl DeliveryRuntime for FakeRuntime {
    fn draining(&self) -> bool {
        self.state.draining.load(Ordering::SeqCst)
    }

    async fn message_availability(&self) -> Result<Vec<Value>, AgentError> {
        self.state.availability_calls.fetch_add(1, Ordering::SeqCst);
        let hook = self.state.before_availability.lock().unwrap().take();
        if let Some((character, ids, paused)) = hook {
            let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
            setting(&self.pool, &character, &ids, paused).await.unwrap();
        }
        let busy = self.state.busy.lock().unwrap().clone();
        Ok(["a", "b", "c"].iter().map(|id| json!({ "characterId": id, "canReceive": !busy.contains(*id) })).collect())
    }

    async fn start_delivery(&self, start: DeliveryStart) -> Result<(), AgentError> {
        let start_error = self.state.start_error.lock().unwrap().take();
        if let Some(start_error) = start_error {
            return Err(start_error);
        }
        let turn = create_turn(&self.pool, &start.character_id, "running", "받은 답변").await;
        let mut connection = self.pool.acquire().await.unwrap();
        record_reply_delivery(&mut connection, &turn, &start.character_id, Some(&start.delivery_id))
            .await
            .map_err(|error| AgentError::Failed(error.to_string()))?;
        self.state.sent.lock().unwrap().push((start, turn));
        let failure = self.state.fail_after_accept.lock().unwrap().take();
        match failure {
            Some(message) => Err(AgentError::Failed(message)),
            None => Ok(()),
        }
    }
}

impl FakeRuntime {
    fn sent_to(&self) -> Vec<String> {
        self.state.sent.lock().unwrap().iter().map(|(start, _)| start.character_id.clone()).collect()
    }

    fn sent(&self) -> Vec<(DeliveryStart, String)> {
        self.state.sent.lock().unwrap().clone()
    }

    fn busy(&self, ids: &[&str]) {
        *self.state.busy.lock().unwrap() = ids.iter().map(|id| id.to_string()).collect();
    }
}

async fn create_turn(pool: &PgPool, character: &str, status: &str, text: &str) -> String {
    sqlx::query_scalar(
        "WITH conversation AS (
           INSERT INTO conversations (title, workdir) VALUES ('대화', '/tmp/work') RETURNING id
         ), session AS (
           INSERT INTO cli_sessions (conversation_id, character_id) SELECT id, $1 FROM conversation RETURNING id
         ), turn AS (
           INSERT INTO turns (cli_session_id, prompt, status) SELECT id, '질문', $2 FROM session RETURNING id
         )
         INSERT INTO messages (turn_id, role, text) SELECT id, 'assistant', $3 FROM turn RETURNING turn_id::text",
    )
    .bind(character)
    .bind(status)
    .bind(text)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn setting(pool: &PgPool, character: &str, ids: &[&str], paused: bool) -> Result<Value, RouteSaveError> {
    let body = json!({ "recipientIds": ids, "paused": paused });
    db::with_transaction(pool, async |connection| save_reply_route(connection, character, &body).await).await
}

async fn record(pool: &PgPool, turn: &str, character: &str) -> Vec<String> {
    let mut connection = pool.acquire().await.unwrap();
    let mut recipients = record_reply_delivery(&mut connection, turn, character, None).await.unwrap();
    recipients.sort();
    recipients
}

/// 이 턴 답변의 전달 예약(받는 직원 순): 받는 직원, 상태, 오류 문구.
async fn queued(pool: &PgPool, turn: &str) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT recipient_character_id, status, error_message FROM reply_deliveries
         WHERE source_turn_id=$1::uuid ORDER BY recipient_character_id",
    )
    .bind(turn)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn statuses(rows: &[(String, String, Option<String>)]) -> Vec<&str> {
    rows.iter().map(|(_, status, _)| status.as_str()).collect()
}

async fn legacy_reservation(pool: &PgPool, turn: &str, recipient: &str) {
    sqlx::query("INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES($1::uuid,$2)")
        .bind(turn)
        .bind(recipient)
        .execute(pool)
        .await
        .unwrap();
}

async fn reset(pool: &PgPool, fake: &FakeRuntime) {
    sqlx::raw_sql(
        "TRUNCATE characters, conversations, reply_routes, reply_route_recipients, reply_deliveries CASCADE;
         INSERT INTO characters (id, name, seat, backend, permission) VALUES
           ('a', 'A', '왼쪽', 'codex', 'workspace-write'),
           ('b', 'B', '가운데', 'codex', 'workspace-write'),
           ('c', 'C', '오른쪽', 'codex', 'workspace-write');",
    )
    .execute(pool)
    .await
    .unwrap();
    fake.state.sent.lock().unwrap().clear();
    fake.busy(&[]);
    fake.state.availability_calls.store(0, Ordering::SeqCst);
}

async fn advisory_locks(pool: &PgPool) -> i64 {
    // 같은 서버의 다른 DB(운영 백엔드의 전달 서비스 등) 잠금은 세지 않는다.
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype='advisory'
           AND database=(SELECT oid FROM pg_database WHERE datname=current_database())",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn app_state(pool: PgPool) -> AppState {
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

async fn call(state: &AppState, method: &str, path: &str, content_type: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = router(state.clone()).oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

const JSON: Option<&str> = Some("application/json");

#[tokio::test]
async fn 실제_db로_전달_태그와_예약과_전달_서비스를_확인한다() {
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

    let fake = FakeRuntime { pool: pool.clone(), state: Arc::default() };
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let collected = Arc::clone(&events);
    let service = ReplyDeliveryService::new(
        pool.clone(),
        fake.clone(),
        Arc::new(move |event| collected.lock().unwrap().push(event)),
    );

    // 여러 수신자는 새로 읽어도 남고, 자기 자신·없는 직원은 거절한다. 같은 완료를 두 번 기록해도 예약이 늘지 않는다.
    reset(&pool, &fake).await;
    assert_eq!(
        setting(&pool, "a", &["b", "c", "b"], false).await.unwrap(),
        json!({ "characterId": "a", "recipientIds": ["b", "c"], "paused": false })
    );
    let routes = read_reply_routes(&pool).await.unwrap();
    assert_eq!(routes[0], json!({ "characterId": "a", "paused": false, "recipientIds": ["b", "c"] }));
    assert_eq!(routes[1], json!({ "characterId": "b", "paused": false, "recipientIds": [] }));
    for (ids, message) in [
        (vec!["a"], "다른 직원을 선택하고 일시정지 상태를 지정하세요."),
        (vec!["missing"], "전달 설정의 직원을 찾을 수 없습니다."),
    ] {
        match setting(&pool, "a", &ids, false).await {
            Err(RouteSaveError::Invalid(actual)) => assert_eq!(actual, message),
            other => panic!("{ids:?}: {other:?}"),
        }
    }
    assert_eq!(read_reply_routes(&pool).await.unwrap()[0]["recipientIds"], json!(["b", "c"]), "거절된 저장은 되돌린다");
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    assert_eq!(record(&pool, &turn, "a").await, ["b", "c"]);
    assert_eq!(record(&pool, &turn, "a").await, Vec::<String>::new());
    assert_eq!(queued(&pool, &turn).await.len(), 2, "반복 완료가 예약을 늘리면 안 된다");

    // 저장된 태그만 예약을 만든다. 받은 턴은 받는 직원의 태그가 없으면 다시 전달하지 않는다.
    reset(&pool, &fake).await;
    let untagged = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    assert!(record(&pool, &untagged, "a").await.is_empty());
    setting(&pool, "a", &["c"], false).await.unwrap();
    let tagged = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    assert_eq!(record(&pool, &tagged, "a").await, ["c"]);
    service.tick().await.unwrap();
    assert_eq!(fake.sent_to(), ["c"]);
    assert!(queued(&pool, &fake.sent()[0].1).await.is_empty());

    // 예전 일회성 예약도 현재 태그가 있어야 하고, 일시정지·해제를 따른다.
    reset(&pool, &fake).await;
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    legacy_reservation(&pool, &turn, "b").await;
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(
        queued(&pool, &turn).await,
        [("b".into(), "cancelled".into(), Some("선택된 자동 전달 태그가 없어 취소되었습니다.".into()))]
    );
    setting(&pool, "a", &["b"], true).await.unwrap();
    let paused = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    legacy_reservation(&pool, &paused, "b").await;
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(statuses(&queued(&pool, &paused).await), ["pending"]);
    setting(&pool, "a", &["b"], false).await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 1);
    let removed = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    legacy_reservation(&pool, &removed, "b").await;
    setting(&pool, "a", &[], false).await.unwrap();
    assert_eq!(
        queued(&pool, &removed).await,
        [("b".into(), "cancelled".into(), Some("전달 대상에서 해제되었습니다.".into()))]
    );

    // 조회와 확정 사이에 태그를 지우면 보내지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    legacy_reservation(&pool, &turn, "b").await;
    *fake.state.before_availability.lock().unwrap() = Some(("a".into(), vec![], false));
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(statuses(&queued(&pool, &turn).await), ["cancelled"]);

    // 바쁜 수신자는 다른 수신자를 막지 않는다. 받은 턴은 자기 태그로 예약하고, 진행 중인 답변은 넘기지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b", "c"], false).await.unwrap();
    setting(&pool, "b", &["a"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    fake.busy(&["b"]);
    service.tick().await.unwrap();
    assert_eq!(fake.sent_to(), ["c"]);
    fake.busy(&[]);
    service.tick().await.unwrap();
    assert_eq!(fake.sent_to(), ["c", "b"]);
    let b_turn = fake.sent()[1].1.clone();
    // 가짜 실행기의 시작이 recordReplyDelivery로 받은 턴의 예약을 받는 직원 자신의 태그로 만든다.
    let b_queue = queued(&pool, &b_turn).await;
    assert_eq!(b_queue.iter().map(|(recipient, _, _)| recipient.as_str()).collect::<Vec<_>>(), ["a"]);
    assert!(queued(&pool, &fake.sent()[0].1).await.is_empty());
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 2, "진행 중인 답변은 넘기지 않는다");
    sqlx::query("UPDATE turns SET status='completed' WHERE id=$1::uuid").bind(&b_turn).execute(&pool).await.unwrap();
    service.tick().await.unwrap();
    let last = fake.sent().last().unwrap().0.clone();
    assert_eq!((last.character_id.as_str(), last.sender_character_id.as_str()), ("a", "b"));
    assert_eq!(last.prompt, "받은 답변");
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 3, "확정된 전달은 다시 보내지 않는다");

    // 일시정지는 다시 읽어도 남고, 재개하면 수신자를 지우지 않고 대기 예약을 보낸다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b", "c"], true).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(queued(&pool, &turn).await.len(), 2);
    assert_eq!(read_reply_routes(&pool).await.unwrap()[0]["paused"], json!(true));
    setting(&pool, "a", &["b", "c"], false).await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 2);

    // 수신자 하나를 빼면 그 대기 예약만 취소한다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b", "c"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(fake.sent_to(), ["b"]);
    assert_eq!(statuses(&queued(&pool, &turn).await), ["delivered", "cancelled"]);

    // 조회와 확정 사이에 일시정지하면 보내지 않고 대기로 남긴다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    *fake.state.before_availability.lock().unwrap() = Some(("a".into(), vec!["b".into()], true));
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(statuses(&queued(&pool, &turn).await), ["pending"]);

    // 막힌 예약이 30개를 넘어도 다른 수신자는 굶지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    for _ in 0..31 {
        let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
        record(&pool, &turn, "a").await;
    }
    setting(&pool, "a", &["b", "c"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    fake.busy(&["b"]);
    service.tick().await.unwrap();
    assert_eq!(fake.sent_to(), ["c"]);

    // 마지막 답변 원문과 실제 보낸 직원, 받는 직원의 활성 대화를 그대로 넘기고 한 번만 보낸다. 잠금은 매번 푼다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "첫 답변").await;
    sqlx::query("INSERT INTO messages (turn_id, role, text, received_at) VALUES ($1::uuid, 'assistant', $2, now() + interval '1 second')")
        .bind(&turn)
        .bind("  오늘은 여기까지 이야기하자.\n")
        .execute(&pool)
        .await
        .unwrap();
    let receiving = create_turn(&pool, "b", "completed", "받는 직원의 지난 답변").await;
    let conversation: String = sqlx::query_scalar(
        "WITH session AS (SELECT cli_session_id AS id FROM turns WHERE id=$1::uuid)
         INSERT INTO active_cli_sessions (character_id, cli_session_id) SELECT 'b', id FROM session
         RETURNING (SELECT conversation_id::text FROM cli_sessions WHERE id=cli_session_id)",
    )
    .bind(&receiving)
    .fetch_one(&pool)
    .await
    .unwrap();
    record(&pool, &turn, "a").await;
    let delivery: String = sqlx::query_scalar("SELECT id::text FROM reply_deliveries WHERE source_turn_id=$1::uuid")
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();
    events.lock().unwrap().clear();
    service.tick().await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(
        fake.sent()[0].0,
        DeliveryStart {
            character_id: "b".into(),
            prompt: "  오늘은 여기까지 이야기하자.\n".into(),
            conversation_id: Some(conversation),
            sender_character_id: "a".into(),
            delivery_id: delivery.clone(),
        }
    );
    assert_eq!(statuses(&queued(&pool, &turn).await), ["delivered"]);
    let changed = json!({ "type": "feed.changed", "turnId": turn, "characterId": "a" });
    assert_eq!(*events.lock().unwrap(), [changed.clone(), changed], "sending 갱신과 전달 뒤에 한 번씩 알린다");
    assert_eq!(advisory_locks(&pool).await, 0);

    // 보낸 직원이나 받는 직원이 바쁘면 예약을 대기로 두고, 풀리면 보낸다.
    for busy in ["a", "b"] {
        reset(&pool, &fake).await;
        setting(&pool, "a", &["b"], false).await.unwrap();
        let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
        record(&pool, &turn, "a").await;
        fake.busy(&[busy]);
        service.tick().await.unwrap();
        assert!(fake.sent().is_empty(), "{busy}");
        assert_eq!(statuses(&queued(&pool, &turn).await), ["pending"], "{busy}");
        fake.busy(&[]);
        service.tick().await.unwrap();
        assert_eq!(fake.sent().len(), 1, "{busy}");
    }

    // 중단·실패·빈 답변·사용자 확인 답변은 다른 직원에게 넣지 않는다.
    for (status, needs_input, text, message) in [
        ("interrupted", false, "답변", "완료된 답변이 없어 전달하지 않았습니다."),
        ("failed", false, "답변", "완료된 답변이 없어 전달하지 않았습니다."),
        ("completed", true, "답변", "사용자 확인이 필요한 답변은 자동 전달하지 않습니다."),
        ("completed", false, " \u{3000}\n", "완료된 답변이 없어 전달하지 않았습니다."),
    ] {
        reset(&pool, &fake).await;
        setting(&pool, "a", &["b"], false).await.unwrap();
        let turn = create_turn(&pool, "a", status, text).await;
        sqlx::query("UPDATE turns SET needs_input=$2 WHERE id=$1::uuid")
            .bind(&turn)
            .bind(needs_input)
            .execute(&pool)
            .await
            .unwrap();
        record(&pool, &turn, "a").await;
        service.tick().await.unwrap();
        assert!(fake.sent().is_empty(), "{status} {needs_input}");
        assert_eq!(queued(&pool, &turn).await, [("b".into(), "cancelled".into(), Some(message.into()))]);
    }

    // 재시작 때 sending은 결과를 모르므로 uncertain으로 두고 다시 보내지 않는다. 늦은 확정은 받아들인다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    sqlx::query("UPDATE reply_deliveries SET status='sending' WHERE source_turn_id=$1::uuid")
        .bind(&turn)
        .execute(&pool)
        .await
        .unwrap();
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    let rows = queued(&pool, &turn).await;
    assert_eq!(statuses(&rows), ["uncertain"]);
    assert_eq!(
        rows[0].2.as_deref(),
        Some("전달 도중 연결이 종료되어 접수 여부를 확인해야 합니다. 중복 방지를 위해 자동 재전송하지 않았습니다.")
    );
    assert!(cancel_pending_reply_delivery(&pool, &turn).await.unwrap() == false, "uncertain은 사용자가 취소할 대기가 아니다");
    let late = create_turn(&pool, "b", "running", "늦은 답").await;
    let delivery: String = sqlx::query_scalar("SELECT id::text FROM reply_deliveries WHERE source_turn_id=$1::uuid")
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    record_reply_delivery(&mut connection, &late, "b", Some(&delivery)).await.unwrap();
    drop(connection);
    assert_eq!(statuses(&queued(&pool, &turn).await), ["delivered"]);

    // 받는 턴이 접수를 확정한 뒤 실행이 실패해도 전달은 확정이고 다시 보내지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    *fake.state.fail_after_accept.lock().unwrap() = Some("runner failed after committed acceptance".into());
    service.tick().await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(statuses(&queued(&pool, &turn).await), ["delivered"]);

    // 바쁨 경쟁은 대기로 되돌려 다시 시도하고, 그 밖의 실패는 문구를 남기고 반복하지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    for start_error in [AgentError::Busy("busy".into()), AgentError::Draining("draining".into())] {
        *fake.state.start_error.lock().unwrap() = Some(start_error);
        service.tick().await.unwrap();
        assert_eq!(queued(&pool, &turn).await, [("b".into(), "pending".into(), None)]);
    }
    *fake.state.start_error.lock().unwrap() = Some(AgentError::Failed("CLI missing".into()));
    service.tick().await.unwrap();
    assert_eq!(queued(&pool, &turn).await, [("b".into(), "failed".into(), Some("CLI missing".into()))]);
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());

    // 동시에 부른 확인은 한 번의 전달을 함께 기다리고, 잠금을 남기지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    let (first, second, third) = tokio::join!(service.tick(), service.tick(), service.tick());
    assert!(first.is_ok() && second.is_ok() && third.is_ok());
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(fake.state.availability_calls.load(Ordering::SeqCst), 1, "drain은 한 번만 돈다");
    assert_eq!(advisory_locks(&pool).await, 0);

    // 다른 백엔드가 잠금을 쥐고 있으면 보내지 않는다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(hashtext('officestra:reply-delivery'))").execute(&mut *holder).await.unwrap();
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(statuses(&queued(&pool, &turn).await), ["pending"]);
    sqlx::query("SELECT pg_advisory_unlock(hashtext('officestra:reply-delivery'))").execute(&mut *holder).await.unwrap();
    drop(holder);
    service.tick().await.unwrap();
    assert_eq!(fake.sent().len(), 1);

    // 사용자는 두 직원을 멈추지 않고 대기 예약을 취소할 수 있다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    assert!(cancel_pending_reply_delivery(&pool, &turn).await.unwrap());
    service.tick().await.unwrap();
    assert!(fake.sent().is_empty());
    assert_eq!(
        queued(&pool, &turn).await,
        [("b".into(), "cancelled".into(), Some("사용자가 전달 예약을 취소했습니다.".into()))]
    );
    assert!(!cancel_pending_reply_delivery(&pool, &turn).await.unwrap());

    // 중복 확정이나 다른 직원의 확정은 실패한다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    let delivery: String = sqlx::query_scalar("SELECT id::text FROM reply_deliveries WHERE source_turn_id=$1::uuid")
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();
    let wrong = create_turn(&pool, "a", "running", "답").await;
    let mut connection = pool.acquire().await.unwrap();
    let claimed = record_reply_delivery(&mut connection, &wrong, "a", Some(&delivery)).await;
    assert!(matches!(claimed, Err(ReplyDeliveryError::AlreadyDelivered)));
    drop(connection);
    service.tick().await.unwrap();
    let duplicate = create_turn(&pool, "b", "running", "답").await;
    let mut connection = pool.acquire().await.unwrap();
    let claimed = record_reply_delivery(&mut connection, &duplicate, "b", Some(&delivery)).await;
    assert_eq!(claimed.unwrap_err().to_string(), "이미 전달되었거나 취소된 답변입니다.");
    drop(connection);

    // 드레인 중이거나 멈춘 서비스는 확인하지 않는다. start()는 주기마다 확인한다.
    reset(&pool, &fake).await;
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    fake.state.draining.store(true, Ordering::SeqCst);
    service.tick().await.unwrap();
    assert_eq!(fake.state.availability_calls.load(Ordering::SeqCst), 0);
    fake.state.draining.store(false, Ordering::SeqCst);
    let periodic =
        ReplyDeliveryService::with_interval(pool.clone(), fake.clone(), Arc::new(|_| {}), Duration::from_millis(30));
    periodic.start();
    for _ in 0..100 {
        if !fake.sent().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    periodic.stop().await.unwrap();
    assert_eq!(fake.sent().len(), 1, "주기 확인이 대기 예약을 보낸다");
    let calls = fake.state.availability_calls.load(Ordering::SeqCst);
    let second = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &second, "a").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    periodic.tick().await.unwrap();
    assert_eq!(fake.state.availability_calls.load(Ordering::SeqCst), calls, "멈춘 뒤에는 확인하지 않는다");
    assert_eq!(statuses(&queued(&pool, &second).await), ["pending"]);
    service.stop().await.unwrap();

    // HTTP 경로. Node와 같은 상태 코드·문구·키 순서다.
    reset(&pool, &fake).await;
    let state = app_state(pool.clone());
    let (status, body) = call(&state, "GET", "/api/reply-routes", None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::to_string(&body["routes"][0]).unwrap(), r#"{"characterId":"a","paused":false,"recipientIds":[]}"#);
    let (status, body) = call(&state, "PUT", "/api/reply-routes/a", JSON, r#"{"recipientIds":["c","b","c"],"paused":true}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::to_string(&body).unwrap(), r#"{"characterId":"a","recipientIds":["c","b"],"paused":true}"#);
    let (_, body) = call(&state, "GET", "/api/reply-routes", None, "").await;
    assert_eq!(body["routes"][0], json!({ "characterId": "a", "paused": true, "recipientIds": ["b", "c"] }));
    for (path, content_type, request_body, expected_status, message) in [
        ("/api/reply-routes/a", JSON, "null", 400, "Cannot destructure property 'recipientIds' of 'object null' as it is null."),
        ("/api/reply-routes/a", JSON, "", 400, "다른 직원을 선택하고 일시정지 상태를 지정하세요."),
        ("/api/reply-routes/a", JSON, r#"{"recipientIds":["b"],"paused":"no"}"#, 400, "다른 직원을 선택하고 일시정지 상태를 지정하세요."),
        ("/api/reply-routes/a", JSON, r#"{"recipientIds":[1],"paused":true}"#, 400, "다른 직원을 선택하고 일시정지 상태를 지정하세요."),
        ("/api/reply-routes/missing", JSON, r#"{"recipientIds":[],"paused":true}"#, 400, "전달 설정의 직원을 찾을 수 없습니다."),
        ("/api/reply-routes/%ZZ", JSON, "{", 500, "URI malformed"),
        ("/api/reply-routes/a", None, "{}", 415, "상태 변경 요청은 application/json 형식이어야 합니다."),
        ("/api/reply-deliveries/not-a-uuid/cancel", JSON, "{}", 400, "올바른 대화 ID가 필요합니다."),
        (
            "/api/reply-deliveries/11111111-1111-4111-8111-111111111111/cancel",
            JSON,
            "",
            409,
            "이미 전달을 시작했거나 종료된 예약입니다.",
        ),
    ] {
        let method = if path.contains("cancel") { "POST" } else { "PUT" };
        let (status, body) = call(&state, method, path, content_type, request_body).await;
        assert_eq!(status.as_u16(), expected_status, "{path} {request_body}");
        assert_eq!(body, json!({ "error": message }), "{path} {request_body}");
    }
    let (status, _) = call(&state, "GET", "/api/reply-routes/a", None, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    setting(&pool, "a", &["b"], false).await.unwrap();
    let turn = create_turn(&pool, "a", "completed", "자연스러운 답변").await;
    record(&pool, &turn, "a").await;
    let (status, body) = call(&state, "POST", &format!("/api/reply-deliveries/{turn}/cancel"), JSON, "{}").await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "cancelled": true })));
    assert_eq!(statuses(&queued(&pool, &turn).await), ["cancelled"]);
}
