// Slack 연결 시험. Node slack-bridge.test.mjs 이식, Web API 규칙, DB 함수, 가짜 Slack·가짜 백엔드 연결 시험을 둔다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;

use super::web::{RetryPolicy, SOCKET_RETRIES, TEN_RETRIES, WebClient, form_body, query_escape, retry_timeouts};
use super::*;

fn environment(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

fn temporary_directory() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("office-rs-slack-{}", crate::js::random_uuid()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn configuration_separates_allowed_users_and_teams() {
    let configuration = read_slack_configuration(
        &environment(&[
            ("SLACK_BOT_TOKEN", "xoxb-test"),
            ("SLACK_APP_TOKEN", "xapp-test"),
            ("OFFICE_SLACK_ALLOWED_USER_IDS", "U1, U2"),
            ("OFFICE_SLACK_ALLOWED_TEAM_IDS", "T1 T2"),
        ]),
        Some("/not-present/officestra-slack.env"),
    )
    .unwrap();
    assert_eq!(configuration.bot_token, "xoxb-test");
    assert_eq!(configuration.app_token, "xapp-test");
    assert_eq!(configuration.allowed_user_ids, ["U1", "U2"]);
    assert_eq!(configuration.allowed_team_ids, ["T1", "T2"]);
}

#[test]
fn environment_file_reads_comments_and_quotes() {
    let parsed = parse_environment_file(
        "\n      # local only\n      SLACK_BOT_TOKEN=\"xoxb-value\"\n      SLACK_APP_TOKEN='xapp-value'\n      invalid-line\n    ",
    );
    assert_eq!(
        parsed,
        [("SLACK_BOT_TOKEN".to_string(), "xoxb-value".to_string()), ("SLACK_APP_TOKEN".to_string(), "xapp-value".to_string())]
    );
    // CRLF, = 앞이 빈 줄, 짝이 안 맞는 따옴표, 같은 키 다시 쓰기, __proto__ 무시.
    let parsed = parse_environment_file("A=1\r\n=skip\r\nB=\"x'\r\nA = 2 \r\n__proto__=x\r\nC=\"\"");
    assert_eq!(
        parsed,
        [("A".to_string(), "2".to_string()), ("B".to_string(), "\"x'".to_string()), ("C".to_string(), String::new())]
    );
}

#[test]
fn configuration_file_values_yield_to_non_blank_environment() {
    let directory = temporary_directory();
    let file = directory.join("slack.env");
    std::fs::write(&file, "SLACK_BOT_TOKEN=' from-file '\nSLACK_APP_TOKEN=file-app\nOFFICE_SLACK_ALLOWED_USER_IDS=U9,,U9  U8\n").unwrap();
    let path = file.to_string_lossy().into_owned();
    let configuration = read_slack_configuration(&environment(&[("SLACK_APP_TOKEN", "  "), ("OFFICE_SLACK_ENV_FILE", &path)]), None).unwrap();
    assert_eq!(configuration.file_path, path);
    assert_eq!(configuration.bot_token, "from-file");
    assert_eq!(configuration.app_token, "file-app");
    assert_eq!(configuration.allowed_user_ids, ["U9", "U8"]);
    assert!(configuration.allowed_team_ids.is_empty());
    let overridden = read_slack_configuration(&environment(&[("SLACK_APP_TOKEN", "env-app")]), Some(&path)).unwrap();
    assert_eq!(overridden.app_token, "env-app");
    // 빈 OFFICE_SLACK_ENV_FILE은 파일을 읽지 않는다.
    let empty = read_slack_configuration(&environment(&[("OFFICE_SLACK_ENV_FILE", "")]), None).unwrap();
    assert_eq!((empty.file_path.as_str(), empty.bot_token.as_str()), ("", ""));
    // 폴더를 가리키면 Node readFileSync처럼 실패한다.
    let error = read_slack_configuration(&[], Some(&directory.to_string_lossy())).err().unwrap();
    assert_eq!(error, "EISDIR: illegal operation on a directory, read");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn selection_buttons_follow_names_and_current_choice() {
    let blocks = employee_selection_blocks(
        &[json!({ "id": "boss", "name": "백부장" }), json!({ "id": "right-woman", "name": "코대리" })],
        Some(&json!("right-woman")),
    );
    let buttons = blocks[1]["elements"].as_array().unwrap();
    assert_eq!(buttons[0]["text"]["text"], "백부장");
    assert_eq!(buttons[1]["text"]["text"], "✓ 코대리");
    assert_eq!(buttons[1]["style"], "primary");
    assert_eq!(buttons[1]["value"], "right-woman");
    assert_ne!(buttons[0]["action_id"], buttons[1]["action_id"]);
    assert_eq!(
        serde_json::to_string(&buttons[0]).unwrap(),
        r#"{"type":"button","action_id":"officestra.select-character.boss","text":{"type":"plain_text","text":"백부장","emoji":true},"value":"boss"}"#
    );
    // 다섯 명까지만 버튼이 되고, 선택이 없으면 표시도 없다.
    let many: Vec<Value> = (0..7).map(|index| json!({ "id": format!("c{index}"), "name": index })).collect();
    let blocks = employee_selection_blocks(&many, None);
    let buttons = blocks[1]["elements"].as_array().unwrap();
    assert_eq!(buttons.len(), 5);
    assert!(buttons.iter().all(|button| button.get("style").is_none()));
    assert_eq!(buttons[4]["text"]["text"], "4");
}

#[test]
fn mentions_are_removed_from_prompt() {
    assert_eq!(strip_slack_mention(Some(&json!("<@UBOT>   상태를 확인해줘")), Some(&json!("UBOT"))), "상태를 확인해줘");
    assert_eq!(strip_slack_mention(Some(&json!("<@U1A> 안녕\n<@x> <@<@B2>끝")), None), "안녕 <@x> <@ 끝");
    assert_eq!(strip_slack_mention(Some(&json!("<@UBOT> <@UOTHER> 일")), Some(&json!("UBOT"))), "<@UOTHER> 일");
    assert_eq!(strip_slack_mention(None, None), "");
}

#[test]
fn progress_shows_recent_activities_only() {
    let text = render_slack_turn(&json!({
        "characterName": "코과장",
        "status": "running",
        "activities": [
            { "text": "첫 단계" }, { "text": "둘째 단계" }, { "text": "셋째 단계" },
            { "text": "넷째 단계" }, { "text": "다섯째 단계" }, { "text": "   " },
        ],
        "response": "응답 작성 중",
    }));
    assert_eq!(text, "⏳ 작업 중 *코과장*\n• 둘째 단계\n• 셋째 단계\n• 넷째 단계\n• 다섯째 단계\n\n\n응답 작성 중");
}

#[test]
fn slack_shows_turn_status_not_workspace_metadata() {
    let completed = render_slack_turn(&json!({
        "characterName": "백부장",
        "status": "completed",
        "workspace": { "status": "awaiting_approval" },
        "response": "끝난 응답은 진행 메시지에 넣지 않는다",
    }));
    assert_eq!(completed, "✅ 완료 *백부장*");
    let labels: Vec<String> = [
        json!({ "status": "completed", "needsInput": true }),
        json!({ "status": "failed", "errorMessage": "시간 초과" }),
        json!({ "status": "failed", "errorMessage": "" }),
        json!({ "status": "interrupted" }),
        json!({ "status": "starting", "characterName": "<a&b>" }),
        json!({ "status": "queued", "characterName": null }),
    ]
    .iter()
    .map(render_slack_turn)
    .collect();
    assert_eq!(labels, ["❓ 답변 필요 *직원*", "❌ 실패 · 시간 초과 *직원*", "❌ 실패 *직원*", "⏹️ 중단 *직원*", "⏳ 업무 시작 *&lt;a&amp;b&gt;*", "⏳ 작업 중 *직원*"]);
    // 진행 중 응답은 끝 1,200자, 전체는 2,700자까지다.
    let long = render_slack_turn(&json!({ "status": "running", "response": format!("앞{}", "가".repeat(1_200)) }));
    assert_eq!(long, format!("⏳ 작업 중 *직원*\n\n\n{}", "가".repeat(1_200)));
    let activities: Vec<Value> = (0..4).map(|_| json!({ "text": "나".repeat(800) })).collect();
    let capped = render_slack_turn(&json!({ "status": "running", "activities": activities }));
    assert_eq!(capped.encode_utf16().count(), 2_700);
}

#[test]
fn long_final_answers_split_without_loss() {
    let chunks = split_slack_message("첫째 줄\n둘째 줄\n셋째 줄", 10);
    assert!(chunks.len() > 1);
    assert_eq!(chunks.join("\n"), "첫째 줄\n둘째 줄\n셋째 줄");
    // 줄바꿈이 한도의 60% 앞이면 한도에서 자른다.
    assert_eq!(split_slack_message("ab\ncdefghijkl", 10), ["ab\ncdefghi", "jkl"]);
    assert_eq!(split_slack_message("abcdefg\nhij", 10), ["abcdefg", "hij"]);
    assert!(split_slack_message(" \n ", 10).is_empty());
}

#[test]
fn markdown_headings_and_bold_become_slack_bold() {
    assert_eq!(normalize_slack_markdown("## 결과\n본문 **굵게** 와 **둘**\n####### 일곱"), "*결과*\n본문 *굵게* 와 *둘*\n####### 일곱");
}

#[test]
fn team_and_authorization_follow_node_order() {
    assert_eq!(slack_team_id(&json!({ "team_id": "T1", "team": { "id": "T2" } })), "T1");
    assert_eq!(slack_team_id(&json!({ "team_id": null, "team": { "id": "T2" } })), "T2");
    assert_eq!(slack_team_id(&json!({ "authorizations": [{ "team_id": "T3" }] })), "T3");
    assert_eq!(slack_team_id(&json!({ "team_id": false })), false);
    assert_eq!(slack_team_id(&json!({})), "");
    let configuration = read_slack_configuration(
        &environment(&[("OFFICE_SLACK_ALLOWED_USER_IDS", "U1"), ("OFFICE_SLACK_ALLOWED_TEAM_IDS", "T1")]),
        Some(""),
    )
    .unwrap();
    assert!(is_authorized_slack_request(&configuration, &json!({ "user_id": "U1", "team_id": "T1" })));
    assert!(is_authorized_slack_request(&configuration, &json!({ "user": { "id": "U1" }, "team": { "id": "T1" } })));
    assert!(is_authorized_slack_request(&configuration, &json!({ "event": { "user": "U1" }, "team_id": "T1" })));
    assert!(!is_authorized_slack_request(&configuration, &json!({ "user_id": "U1", "team_id": "T2" })));
    assert!(!is_authorized_slack_request(&configuration, &json!({ "user_id": "U2", "team_id": "T1" })));
    assert!(!is_authorized_slack_request(&configuration, &json!({ "team_id": "T1" })));
}

#[test]
fn incoming_types_and_sources_follow_bolt() {
    let kind = |body: Value| incoming_type(Some(&body));
    assert_eq!(kind(json!({ "event": { "type": "app_mention" } })), Ok(Some(Incoming::Event)));
    assert_eq!(kind(json!({ "command": "/office" })), Ok(Some(Incoming::Command)));
    assert_eq!(kind(json!({ "type": "block_actions", "actions": [] })), Ok(Some(Incoming::Action)));
    assert_eq!(kind(json!({ "type": "block_suggestion" })), Ok(Some(Incoming::Options)));
    assert_eq!(kind(json!({ "type": "shortcut" })), Ok(Some(Incoming::Shortcut)));
    assert_eq!(kind(json!({ "type": "view_closed" })), Ok(Some(Incoming::ViewAction)));
    assert_eq!(kind(json!({ "type": "something" })), Ok(None));
    assert_eq!(kind(json!("text")), Ok(None));
    assert_eq!(incoming_type(None).unwrap_err(), "TypeError: Cannot read properties of undefined (reading 'event')");
    assert_eq!(kind(json!({ "event": null })).unwrap_err(), "TypeError: Cannot use 'in' operator to search for 'channel' in null");
    assert_eq!(kind(json!({ "actions": [], "channel": null })).unwrap_err(), "TypeError: Cannot read properties of null (reading 'id')");

    let source = |kind: Incoming, body: Value| build_source(kind, &body);
    assert_eq!(
        source(Incoming::Event, json!({ "team_id": "T0", "authorizations": [{ "team_id": "T1", "enterprise_id": null }], "event": {} })),
        Ok(Source { team_id: Some(json!("T1")), enterprise_id: None })
    );
    assert_eq!(
        source(Incoming::Event, json!({ "team_id": "T0", "authorizations": [{ "team_id": null, "enterprise_id": "E1" }], "event": {} })),
        Ok(Source { team_id: Some(json!("T0")), enterprise_id: Some(json!("E1")) })
    );
    assert_eq!(source(Incoming::Event, json!({ "authorizations": [{}], "team_id": "T0", "event": {} })), Ok(Source { team_id: None, enterprise_id: None }));
    assert_eq!(
        source(Incoming::Action, json!({ "team": null, "user": { "id": "U1", "team_id": "T5" }, "enterprise": { "id": "E2" } })),
        Ok(Source { team_id: Some(json!("T5")), enterprise_id: Some(json!("E2")) })
    );
    assert_eq!(source(Incoming::Action, json!({ "team": { "id": "T6" } })).unwrap_err(), "TypeError: Cannot read properties of undefined (reading 'id')");
    assert_eq!(source(Incoming::Action, json!({})).unwrap_err(), "TypeError: Cannot read properties of undefined (reading 'team_id')");
    assert_eq!(source(Incoming::ViewAction, json!({})).unwrap_err(), "TypeError: Cannot read properties of undefined (reading 'app_installed_team_id')");
}

#[test]
fn ignore_self_skips_own_bot_and_user() {
    let auth = AuthResult { bot_user_id: Some(json!("UBOT")), bot_id: Some(json!("BBOT")) };
    assert!(ignored_self(Some(&auth), &json!({ "type": "message", "subtype": "bot_message", "bot_id": "BBOT" })));
    assert!(!ignored_self(Some(&auth), &json!({ "type": "message", "subtype": "bot_message", "bot_id": "BOTHER" })));
    assert!(ignored_self(Some(&auth), &json!({ "type": "app_mention", "user": "UBOT" })));
    assert!(!ignored_self(Some(&auth), &json!({ "type": "member_joined_channel", "user": "UBOT" })));
    assert!(!ignored_self(Some(&auth), &json!({ "type": "app_mention", "user": "U1" })));
    assert!(!ignored_self(None, &json!({ "type": "app_mention", "user": "UBOT" })));
}

#[test]
fn form_body_matches_node_querystring() {
    assert_eq!(query_escape("a b+c/가!*'()~-_."), "a%20b%2Bc%2F%EA%B0%80!*'()~-_.");
    let body = json!({ "team_id": null, "channel": "C1", "n": 1.5, "flag": true, "blocks": [{ "a": 1 }], "text": "줄\n바꿈" });
    assert_eq!(
        form_body(body.as_object().unwrap()),
        "channel=C1&n=1.5&flag=true&blocks=%5B%7B%22a%22%3A1%7D%5D&text=%EC%A4%84%0A%EB%B0%94%EA%BF%88"
    );
}

#[test]
fn retry_waits_follow_p_retry() {
    let socket = retry_timeouts(SOCKET_RETRIES, Duration::from_millis(1_000));
    assert_eq!(socket.len(), 100);
    assert_eq!(&socket[..3], [Duration::from_millis(1_000), Duration::from_millis(1_300), Duration::from_millis(1_690)]);
    let ten = retry_timeouts(TEN_RETRIES, Duration::from_millis(1_000));
    assert_eq!(ten.len(), 10);
    assert!(ten.windows(2).all(|pair| pair[0] <= pair[1]));
    let ceiling = Duration::from_secs_f64(2.0 * 1.96821f64.powi(9)) + Duration::from_millis(1);
    assert!(ten[0] >= Duration::from_millis(1_000) && ten[9] <= ceiling);
}

fn slack_http() -> reqwest::Client {
    crate::http_client_builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

async fn serve(listener: tokio::net::TcpListener, router: Router) {
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
}

#[tokio::test]
async fn web_client_handles_errors_like_node() {
    let counts: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let handler = {
        let counts = Arc::clone(&counts);
        move |Path(method): Path<String>| {
            let counts = Arc::clone(&counts);
            async move {
                let count = {
                    let mut counts = counts.lock().unwrap();
                    let count = counts.entry(method.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                match method.as_str() {
                    "error" => Json(json!({ "ok": false, "error": "channel_not_found" })).into_response(),
                    "text" => "not json".into_response(),
                    "ratelimited" if count == 1 => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "").into_response(),
                    "badheader" => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "soon")], "").into_response(),
                    "redirect" => (StatusCode::FOUND, [("location", "/api/error")], "").into_response(),
                    "fail" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    _ => Json(json!({ "ok": true, "method": method })).into_response(),
                }
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    serve(listener, Router::new().route("/api/{method}", post(handler))).await;
    let policy = RetryPolicy { retries: 2, factor: 1.0, randomize: false };
    let client = WebClient::new(slack_http(), &format!("http://{address}/api"), "xoxb-test", None, policy, Duration::from_millis(1));
    let message = |result: Result<Value, SlackError>| result.unwrap_err().message();
    assert_eq!(message(client.api_call("error", json!({})).await), "An API error occurred: channel_not_found");
    assert_eq!(message(client.api_call("text", json!({})).await), "An API error occurred: not json");
    assert_eq!(client.api_call("ratelimited", json!({})).await.unwrap()["ok"], true);
    assert_eq!(
        message(client.api_call("badheader", json!({})).await),
        format!("Retry header did not contain a valid timeout (url: http://{address}/api/badheader, retry-after header: soon)")
    );
    assert_eq!(message(client.api_call("redirect", json!({})).await), "A request error occurred: fetch failed");
    assert_eq!(message(client.api_call("fail", json!({})).await), "An HTTP protocol error occurred: statusCode = 500");
    let counts = counts.lock().unwrap().clone();
    assert_eq!(
        (counts["error"], counts["text"], counts["ratelimited"], counts["badheader"], counts["redirect"], counts["fail"]),
        (1, 1, 2, 1, 3, 3)
    );
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
/// slack_* 표를 쓰는 시험은 하나씩 돈다. 연결 시험의 모니터 복구가 다른 시험의 미전달 행을 집어 가지 않게 한다.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn database() -> Option<PgPool> {
    let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR")) else {
        eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR가 없어 Slack DB 시험을 건너뜀");
        return None;
    };
    let pool = crate::db::connect(&url).await.unwrap();
    MIGRATED
        .get_or_init(|| async {
            crate::migrate::run(&pool, std::path::Path::new(&migrations)).await.unwrap();
        })
        .await;
    Some(pool)
}

/// 시험이 만든 행을 모두 지운다. 팀은 TSLACKTEST, 이벤트는 EvSLACKTEST, 직원은 slack-test- 로 시작한다.
async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM slack_threads WHERE team_id LIKE 'TSLACKTEST%'",
        "DELETE FROM slack_user_preferences WHERE team_id LIKE 'TSLACKTEST%'",
        "DELETE FROM slack_event_receipts WHERE event_id LIKE 'EvSLACKTEST%'",
        "DELETE FROM turns WHERE cli_session_id IN (SELECT id FROM cli_sessions WHERE character_id LIKE 'slack-test-%')",
        "DELETE FROM cli_sessions WHERE character_id LIKE 'slack-test-%'",
        "DELETE FROM conversations WHERE title = 'Slack 시험'",
        "DELETE FROM characters WHERE id LIKE 'slack-test-%'",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

struct Fixture {
    pool: PgPool,
    team: String,
    character: String,
    suffix: String,
}

async fn fixture(pool: PgPool) -> Fixture {
    purge(&pool).await;
    let suffix = crate::js::random_uuid()[..8].to_string();
    let character = format!("slack-test-{suffix}");
    sqlx::query("INSERT INTO characters (id, name, seat, backend, permission, config) VALUES ($1, '슬랙<시험>', '상단', 'claude', 'acceptEdits', '{}')")
        .bind(&character)
        .execute(&pool)
        .await
        .unwrap();
    Fixture { pool, team: format!("TSLACKTEST{suffix}"), character, suffix }
}

/// conversations → cli_sessions → turns. 대화 ID를 주면 그 ID로 대화를 만든다. (대화 ID, 턴 ID)를 돌려준다.
async fn create_turn(pool: &PgPool, character: &str, conversation_id: Option<&str>) -> (String, String) {
    sqlx::query_as(
        "WITH conversation AS (
             INSERT INTO conversations (id, title, workdir) VALUES (COALESCE($1::uuid, gen_random_uuid()), 'Slack 시험', '/tmp') RETURNING id),
         session AS (INSERT INTO cli_sessions (conversation_id, character_id) SELECT id, $2 FROM conversation RETURNING id, conversation_id),
         turn AS (INSERT INTO turns (cli_session_id, prompt, status) SELECT id, 'Slack 시험', 'running' FROM session RETURNING id)
         SELECT session.conversation_id::text, turn.id::text FROM turn, session",
    )
    .bind(conversation_id)
    .bind(character)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn preference_is_scoped_to_team() {
    let Some(pool) = database().await else { return };
    let _serial = SERIAL.lock().await;
    let fixture = fixture(pool.clone()).await;
    let (team, user, character) = (json!(fixture.team), json!("U1"), json!(fixture.character));
    assert_eq!(slack_preference(&pool, &team, Some(&user)).await, Ok(None));
    save_slack_preference(&pool, &team, Some(&user), Some(&character)).await.unwrap();
    save_slack_preference(&pool, &team, Some(&user), Some(&character)).await.unwrap();
    assert_eq!(slack_preference(&pool, &team, Some(&user)).await, Ok(Some(fixture.character.clone())));
    assert_eq!(slack_preference(&pool, &json!(format!("{}-other", fixture.team)), Some(&user)).await, Ok(None));
    assert_eq!(slack_preference(&pool, &team, None).await, Ok(None));
    let error = save_slack_preference(&pool, &team, None, Some(&character)).await.unwrap_err();
    assert!(error.contains("\"user_id\""), "{error}");
    purge(&fixture.pool).await;
}

#[tokio::test]
async fn turn_launch_saves_backend_conversation_and_restore_targets() {
    let Some(pool) = database().await else { return };
    let _serial = SERIAL.lock().await;
    let fixture = fixture(pool.clone()).await;
    sqlx::query("INSERT INTO slack_threads (team_id, channel_id, thread_ts, user_id, character_id) VALUES ($1, 'C1', '100.1', 'U1', $2)")
        .bind(&fixture.team)
        .bind(&fixture.character)
        .execute(&pool)
        .await
        .unwrap();
    let (conversation_id, turn_id) = create_turn(&pool, &fixture.character, None).await;
    let (team, channel, thread) = (json!(fixture.team), json!("C1"), json!("100.1"));
    let (conversation, turn, ts, blank, invalid, other_thread) =
        (json!(conversation_id), json!(turn_id), json!("100.2"), json!(""), json!("not-a-uuid"), json!("999.9"));
    let launch = |conversation_id, turn_id, status_message_ts| LaunchRecord {
        team_id: &team,
        channel_id: &channel,
        thread_ts: &thread,
        conversation_id,
        turn_id,
        status_message_ts,
    };
    let incomplete = "Slack 업무 연결 정보가 완전하지 않습니다.".to_string();
    assert_eq!(record_slack_turn_launch(&pool, &launch(Some(&conversation), Some(&turn), None)).await, Err(incomplete.clone()));
    assert_eq!(record_slack_turn_launch(&pool, &launch(Some(&conversation), Some(&blank), Some(&ts))).await, Err(incomplete));
    assert_eq!(
        record_slack_turn_launch(&pool, &launch(Some(&invalid), Some(&turn), Some(&ts))).await,
        Err("invalid input syntax for type uuid: \"not-a-uuid\"".into())
    );
    let missing = LaunchRecord { thread_ts: &other_thread, ..launch(Some(&conversation), Some(&turn), Some(&ts)) };
    assert_eq!(record_slack_turn_launch(&pool, &missing).await, Err("Slack 스레드 연결 정보를 찾을 수 없습니다.".into()));
    assert_eq!(record_slack_turn_launch(&pool, &launch(Some(&conversation), Some(&turn), Some(&ts))).await, Ok(conversation_id.clone()));
    let target = PendingTarget { channel_id: "C1".into(), thread_ts: "100.1".into(), message_ts: "100.2".into(), turn_id: turn_id.clone() };
    assert_eq!(pending_slack_turn_targets(&pool).await, Ok(vec![target.clone()]));
    mark_slack_turn_delivery_completed(&pool, &turn).await.unwrap();
    assert_eq!(pending_slack_turn_targets(&pool).await, Ok(vec![]));
    // 같은 스레드에서 다시 시작하면 배달 완료 표시를 지운다.
    record_slack_turn_launch(&pool, &launch(Some(&conversation), Some(&turn), Some(&ts))).await.unwrap();
    assert_eq!(pending_slack_turn_targets(&pool).await, Ok(vec![target]));
    purge(&fixture.pool).await;
}

#[tokio::test]
async fn event_receipt_is_claimed_once() {
    let Some(pool) = database().await else { return };
    let _serial = SERIAL.lock().await;
    let fixture = fixture(pool.clone()).await;
    let event = json!(format!("EvSLACKTEST{}", fixture.suffix));
    assert_eq!(claim_slack_event(&pool, &event).await, Ok(true));
    assert_eq!(claim_slack_event(&pool, &event).await, Ok(false));
    purge(&fixture.pool).await;
}

/// 가짜 Slack이 받은 Web API 호출 하나.
#[derive(Clone, Debug)]
struct SlackCall {
    method: String,
    form: Vec<(String, String)>,
    authorization: Option<String>,
    raw: String,
}

impl SlackCall {
    fn field(&self, key: &str) -> Option<&str> {
        self.form.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.form.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn blocks(&self) -> Value {
        serde_json::from_str(self.field("blocks").unwrap()).unwrap()
    }
}

#[derive(Default)]
struct FakeSlackState {
    calls: Vec<SlackCall>,
    /// 연결마다 서버가 보낼 메시지 통로. 마지막이 지금 연결이다.
    sockets: Vec<mpsc::UnboundedSender<WsMessage>>,
    /// 클라이언트가 보낸 텍스트(ack).
    received: Vec<Value>,
}

/// Web API(/api/{method})와 소켓 모드(/socket)를 흉내 낸다. 실제 Slack에는 연결하지 않는다.
struct FakeSlack {
    address: std::net::SocketAddr,
    posted: AtomicUsize,
    state: Mutex<FakeSlackState>,
}

impl FakeSlack {
    async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Arc::new(Self { address: listener.local_addr().unwrap(), posted: AtomicUsize::new(0), state: Mutex::default() });
        let router = Router::new()
            .route("/api/{method}", post(fake_slack_api))
            .route("/socket", get(fake_slack_socket))
            .with_state(Arc::clone(&fake));
        serve(listener, router).await;
        fake
    }

    fn calls(&self) -> Vec<SlackCall> {
        self.state.lock().unwrap().calls.clone()
    }

    fn calls_to(&self, method: &str) -> Vec<SlackCall> {
        self.calls().into_iter().filter(|call| call.method == method).collect()
    }

    fn connections(&self) -> usize {
        self.state.lock().unwrap().sockets.len()
    }

    fn send(&self, message: Value) {
        let state = self.state.lock().unwrap();
        state.sockets.last().unwrap().send(WsMessage::Text(message.to_string().into())).unwrap();
    }

    fn acked(&self, envelope_id: &str) -> bool {
        let expected = json!({ "envelope_id": envelope_id, "payload": {} });
        self.state.lock().unwrap().received.iter().any(|message| *message == expected)
    }
}

async fn fake_slack_api(State(fake): State<Arc<FakeSlack>>, Path(method): Path<String>, headers: HeaderMap, raw: String) -> Json<Value> {
    let form = url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect();
    let authorization = headers.get("authorization").map(|value| value.to_str().unwrap().to_string());
    let reply = match method.as_str() {
        "auth.test" => json!({ "ok": true, "user_id": "UBOT", "bot_id": "BBOT" }),
        "apps.connections.open" => json!({ "ok": true, "url": format!("ws://{}/socket", fake.address) }),
        "chat.postMessage" => json!({ "ok": true, "ts": format!("200.{}", fake.posted.fetch_add(1, Ordering::SeqCst) + 1) }),
        "chat.update" => json!({ "ok": true }),
        _ => json!({ "ok": false, "error": "unknown_method" }),
    };
    fake.state.lock().unwrap().calls.push(SlackCall { method, form, authorization, raw });
    Json(reply)
}

async fn fake_slack_socket(State(fake): State<Arc<FakeSlack>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        let (sender, mut outgoing) = mpsc::unbounded_channel();
        fake.state.lock().unwrap().sockets.push(sender);
        let hello = json!({ "type": "hello", "num_connections": 1 }).to_string();
        if socket.send(WsMessage::Text(hello.into())).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                // ping에는 tungstenite가 읽는 동안 pong으로 답한다. 닫기 프레임도 읽기를 계속해야 답이 나간다.
                incoming = socket.recv() => match incoming {
                    Some(Ok(WsMessage::Text(text))) => fake.state.lock().unwrap().received.push(serde_json::from_str(text.as_str()).unwrap()),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                },
                Some(message) = outgoing.recv() => {
                    if socket.send(message).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

/// OFFICESTRA 백엔드 흉내. 직원 목록, 업무 접수(실제 턴 행 생성), 실시간 기록(404 → 작업 중 2번 → 완료)을 준다.
struct FakeBackend {
    address: std::net::SocketAddr,
    pool: PgPool,
    character: String,
    jobs: Mutex<Vec<Value>>,
    feeds: Mutex<HashMap<String, usize>>,
}

const FINAL_RESPONSE: &str = "## 결과\n**굵게** 끝";

fn live_turn(turn_id: &str, status: &str, response: &str) -> Value {
    json!({ "id": turn_id, "characterName": "슬랙<시험>", "status": status, "activities": [{ "text": "파일 읽기" }], "response": response })
}

impl FakeBackend {
    async fn start(pool: PgPool, character: String) -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Arc::new(Self {
            address: listener.local_addr().unwrap(),
            pool,
            character,
            jobs: Mutex::default(),
            feeds: Mutex::default(),
        });
        let characters = |State(fake): State<Arc<FakeBackend>>| async move {
            Json(json!({ "characters": [{ "id": fake.character, "name": "슬랙<시험>" }] }))
        };
        let job = |State(fake): State<Arc<FakeBackend>>, Json(body): Json<Value>| async move {
            fake.jobs.lock().unwrap().push(body.clone());
            let (conversation_id, turn_id) = create_turn(&fake.pool, &fake.character, body["conversationId"].as_str()).await;
            Json(json!({ "conversationId": conversation_id, "turnId": turn_id }))
        };
        let feed = |State(fake): State<Arc<FakeBackend>>, Path(turn_id): Path<String>| async move {
            let count = {
                let mut feeds = fake.feeds.lock().unwrap();
                let count = feeds.entry(turn_id.clone()).or_insert(0);
                *count += 1;
                *count
            };
            match count {
                1 => StatusCode::NOT_FOUND.into_response(),
                2 | 3 => Json(json!({ "turn": live_turn(&turn_id, "running", "중간") })).into_response(),
                _ => Json(json!({ "turn": live_turn(&turn_id, "completed", FINAL_RESPONSE) })).into_response(),
            }
        };
        let router = Router::new()
            .route("/api/characters", get(characters))
            .route("/api/agent-jobs", post(job))
            .route("/api/live-feed/{turn}", get(feed))
            .with_state(Arc::clone(&fake));
        serve(listener, router).await;
        fake
    }
}

/// 조건이 참이 될 때까지 20ms마다 확인한다. 10초가 지나면 실패한다.
async fn eventually<F, Fut>(label: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "{label} 기다리다 시간 초과");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn delivered(pool: &PgPool, team: &str, channel: &str, thread_ts: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT delivery_completed_at IS NOT NULL FROM slack_threads WHERE team_id = $1 AND channel_id = $2 AND thread_ts = $3",
    )
    .bind(team)
    .bind(channel)
    .bind(thread_ts)
    .fetch_optional(pool)
    .await
    .unwrap()
    .unwrap_or(false)
}

async fn receipt_exists(pool: &PgPool, event_id: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM slack_event_receipts WHERE event_id = $1)")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// 모니터가 보낸 상태 갱신 둘(작업 중·완료)과 최종 답 하나를 확인한다. team_id는 팀별 클라이언트일 때만 붙는다.
fn assert_monitor_calls(calls: &[SlackCall], team: Option<&str>, channel: &str, thread_ts: &str, message_ts: &str, turn_id: &str) {
    let prefix: Vec<&str> = team.map(|_| "team_id").into_iter().collect();
    let updates: Vec<&SlackCall> =
        calls.iter().filter(|call| call.method == "chat.update" && call.field("ts") == Some(message_ts)).collect();
    let running = live_turn(turn_id, "running", "중간");
    let completed = live_turn(turn_id, "completed", FINAL_RESPONSE);
    assert_eq!(updates.len(), 2, "같은 표시는 다시 보내지 않는다 {updates:?}");
    for (update, turn) in updates.iter().zip([&running, &completed]) {
        assert_eq!(update.keys(), [prefix.clone(), vec!["channel", "ts", "text", "blocks"]].concat());
        assert_eq!(update.field("team_id"), team);
        assert_eq!(update.field("channel"), Some(channel));
        assert_eq!(update.field("text"), Some(render_slack_turn(turn).as_str()));
        assert_eq!(update.blocks(), progress_blocks(turn));
    }
    let finals: Vec<&SlackCall> = calls
        .iter()
        .filter(|call| call.method == "chat.postMessage" && call.field("channel") == Some(channel) && call.field("thread_ts") == Some(thread_ts))
        .filter(|call| call.field("blocks").is_none())
        .collect();
    assert_eq!(finals.len(), 1, "{finals:?}");
    assert_eq!(finals[0].keys(), [prefix, vec!["channel", "thread_ts", "text"]].concat());
    assert_eq!(finals[0].field("text"), Some("*결과*\n*굵게* 끝"));
}

#[tokio::test]
async fn socket_mode_runs_commands_actions_and_turns_end_to_end() {
    let Some(pool) = database().await else { return };
    let _serial = SERIAL.lock().await;
    let fixture = fixture(pool.clone()).await;
    let (team, character) = (fixture.team.clone(), fixture.character.clone());
    // 재시작 전에 배달하지 못한 업무. 시작하면 app.client로 다시 지켜본다.
    let (_, restored_turn) = create_turn(&pool, &character, None).await;
    sqlx::query(
        "INSERT INTO slack_threads (team_id, channel_id, thread_ts, user_id, character_id, last_turn_id, status_message_ts)
         VALUES ($1, 'CRESTORE', '9.000', 'UALLOWED', $2, $3::uuid, '9.100')",
    )
    .bind(&team)
    .bind(&character)
    .bind(&restored_turn)
    .execute(&pool)
    .await
    .unwrap();
    let slack = FakeSlack::start().await;
    let backend = FakeBackend::start(pool.clone(), character.clone()).await;
    let mut options = SlackBridgeOptions::new(pool.clone(), format!("http://{}/", backend.address));
    options.environment = environment(&[
        ("SLACK_BOT_TOKEN", "xoxb-test-only"),
        ("SLACK_APP_TOKEN", "xapp-test-only"),
        ("OFFICE_SLACK_ALLOWED_USER_IDS", "UALLOWED"),
    ]);
    options.file_path = Some("/not-present/officestra-slack.env".into());
    options.api_url = format!("http://{}/api/", slack.address);
    options.timings = SlackTimings {
        poll_interval: Duration::from_millis(50),
        socket: SocketTimings { client_ping_timeout: Duration::from_millis(1_000), ..SocketTimings::default() },
        retry_min_timeout: Duration::from_millis(10),
    };
    let running = match start_slack_bridge(options).await {
        Ok(SlackBridgeStart::Enabled(running)) => running,
        Ok(SlackBridgeStart::Disabled(reason)) => panic!("Slack 연결이 꺼짐 {reason}"),
        Err(failure) => panic!("Slack 연결 실패 {}", failure.message),
    };

    // 시작: auth.test는 봇 토큰을 본문과 헤더에, apps.connections.open은 앱 토큰을 헤더에만 싣는다.
    eventually("auth.test", || async { !slack.calls_to("auth.test").is_empty() }).await;
    let auth = &slack.calls_to("auth.test")[0];
    assert_eq!(auth.form, [("token".to_string(), "xoxb-test-only".to_string())]);
    assert_eq!(auth.authorization.as_deref(), Some("Bearer xoxb-test-only"));
    let open = &slack.calls_to("apps.connections.open")[0];
    assert_eq!((open.raw.as_str(), open.authorization.as_deref()), ("", Some("Bearer xapp-test-only")));
    assert_eq!(slack.connections(), 1);

    // 복구한 모니터는 team_id 없는 app.client로 보낸다.
    eventually("복구한 업무 배달", || delivered(&pool, &team, "CRESTORE", "9.000")).await;
    assert_monitor_calls(&slack.calls(), None, "CRESTORE", "9.000", "9.100", &restored_turn);
    // 배달 표시 뒤에 목록에서 빠진다.
    eventually("복구 모니터 정리", || async { running.active_monitors() == 0 }).await;

    // 허용되지 않은 /office는 ack한 뒤 안내만 보낸다.
    let command = |envelope: &str, user: &str| {
        json!({ "envelope_id": envelope, "type": "slash_commands", "accepts_response_payload": true,
            "payload": { "team_id": team, "user_id": user, "command": "/office", "channel_id": "C1", "text": "" } })
    };
    let posts_before = slack.calls_to("chat.postMessage").len();
    slack.send(command("env-denied", "UOTHER"));
    eventually("권한 없음 안내", || async { slack.calls_to("chat.postMessage").len() > posts_before }).await;
    let denied = slack.calls_to("chat.postMessage").pop().unwrap();
    assert_eq!(
        denied.form,
        [("team_id", team.as_str()), ("channel", "C1"), ("text", "OFFICESTRA 사용 권한이 없습니다.")].map(|(k, v)| (k.to_string(), v.to_string()))
    );
    assert_eq!(denied.authorization.as_deref(), Some("Bearer xoxb-test-only"));
    assert!(slack.acked("env-denied"));

    // 허용된 /office는 직원 선택 버튼을 보낸다.
    let characters = [json!({ "id": character, "name": "슬랙<시험>" })];
    slack.send(command("env-office", "UALLOWED"));
    eventually("직원 선택", || async { slack.calls_to("chat.postMessage").len() > posts_before + 1 }).await;
    let selection = slack.calls_to("chat.postMessage").pop().unwrap();
    assert_eq!(selection.keys(), ["team_id", "channel", "text", "blocks"]);
    assert_eq!(selection.field("text"), Some("OFFICESTRA 직원을 선택하세요."));
    assert_eq!(selection.blocks(), employee_selection_blocks(&characters, None));
    let selection_ts = format!("200.{}", slack.posted.load(Ordering::SeqCst));
    assert!(slack.acked("env-office"));

    // 버튼을 누르면 선택을 저장하고 원래 메시지를 바꾼다.
    slack.send(json!({ "envelope_id": "env-action", "type": "interactive", "payload": {
        "type": "block_actions", "team": { "id": team }, "user": { "id": "UALLOWED", "team_id": team }, "channel": { "id": "C1" },
        "message": { "ts": selection_ts }, "actions": [{ "action_id": format!("{SELECT_ACTION_PREFIX}{character}"), "value": character }] } }));
    eventually("선택 저장", || async { slack.calls_to("chat.update").iter().any(|call| call.field("ts") == Some(selection_ts.as_str())) }).await;
    let chosen = slack.calls_to("chat.update").pop().unwrap();
    assert_eq!(chosen.keys(), ["team_id", "channel", "ts", "text"]);
    assert_eq!(chosen.field("text"), Some("✅ 기본 직원을 *슬랙&lt;시험&gt;*으로 선택했습니다. 이제 봇에게 업무를 보내세요."));
    assert_eq!(slack_preference(&pool, &json!(team), Some(&json!("UALLOWED"))).await, Ok(Some(character.clone())));
    assert!(slack.acked("env-action"));
    slack.send(command("env-office-again", "UALLOWED"));
    eventually("선택 표시", || async { slack.calls_to("chat.postMessage").len() > posts_before + 2 }).await;
    let reselection = slack.calls_to("chat.postMessage").pop().unwrap();
    assert_eq!(reselection.blocks(), employee_selection_blocks(&characters, Some(&json!(character))));

    // 멘션은 바로 ack하고, 업무를 시작하고, 끝나면 정리한 답을 스레드에 보낸다.
    let event = |envelope: &str, event_id: &str, event: Value| {
        json!({ "envelope_id": envelope, "type": "events_api", "payload": {
            "type": "event_callback", "team_id": team, "event_id": event_id, "event": event } })
    };
    let mention_id = format!("EvSLACKTEST{}-mention", fixture.suffix);
    let mention = json!({ "type": "app_mention", "user": "UALLOWED", "text": "<@UBOT>  일을\n 해줘", "channel": "C1", "ts": "300.1" });
    slack.send(event("env-mention", &mention_id, mention.clone()));
    eventually("멘션 업무 배달", || delivered(&pool, &team, "C1", "300.1")).await;
    assert!(slack.acked("env-mention"));
    let jobs = backend.jobs.lock().unwrap().clone();
    let (thread_conversation, thread_turn, status_ts): (String, String, String) = sqlx::query_as(
        "SELECT conversation_id::text, last_turn_id::text, status_message_ts FROM slack_threads WHERE team_id = $1 AND channel_id = 'C1' AND thread_ts = '300.1'",
    )
    .bind(&team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, [json!({ "characterId": character, "prompt": "일을 해줘", "conversationId": thread_conversation })]);
    let calls = slack.calls();
    let starting = calls.iter().find(|call| call.method == "chat.postMessage" && call.field("thread_ts") == Some("300.1")).unwrap();
    assert_eq!(starting.keys(), ["team_id", "channel", "thread_ts", "text", "blocks"]);
    assert_eq!(starting.field("text"), Some("⏳ 슬랙<시험> 업무를 시작합니다."));
    assert_eq!(starting.blocks(), progress_blocks(&json!({ "characterName": "슬랙<시험>", "status": "starting" })));
    assert_monitor_calls(&calls, Some(&team), "C1", "300.1", &status_ts, &thread_turn);
    assert!(receipt_exists(&pool, &mention_id).await);

    // 같은 event_id는 ack만 하고 다시 처리하지 않는다. 남·자기 자신·봇·채널 메시지는 수신 기록도 남기지 않는다.
    let calls_before = slack.calls().len();
    slack.send(event("env-mention-again", &mention_id, mention));
    let ignored = [
        ("env-other", "other", json!({ "type": "app_mention", "user": "UOTHER", "text": "<@UBOT> 해줘", "channel": "C1", "ts": "301.1" })),
        ("env-self", "self", json!({ "type": "message", "channel_type": "im", "user": "UBOT", "text": "봇", "channel": "D1", "ts": "302.1" })),
        ("env-bot", "bot", json!({ "type": "message", "channel_type": "im", "bot_id": "BOTHER", "user": "UALLOWED", "text": "봇", "channel": "D1", "ts": "303.1" })),
        ("env-channel", "channel", json!({ "type": "message", "channel_type": "channel", "user": "UALLOWED", "text": "채널", "channel": "C2", "ts": "304.1" })),
    ];
    for (envelope, name, body) in &ignored {
        slack.send(event(envelope, &format!("EvSLACKTEST{}-{name}", fixture.suffix), body.clone()));
    }
    // 종류를 알 수 없는 메시지는 ack하지 않는다.
    slack.send(json!({ "envelope_id": "env-unknown", "type": "events_api", "payload": { "type": "unknown_payload" } }));
    eventually("무시한 이벤트 ack", || async {
        slack.acked("env-mention-again") && ignored.iter().all(|(envelope, _, _)| slack.acked(envelope))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(slack.calls().len(), calls_before, "{:?}", &slack.calls()[calls_before..]);
    assert_eq!(backend.jobs.lock().unwrap().len(), 1);
    for (_, name, _) in &ignored {
        assert!(!receipt_exists(&pool, &format!("EvSLACKTEST{}-{name}", fixture.suffix)).await, "{name}");
    }
    assert!(!slack.acked("env-unknown"));

    // 빈 DM은 같은 스레드에 직원 선택을 보낸다.
    let empty_id = format!("EvSLACKTEST{}-empty", fixture.suffix);
    slack.send(event("env-empty", &empty_id, json!({ "type": "message", "channel_type": "im", "user": "UALLOWED", "text": "<@UBOT>", "channel": "D1", "ts": "400.1" })));
    eventually("빈 DM 선택", || async { slack.calls().len() > calls_before }).await;
    let empty = slack.calls().pop().unwrap();
    assert_eq!(empty.method, "chat.postMessage");
    assert_eq!(empty.keys(), ["team_id", "channel", "thread_ts", "text", "blocks"]);
    assert_eq!((empty.field("channel"), empty.field("thread_ts")), (Some("D1"), Some("400.1")));
    assert_eq!(empty.blocks(), employee_selection_blocks(&characters, Some(&json!(character))));
    assert!(receipt_exists(&pool, &empty_id).await);

    // disconnect를 받으면 닫고 새 주소로 다시 연결한다. ack는 새 연결로 나간다.
    slack.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    eventually("재연결", || async { slack.connections() == 2 }).await;
    assert_eq!(slack.calls_to("apps.connections.open").len(), 2);
    slack.send(command("env-after-reconnect", "UOTHER"));
    eventually("재연결 뒤 ack", || async { slack.acked("env-after-reconnect") }).await;

    running.stop();
    purge(&pool).await;
}
