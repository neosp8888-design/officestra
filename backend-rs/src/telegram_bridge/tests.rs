// Telegram 연결 시험. Node telegram-bridge.test.mjs 23개를 옮기고, 실제 HTTP 호출·실행 프로세스는 127.0.0.1 가짜 서버와 임시 폴더로 확인한다.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

use super::cli::{CliOptions, run};
use super::*;

#[derive(Clone, Debug)]
enum Call {
    Backend { path: String },
    Telegram { method: String, payload: Value },
}

impl Call {
    fn method(&self) -> Option<&str> {
        match self {
            Call::Telegram { method, .. } => Some(method),
            Call::Backend { .. } => None,
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            Call::Backend { path, .. } => Some(path),
            Call::Telegram { .. } => None,
        }
    }

    fn payload(&self) -> Option<&Value> {
        match self {
            Call::Telegram { payload, .. } => Some(payload),
            Call::Backend { .. } => None,
        }
    }

    /// `c.payload?.text`
    fn text(&self) -> Option<&str> {
        self.payload().and_then(|payload| payload.get("text")).and_then(Value::as_str)
    }
}

struct Controls {
    next: i64,
    available: bool,
    job_error: Option<CallError>,
    post_gate: Option<Arc<Semaphore>>,
    turn: Value,
    telegram_error: Option<CallError>,
    roleplay: Value,
    roleplay_error: Option<CallError>,
}

struct Fixture {
    bridge: Arc<TelegramBridge>,
    calls: Arc<Mutex<Vec<Call>>>,
    saved: Arc<Mutex<Vec<Value>>>,
    jobs: Arc<Mutex<Vec<Value>>>,
    roleplay_posts: Arc<Mutex<Vec<(String, Value)>>>,
    controls: Arc<Mutex<Controls>>,
}

impl Fixture {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn jobs(&self) -> Vec<Value> {
        self.jobs.lock().unwrap().clone()
    }

    fn posts(&self) -> Vec<(String, Value)> {
        self.roleplay_posts.lock().unwrap().clone()
    }

    fn state(&self) -> Value {
        self.bridge.state()
    }

    fn set(&self, change: impl FnOnce(&mut Controls)) {
        change(&mut self.controls.lock().unwrap());
    }

    fn texts(&self) -> Vec<String> {
        self.calls().iter().filter_map(|call| call.text().map(str::to_string)).collect()
    }

    fn some_text(&self, needle: &str) -> bool {
        self.texts().iter().any(|text| text.contains(needle))
    }

    fn pending_values(&self) -> Vec<Value> {
        let state = self.state();
        let pending = state["pending"].as_object().unwrap();
        js_keys(pending).into_iter().map(|key| pending[key].clone()).collect()
    }
}

fn employees() -> Value {
    json!([{ "id": "boss", "name": "백부장" }, { "id": "left-man", "name": "클대리" }])
}

fn silent() -> Logger {
    Arc::new(|_, _| {})
}

fn default_state() -> Value {
    json!({ "allowedChats": { "123": "123" }, "offset": 0 })
}

fn fixture(state: Value) -> Fixture {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
    let saved: Arc<Mutex<Vec<Value>>> = Arc::default();
    let jobs: Arc<Mutex<Vec<Value>>> = Arc::default();
    let roleplay_posts: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let controls = Arc::new(Mutex::new(Controls {
        next: 100,
        available: true,
        job_error: None,
        post_gate: None,
        turn: json!({ "status": "running", "response": "" }),
        telegram_error: None,
        roleplay: json!({ "profile": { "enabled": false }, "messages": [], "generating": false }),
        roleplay_error: None,
    }));
    let backend: BackendCall = {
        let (calls, jobs, posts, controls) = (calls.clone(), jobs.clone(), roleplay_posts.clone(), controls.clone());
        Arc::new(move |path: &str, body: Option<Value>| {
            let path = path.to_string();
            calls.lock().unwrap().push(Call::Backend { path: path.clone() });
            let (jobs, posts, controls) = (jobs.clone(), posts.clone(), controls.clone());
            async move {
                if path == "/api/characters" {
                    return Ok(json!({ "characters": employees() }));
                }
                if path.starts_with("/api/roleplay/profiles/") {
                    let (error, roleplay) = {
                        let controls = controls.lock().unwrap();
                        (controls.roleplay_error.clone(), controls.roleplay.clone())
                    };
                    if let Some(error) = error {
                        return Err(error);
                    }
                    if let Some(body) = body {
                        posts.lock().unwrap().push((path.clone(), body));
                        return Ok(if path.ends_with("/messages") {
                            json!({ "accepted": true, "chatId": roleplay["profile"]["currentChatId"] })
                        } else {
                            json!({})
                        });
                    }
                    return Ok(roleplay);
                }
                let available = controls.lock().unwrap().available;
                if path.starts_with("/api/agent-jobs?") {
                    return Ok(json!({ "employees": [{ "canReceive": available }] }));
                }
                if path == "/api/agent-jobs" && body.is_none() {
                    let list: Vec<Value> = employees()
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|employee| {
                            let mut employee = employee.clone();
                            employee["canReceive"] = json!(available);
                            employee
                        })
                        .collect();
                    return Ok(json!({ "employees": list }));
                }
                if path == "/api/agent-jobs" {
                    let body = body.unwrap();
                    jobs.lock().unwrap().push(body.clone());
                    let gate = controls.lock().unwrap().post_gate.clone();
                    if let Some(gate) = gate {
                        let _ = gate.acquire().await.unwrap();
                    }
                    if let Some(error) = controls.lock().unwrap().job_error.clone() {
                        return Err(error);
                    }
                    let count = jobs.lock().unwrap().len();
                    let conversation = body.get("conversationId").cloned().unwrap_or_else(|| json!(format!("conversation-{count}")));
                    return Ok(json!({ "turnId": format!("turn-{count}"), "conversationId": conversation }));
                }
                if path.starts_with("/api/live-feed/") {
                    return Ok(json!({ "turn": controls.lock().unwrap().turn.clone() }));
                }
                Err(CallError::new(path))
            }
            .boxed()
        })
    };
    let telegram: TelegramCall = {
        let (calls, controls) = (calls.clone(), controls.clone());
        Arc::new(move |method: &str, payload: Value| {
            calls.lock().unwrap().push(Call::Telegram { method: method.to_string(), payload });
            let result = {
                let mut controls = controls.lock().unwrap();
                match controls.telegram_error.take() {
                    Some(error) if method == "sendMessage" => Err(error),
                    other => {
                        controls.telegram_error = other;
                        let id = controls.next;
                        controls.next += 1;
                        Ok(json!({ "message_id": id }))
                    }
                }
            };
            async move { result }.boxed()
        })
    };
    let persist: Persist = {
        let saved = saved.clone();
        Arc::new(move |state: &Value| {
            saved.lock().unwrap().push(state.clone());
            Ok(())
        })
    };
    let bridge = TelegramBridge::new(state, persist, telegram, backend, silent()).unwrap();
    Fixture { bridge, calls, saved, jobs, roleplay_posts, controls }
}

/// `message(id, text, options)`. edit로 chat·from 등을 바꾸거나 지운다(undefined).
fn message_with(id: i64, text: &str, edit: impl FnOnce(&mut Map<String, Value>)) -> Value {
    let mut message = json!({ "message_id": id, "chat": { "id": 123, "type": "private" }, "from": { "id": 123 }, "text": text });
    edit(message.as_object_mut().unwrap());
    json!({ "update_id": id, "message": message })
}

fn message(id: i64, text: &str) -> Value {
    message_with(id, text, |_| {})
}

fn error_with_status(message: &str, status: u16) -> CallError {
    CallError { status: Some(status), ..CallError::new(message) }
}

fn roleplay_state(messages: Value, extra: Value) -> Value {
    let mut state = json!({
        "profile": { "enabled": true, "currentChatId": "existing-roleplay-chat", "presetId": "unchanged-preset" },
        "messages": messages, "generating": false, "lastError": null,
    });
    for (key, value) in extra.as_object().unwrap() {
        state[key] = value.clone();
    }
    state
}

#[tokio::test]
async fn owner_allowlist_is_mandatory_and_strangers_cannot_launch_work() {
    let error = TelegramBridge::new(json!({}), Arc::new(|_: &Value| Ok(())), Arc::new(|_: &str, _| async { Ok(Value::Null) }.boxed()), Arc::new(|_: &str, _| async { Ok(Value::Null) }.boxed()), silent())
        .err()
        .unwrap();
    assert!(error.contains("allowlist"));
    let f = fixture(default_state());
    f.bridge.receive(&message_with(1, "hello", |m| { m.insert("from".into(), json!({ "id": 999 })); })).await.unwrap();
    f.bridge.receive(&message_with(2, "hello", |m| { m.insert("chat".into(), json!({ "id": 123, "type": "group" })); })).await.unwrap();
    f.bridge.receive(&message_with(3, "hello", |m| { m.shift_remove("from"); })).await.unwrap();
    f.bridge
        .receive(&message_with(4, "hello", |m| {
            m.insert("chat".into(), json!({ "id": 999, "type": "private" }));
            m.shift_remove("from");
        }))
        .await
        .unwrap();
    assert_eq!(f.calls().len(), 0);
    assert_eq!(f.state()["offset"], json!(5));
}

#[tokio::test]
async fn staff_menu_uses_real_employee_ids_and_callbacks_select_employee() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "/office")).await.unwrap();
    let menu = f.calls().into_iter().find(|call| call.method() == Some("sendMessage")).unwrap();
    assert_eq!(menu.payload().unwrap()["reply_markup"]["inline_keyboard"][1][0]["callback_data"], json!("staff:left-man"));
    let callback_message = message(0, "")["message"].clone();
    let callback = json!({ "update_id": 2, "callback_query": { "id": "cb", "from": { "id": 123 }, "data": "staff:left-man", "message": callback_message } });
    f.bridge.receive(&callback).await.unwrap();
    assert_eq!(f.state()["chats"]["123"]["selected"], json!("left-man"));
    assert!(f.calls().iter().any(|call| call.method() == Some("answerCallbackQuery")));
    assert_eq!(f.jobs().len(), 0);
}

#[tokio::test]
async fn dispatch_persists_receipt_before_post_and_resumes_same_conversation() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "클대리: 첫 줄\n둘째 줄")).await.unwrap();
    assert_eq!(f.jobs()[0], json!({ "characterId": "left-man", "requestSource": "remote", "prompt": "첫 줄\n둘째 줄" }));
    // 발신자 규칙: 원격 접수 본문에 senderCharacterId를 넣지 않는다(키 순서까지 Node와 같다).
    assert_eq!(String::from_utf8(to_js_json(&f.jobs()[0]).unwrap()).unwrap(), r#"{"characterId":"left-man","requestSource":"remote","prompt":"첫 줄\n둘째 줄"}"#);
    let saved = f.saved.lock().unwrap().clone();
    assert!(saved.iter().any(|state| state["offset"] == json!(2) && state["pending"].as_object().unwrap().values().next().is_some_and(|pending| pending["stage"] == json!("launching"))));
    f.bridge.receive(&message(1, "duplicate")).await.unwrap();
    assert_eq!(f.jobs().len(), 1);
    f.bridge.receive(&message(2, "이어서")).await.unwrap();
    assert_eq!(f.jobs()[1]["conversationId"], json!("conversation-1"));
    assert_eq!(f.jobs()[1]["characterId"], json!("left-man"));
}

#[tokio::test]
async fn reply_to_earlier_response_routes_to_its_employee() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "클대리: 질문")).await.unwrap();
    f.bridge.edit_state(|state| state["chats"]["123"]["selected"] = json!("boss"));
    f.bridge.receive(&message_with(2, "추가 질문", |m| { m.insert("reply_to_message".into(), json!({ "message_id": 100 })); })).await.unwrap();
    assert_eq!(f.jobs()[1]["characterId"], json!("left-man"));
}

#[tokio::test]
async fn busy_preflight_and_post_409_never_retry_employee_work() {
    let f = fixture(default_state());
    f.set(|c| c.available = false);
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    assert_eq!(f.jobs().len(), 0);
    f.set(|c| {
        c.available = true;
        c.job_error = Some(error_with_status("busy", 409));
    });
    f.bridge.receive(&message(2, "질문")).await.unwrap();
    f.bridge.receive(&message(2, "질문")).await.unwrap();
    assert_eq!(f.jobs().len(), 1);
    assert_eq!(f.state()["pending"], json!({}));
    assert!(f.some_text("접수하지 못했습니다"));
}

#[tokio::test]
async fn completed_response_is_relayed_in_chunks_once() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    f.bridge.tick().await.unwrap();
    assert_eq!(f.pending_values().len(), 1);
    let response = "한😀".repeat(3000);
    f.set(|c| c.turn = json!({ "status": "completed", "response": response, "needsInput": true }));
    f.bridge.tick().await.unwrap();
    let answers: Vec<String> =
        f.calls().iter().filter(|call| call.method() == Some("sendMessage")).skip(1).map(|call| call.text().unwrap().to_string()).collect();
    assert!(answers.len() >= 3);
    assert_eq!(answers.concat(), format!("[백부장]\n{response}"));
    let count = f.calls().len();
    f.bridge.tick().await.unwrap();
    assert_eq!(f.calls().len(), count);
    assert!(f.calls().iter().any(|call| call.method() == Some("editMessageText") && call.text().unwrap().contains("답변 필요")));
    assert_eq!(f.state()["pending"], json!({}));
}

#[tokio::test]
async fn restart_restores_pending_delivery_without_relaunching() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    let restored = fixture(f.state());
    restored.set(|c| c.turn = json!({ "status": "completed", "response": "응답" }));
    restored.bridge.tick().await.unwrap();
    assert_eq!(restored.jobs().len(), 0);
    assert!(restored.texts().iter().any(|text| text == "[백부장]\n응답"));
}

#[tokio::test]
async fn ambiguous_send_is_not_duplicated_and_status_reports_uncertainty() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    f.set(|c| {
        c.turn = json!({ "status": "completed", "response": "확인" });
        c.telegram_error = Some(CallError { uncertain: true, ..CallError::new("timeout") });
    });
    f.bridge.tick().await.unwrap();
    assert_eq!(f.pending_values()[0]["uncertainDelivery"], json!(true));
    f.bridge.tick().await.unwrap();
    assert_eq!(f.texts().iter().filter(|text| *text == "[백부장]\n확인").count(), 1);
    assert!(f.calls().iter().any(|call| call.method() == Some("editMessageText") && call.text().unwrap().contains("전송 확인 불가")));
}

#[tokio::test]
async fn rate_limit_delays_retry_and_blocked_chat_stops_retrying() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    f.set(|c| {
        c.turn = json!({ "status": "completed", "response": "확인" });
        c.telegram_error = Some(CallError { retry_after: Some(json!(30)), ..error_with_status("rate limit", 429) });
    });
    f.bridge.tick().await.unwrap();
    let count = f.calls().len();
    f.bridge.tick().await.unwrap();
    assert_eq!(f.calls().len(), count);
    f.bridge.edit_state(|state| state["pending"]["1"]["retryAt"] = json!(0));
    f.set(|c| c.telegram_error = Some(error_with_status("blocked", 403)));
    f.bridge.tick().await.unwrap();
    assert_eq!(f.state()["pending"]["1"]["stage"], json!("delivery-failed"));
    let blocked = f.calls().len();
    f.bridge.tick().await.unwrap();
    assert_eq!(f.calls().len(), blocked);
}

#[tokio::test]
async fn monitor_does_not_mistake_in_flight_submission_for_crashed_launch() {
    let f = fixture(default_state());
    let gate = Arc::new(Semaphore::new(0));
    f.set(|c| c.post_gate = Some(gate.clone()));
    let bridge = f.bridge.clone();
    let receive = tokio::spawn(async move { bridge.receive(&message(1, "질문")).await });
    while f.jobs().is_empty() {
        tokio::task::yield_now().await;
    }
    f.bridge.tick().await.unwrap();
    assert_eq!(f.pending_values().len(), 1);
    gate.add_permits(1);
    receive.await.unwrap().unwrap();
    assert_eq!(f.pending_values()[0]["stage"], json!("running"));
}

#[tokio::test]
async fn uncertain_launch_after_restart_is_reported_without_second_post() {
    let f = fixture(json!({ "allowedChats": { "123": "123" }, "pending": { "1": {
        "stage": "launching", "chatID": "123", "characterID": "boss", "name": "백부장", "nextPart": 0,
    } } }));
    f.bridge.tick().await.unwrap();
    assert_eq!(f.jobs().len(), 0);
    assert!(f.some_text("자동 재실행하지 않았습니다"));
}

#[tokio::test]
async fn failed_turn_is_visible_and_new_keeps_old_records() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    f.set(|c| c.turn = json!({ "status": "failed", "errorMessage": "로컬 연결 실패: 4090 SSH operation failed" }));
    f.bridge.tick().await.unwrap();
    assert!(f.some_text("실패"));
    assert!(f.some_text("로컬 모델(4090)"));
    assert!(f.calls().iter().any(|call| call.method() == Some("editMessageText") && call.text().unwrap().starts_with('❌')));
    f.bridge.receive(&message(2, "/new")).await.unwrap();
    assert_eq!(f.state()["chats"]["123"]["conversations"], json!({}));
    assert!(f.some_text("기존 기록은 보존"));
}

#[test]
fn unicode_split_respects_utf16_limit_without_breaking_emoji() {
    let source = "a😀\n ".repeat(2000);
    let parts = split_telegram_text(&source, TEXT_LIMIT);
    assert!(parts.len() > 1);
    assert_eq!(parts.concat(), source);
    assert!(parts.iter().all(|part| part.encode_utf16().count() <= TEXT_LIMIT));
    // 한 글자가 한도보다 길면 Node처럼 빈 조각이 먼저 나온다.
    assert_eq!(split_telegram_text("😀", 1), vec![String::new(), "😀".to_string()]);
}

#[test]
fn token_file_supports_quoted_and_exported_values_without_shell() {
    let directory = std::env::temp_dir().join(format!("office-rs-telegram-{}", crate::js::random_uuid()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("telegram.env");
    let path_text = path.to_string_lossy().into_owned();
    let read = |content: &str| {
        std::fs::write(&path, content).unwrap();
        read_telegram_configuration(&path_text)
    };
    assert_eq!(read("export TELEGRAM_BOT_TOKEN=\"example-token\"\n").unwrap(), "example-token");
    assert_eq!(read("# 주석\r\n  TELEGRAM_BOT_TOKEN = 'single'  \r\n").unwrap(), "single");
    assert_eq!(read("TELEGRAM_BOT_TOKEN=$(echo shell)\n").unwrap(), "$(echo shell)");
    assert_eq!(read("TELEGRAM_BOT_TOKEN=first\nTELEGRAM_BOT_TOKEN=\"\"\n").unwrap_err(), "Telegram token is missing");
    assert_eq!(read("telegram_bot_token=lower\n").unwrap_err(), "Telegram token is missing");
    assert_eq!(read("exportTELEGRAM_BOT_TOKEN=glued\n").unwrap_err(), "Telegram token is missing");
    assert_eq!(read("TELEGRAM_BOT_TOKEN=a\rb\n").unwrap_err(), "Telegram token is missing");
    assert_eq!(read("TELEGRAM_BOT_TOKEN='mismatch\"\n").unwrap(), "'mismatch\"");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read_telegram_configuration(&path_text).unwrap_err(), format!("ENOENT: no such file or directory, open '{path_text}'"));
    std::fs::remove_dir_all(&directory).unwrap();
}

#[tokio::test]
async fn enabled_roleplay_routes_to_roleplay_api_without_cli() {
    let f = fixture(default_state());
    f.set(|c| {
        c.available = false;
        c.roleplay = roleplay_state(json!([{ "id": "old-user", "role": "user" }, { "id": "old-reply", "role": "char", "content": "previous" }]), json!({}));
    });
    f.bridge.receive(&message(1, "클대리: 다음 대화")).await.unwrap();
    assert_eq!(f.posts(), vec![("/api/roleplay/profiles/left-man/messages".to_string(), json!({ "content": "다음 대화" }))]);
    assert_eq!(f.jobs().len(), 0);
    assert_eq!(f.calls().iter().filter(|call| call.path().is_some_and(|path| path.starts_with("/api/agent-jobs"))).count(), 0);
    assert_eq!(f.state()["chats"]["123"]["conversations"], json!({}));
    assert_eq!(f.state()["pending"]["1"]["roleplayChatID"], json!("existing-roleplay-chat"));
    assert_eq!(f.state()["pending"]["1"]["afterMessageID"], json!("old-reply"));
}

#[tokio::test]
async fn roleplay_polling_delivers_only_reply_after_accepted_input() {
    let f = fixture(default_state());
    let old = json!({ "id": "old", "role": "char", "content": "must not repeat" });
    f.set(|c| c.roleplay = roleplay_state(json!([old]), json!({})));
    f.bridge.receive(&message(1, "안녕")).await.unwrap();
    f.set(|c| c.roleplay = roleplay_state(json!([old, { "id": "input", "role": "user", "content": "안녕" }]), json!({ "generating": true })));
    f.bridge.tick().await.unwrap();
    assert!(f.state()["pending"].get("1").is_some());
    f.set(|c| {
        c.roleplay = roleplay_state(json!([old, { "id": "input", "role": "user" }, { "id": "reply", "role": "char", "content": "안녕하세요" }]), json!({}));
    });
    f.bridge.tick().await.unwrap();
    assert!(f.texts().iter().any(|text| text == "[백부장]\n안녕하세요"));
    assert!(!f.some_text("must not repeat"));
    assert_eq!(f.state()["pending"], json!({}));
}

#[tokio::test]
async fn roleplay_busy_state_blocks_generation_only() {
    let f = fixture(default_state());
    f.set(|c| c.roleplay = roleplay_state(json!([]), json!({ "generating": true })));
    f.bridge.receive(&message(1, "대화")).await.unwrap();
    f.bridge.receive(&message(2, "/new")).await.unwrap();
    assert_eq!(f.posts().len(), 0);
    assert_eq!(f.jobs().len(), 0);
    assert!(f.some_text("완료 후"));
}

#[tokio::test]
async fn roleplay_response_is_restored_across_restart() {
    let f = fixture(default_state());
    f.set(|c| c.roleplay = roleplay_state(json!([]), json!({})));
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    let restored = fixture(f.state());
    restored.set(|c| {
        c.roleplay = roleplay_state(json!([{ "id": "input", "role": "user" }, { "id": "reply", "role": "char", "content": "reply after restart" }]), json!({}));
    });
    restored.bridge.tick().await.unwrap();
    assert_eq!(restored.jobs().len(), 0);
    assert_eq!(restored.posts().len(), 0);
    assert!(restored.some_text("reply after restart"));
}

#[tokio::test]
async fn roleplay_chat_change_and_simultaneous_inputs_never_deliver_unrelated_reply() {
    for changed_chat in [false, true] {
        let f = fixture(default_state());
        f.set(|c| c.roleplay = roleplay_state(json!([]), json!({})));
        f.bridge.receive(&message(1, "질문")).await.unwrap();
        let extra = if changed_chat { json!({ "profile": { "enabled": true, "currentChatId": "different-chat" } }) } else { json!({}) };
        f.set(|c| {
            c.roleplay = roleplay_state(
                json!([{ "id": "input1", "role": "user" }, { "id": "input2", "role": "user" }, { "id": "reply", "role": "char", "content": "unrelated reply" }]),
                extra,
            );
        });
        f.bridge.tick().await.unwrap();
        assert!(!f.some_text("unrelated reply"));
        assert_eq!(f.state()["pending"], json!({}));
    }
}

#[tokio::test]
async fn roleplay_generation_failure_is_surfaced_without_cli_fallback() {
    let f = fixture(default_state());
    f.set(|c| c.roleplay = roleplay_state(json!([]), json!({})));
    f.bridge.receive(&message(1, "질문")).await.unwrap();
    f.set(|c| c.roleplay = roleplay_state(json!([{ "id": "input", "role": "user" }]), json!({ "lastError": "upstream failed" })));
    f.bridge.tick().await.unwrap();
    assert!(f.some_text("롤플레이 답변 생성에 실패"));
    assert_eq!(f.jobs().len(), 0);
}

#[tokio::test]
async fn roleplay_profile_errors_fail_closed_and_only_404_allows_routing() {
    let f = fixture(default_state());
    f.set(|c| c.roleplay_error = Some(error_with_status("unavailable", 503)));
    assert!(f.bridge.receive(&message(1, "질문")).await.is_err());
    assert_eq!(f.jobs().len(), 0);
    f.set(|c| c.roleplay_error = Some(error_with_status("not found", 404)));
    f.bridge.receive(&message(2, "질문")).await.unwrap();
    assert_eq!(f.jobs().len(), 1);
}

#[tokio::test]
async fn roleplay_new_uses_new_chat_api_and_menu_marks_mode() {
    let f = fixture(default_state());
    f.set(|c| c.roleplay = roleplay_state(json!([]), json!({})));
    f.bridge.receive(&message(1, "/office")).await.unwrap();
    let menu = f.calls().into_iter().find(|call| call.payload().is_some_and(|payload| payload.get("reply_markup").is_some())).unwrap();
    assert!(menu.payload().unwrap()["reply_markup"]["inline_keyboard"][0][0]["text"].as_str().unwrap().contains("롤플레이"));
    assert_eq!(f.posts().len(), 0);
    f.bridge.receive(&message(2, "/new")).await.unwrap();
    assert_eq!(f.posts(), vec![("/api/roleplay/profiles/boss/chats".to_string(), json!({}))]);
    assert_eq!(f.jobs().len(), 0);
}

// 아래는 Rust에서 더한 시험이다.

#[tokio::test]
async fn persisted_state_uses_js_key_order_and_keeps_500_routes() {
    let routes: Map<String, Value> = (1..=500).rev().map(|id| (id.to_string(), json!("boss"))).collect();
    let f = fixture(json!({ "allowedChats": { "123": "123" }, "offset": 0, "chats": { "123": { "selected": "boss", "conversations": {}, "routes": routes } } }));
    f.set(|c| c.next = 1000);
    let callback = json!({ "update_id": 3, "callback_query": { "id": "cb", "from": { "id": 123 }, "data": "staff:left-man", "message": message(0, "")["message"] } });
    f.bridge.receive(&callback).await.unwrap();
    let last = f.saved.lock().unwrap().last().cloned().unwrap();
    let text = String::from_utf8(to_js_json(&last).unwrap()).unwrap();
    assert!(text.starts_with(r#"{"allowedChats":{"123":"123"},"offset":4,"chats":{"123":{"selected":"left-man","conversations":{},"routes":{"2":"boss","3":"boss""#), "{}", &text[..120]);
    // 가짜 Telegram은 호출마다 번호를 하나씩 쓴다. answerCallbackQuery가 1000, 메뉴 sendMessage가 1001이다.
    assert_eq!(f.calls().iter().filter_map(Call::method).collect::<Vec<_>>(), ["answerCallbackQuery", "sendMessage"]);
    assert!(text.ends_with(r#""500":"boss","1001":"left-man"}}},"pending":{}}"#), "{}", &text[text.len() - 120..]);
    let routes = last["chats"]["123"]["routes"].as_object().unwrap();
    assert_eq!(routes.len(), 500);
    assert!(routes.get("1").is_none());
}

#[tokio::test]
async fn ignores_unsafe_update_ids_without_saving() {
    let f = fixture(default_state());
    for id in [json!(1.5), json!("1"), json!(9_007_199_254_740_992u64), Value::Null] {
        f.bridge.receive(&json!({ "update_id": id, "message": message(1, "질문")["message"] })).await.unwrap();
    }
    assert!(f.bridge.receive(&Value::Null).await.is_err());
    assert_eq!(f.saved.lock().unwrap().len(), 0);
    assert_eq!(f.calls().len(), 0);
}

#[tokio::test]
async fn status_text_commands_and_prefixes_follow_node() {
    let f = fixture(default_state());
    f.bridge.receive(&message(1, "/status")).await.unwrap();
    assert_eq!(f.texts().last().unwrap(), "백부장: 대화 가능\n클대리: 대화 가능");
    f.set(|c| c.available = false);
    f.bridge.receive(&message(2, "/status@OfficeBot extra")).await.unwrap();
    assert_eq!(f.texts().last().unwrap(), "백부장: 작업 중 (undefined)\n클대리: 작업 중 (undefined)");
    f.bridge.receive(&message_with(3, "", |m| { m.shift_remove("text"); })).await.unwrap();
    assert_eq!(f.texts().last().unwrap(), "현재는 텍스트 대화를 지원합니다. 내용을 글로 보내주세요.");
    let menus = |f: &Fixture| f.calls().iter().filter(|call| call.payload().is_some_and(|payload| payload.get("reply_markup").is_some())).count();
    f.bridge.receive(&message(4, "/unknown")).await.unwrap();
    f.bridge.receive(&message(5, "클대리:   ")).await.unwrap();
    assert_eq!(menus(&f), 2);
    f.set(|c| c.available = true);
    f.bridge.receive(&message(6, "@left-man 전각 확인")).await.unwrap();
    f.bridge.receive(&message(7, "백부장：  전각 콜론  ")).await.unwrap();
    let jobs = f.jobs();
    assert_eq!((jobs[0]["characterId"].clone(), jobs[0]["prompt"].clone()), (json!("left-man"), json!("전각 확인")));
    assert_eq!((jobs[1]["characterId"].clone(), jobs[1]["prompt"].clone()), (json!("boss"), json!("전각 콜론")));
    let status = f.calls().into_iter().filter(|call| call.text().is_some_and(|text| text.starts_with('⏳'))).last().unwrap();
    assert_eq!(status.payload().unwrap()["reply_parameters"], json!({ "message_id": 7 }));
}

type Reply = Arc<dyn Fn(&Seen) -> futures_util::future::BoxFuture<'static, (u16, String)> + Send + Sync>;

/// 가짜 서버가 받은 요청.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    content_type: Option<String>,
    body: String,
}

struct FakeServer {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeServer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn fake_server(seen: Arc<Mutex<Vec<Seen>>>, reply: Reply) -> FakeServer {
    use axum::response::IntoResponse;
    let handler = {
        let seen = seen.clone();
        move |request: axum::extract::Request| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let method = request.method().to_string();
                let path = request.uri().path_and_query().map(ToString::to_string).unwrap_or_default();
                let content_type = request.headers().get("content-type").map(|value| value.to_str().unwrap_or_default().to_string());
                let bytes = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap_or_default();
                let request = Seen { method, path, content_type, body: String::from_utf8_lossy(&bytes).into_owned() };
                seen.lock().unwrap().push(request.clone());
                let (status, body) = reply(&request).await;
                (axum::http::StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], body).into_response()
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, axum::Router::new().fallback(handler)).await;
    });
    FakeServer { url: format!("http://{address}"), seen }
}

fn fixed(status: u16, body: &'static str) -> Reply {
    Arc::new(move |_| async move { (status, body.to_string()) }.boxed())
}

/// 연결이 거부되는 주소(잡았다 바로 놓은 포트).
async fn closed_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

#[tokio::test]
async fn telegram_client_posts_json_and_never_puts_token_in_errors() {
    const TOKEN: &str = "123:SECRET-test";
    let server = fake_server(Arc::default(), fixed(200, r#"{"ok":true,"result":{"message_id":5}}"#)).await;
    let client = telegram_client(TOKEN, &server.url).unwrap();
    assert_eq!(client("sendMessage", json!({ "chat_id": "1", "text": "안녕" })).await.unwrap(), json!({ "message_id": 5 }));
    let seen = &server.seen()[0];
    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/bot123:SECRET-test/sendMessage"));
    assert_eq!(seen.content_type.as_deref(), Some("application/json"));
    assert_eq!(seen.body, r#"{"chat_id":"1","text":"안녕"}"#);

    let closed = telegram_client(TOKEN, &closed_url().await).unwrap();
    let error = closed("sendMessage", json!({})).await.unwrap_err();
    assert_eq!((error.message.as_str(), error.uncertain, error.status), ("Telegram sendMessage connection failed", true, None));

    let throttled = fake_server(Arc::default(), fixed(429, r#"{"ok":false,"parameters":{"retry_after":20}}"#)).await;
    let error = telegram_client(TOKEN, &throttled.url).unwrap()("sendMessage", json!({})).await.unwrap_err();
    assert_eq!((error.message.as_str(), error.status, error.retry_after, error.uncertain), ("Telegram sendMessage failed (429)", Some(429), Some(json!(20)), false));

    for (status, body, expected) in [
        (200, r#"{"ok":false}"#, "Telegram getMe failed (200)"),
        (200, "not json", "Telegram getMe failed (200)"),
        (500, "null", "Cannot read properties of null (reading 'parameters')"),
        (200, "null", "Cannot read properties of null (reading 'ok')"),
    ] {
        let server = fake_server(Arc::default(), fixed(status, body)).await;
        let error = telegram_client(TOKEN, &server.url).unwrap()("getMe", json!({})).await.unwrap_err();
        assert_eq!(error.message, expected);
        assert!(!error.message.contains("SECRET"));
    }
    let bom = fake_server(Arc::default(), fixed(200, "\u{feff}{\"ok\":true}")).await;
    assert_eq!(telegram_client(TOKEN, &bom.url).unwrap()("getMe", json!({})).await.unwrap(), Value::Null);
}

#[tokio::test]
async fn backend_client_requires_loopback_and_reports_status() {
    for base in ["https://example.com", "http://example.com", "http://192.0.2.1:4317", "https://127.0.0.1:4317"] {
        assert_eq!(backend_client(base).err().unwrap(), "Telegram bridge requires a loopback backend");
    }
    assert_eq!(backend_client("not a url").err().unwrap(), "Invalid URL");
    for base in ["http://localhost:4317", "http://[::1]:4317", "http://LOCALHOST:1"] {
        assert!(backend_client(base).is_ok(), "{base}");
    }
    let reply: Reply = Arc::new(|seen: &Seen| {
        let result = match seen.path.as_str() {
            "/api/missing" => (404, r#"{"error":"없음"}"#.to_string()),
            "/api/text" => (200, "plain".to_string()),
            _ => (200, format!(r#"{{"method":"{}"}}"#, seen.method)),
        };
        async move { result }.boxed()
    });
    let server = fake_server(Arc::default(), reply).await;
    let backend = backend_client(&server.url).unwrap();
    assert_eq!(backend("/api/characters", None).await.unwrap(), json!({ "method": "GET" }));
    assert_eq!(backend("/api/agent-jobs", Some(json!({ "prompt": "질문" }))).await.unwrap(), json!({ "method": "POST" }));
    let seen = server.seen();
    assert_eq!((seen[0].content_type.clone(), seen[0].body.as_str()), (None, ""));
    assert_eq!((seen[1].content_type.as_deref(), seen[1].body.as_str()), (Some("application/json"), r#"{"prompt":"질문"}"#));
    let error = backend("/api/missing", None).await.unwrap_err();
    assert_eq!((error.message.as_str(), error.status), ("OFFICESTRA request failed (404)", Some(404)));
    assert_eq!(backend("/api/text", None).await.unwrap(), json!({}));
    let closed = backend_client(&closed_url().await).unwrap();
    let get = closed("/api/characters", None).await.unwrap_err();
    let post = closed("/api/agent-jobs", Some(json!({}))).await.unwrap_err();
    assert_eq!((get.message.as_str(), get.uncertain, post.uncertain), ("OFFICESTRA backend connection failed", false, true));
}

const CLI_TOKEN: &str = "424242:fake-cli-token";

type Lines = Arc<Mutex<Vec<(LogLevel, String)>>>;

fn capture() -> (Logger, Lines) {
    let lines: Lines = Arc::default();
    let logger: Logger = {
        let lines = lines.clone();
        Arc::new(move |level, line| lines.lock().unwrap().push((level, line.to_string())))
    };
    (logger, lines)
}

struct Workspace {
    root: std::path::PathBuf,
    directory: String,
}

impl Workspace {
    fn new(token: Option<&str>) -> Self {
        let root = std::env::temp_dir().join(format!("office-rs-telegram-cli-{}", crate::js::random_uuid()));
        let directory = root.join("config").to_string_lossy().into_owned();
        std::fs::create_dir_all(&root).unwrap();
        if let Some(token) = token {
            std::fs::write(root.join("telegram.env"), format!("export TELEGRAM_BOT_TOKEN='{token}'\n")).unwrap();
        }
        Self { root, directory }
    }

    fn file(&self, name: &str) -> String {
        format!("{}/{name}", self.directory)
    }

    fn write_state(&self, state: Value) {
        std::fs::create_dir_all(&self.directory).unwrap();
        std::fs::write(self.file("telegram-state.json"), format!("{state}\n")).unwrap();
    }

    fn state(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.file("telegram-state.json")).unwrap()).unwrap()
    }

    fn options(&self, telegram: &str, backend: &str, arguments: &[&str], logger: Logger) -> CliOptions {
        CliOptions {
            directory: self.directory.clone(),
            configuration_file: self.root.join("telegram.env").to_string_lossy().into_owned(),
            telegram_api_url: telegram.into(),
            backend_url: backend.into(),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            tick_interval: Duration::from_millis(20),
            poll_retry_floor: Duration::from_millis(10),
            handle_signals: false,
            logger,
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 가짜 getUpdates 응답 순서. After는 지정한 메서드 요청이 들어온 뒤(조금 더 기다렸다가) 답한다.
enum Scripted {
    Reply(u16, Value),
    After(&'static str, u16, Value),
}

fn updates(result: Value) -> Scripted {
    Scripted::Reply(200, json!({ "ok": true, "result": result }))
}

fn conflict() -> Value {
    json!({ "ok": false, "error_code": 409, "description": "Conflict: terminated by other getUpdates request" })
}

async fn fake_telegram(script: Vec<Scripted>, webhook: &'static str) -> FakeServer {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let queue = Arc::new(Mutex::new(VecDeque::from(script)));
    let next = Arc::new(std::sync::atomic::AtomicI64::new(1));
    let reply: Reply = {
        let seen = seen.clone();
        Arc::new(move |request: &Seen| {
            let method = request.path.rsplit('/').next().unwrap_or_default().to_string();
            let (seen, queue, next) = (seen.clone(), queue.clone(), next.clone());
            async move {
                let reply = match method.as_str() {
                    "getUpdates" => match queue.lock().unwrap().pop_front() {
                        None => Scripted::Reply(409, conflict()),
                        Some(entry) => entry,
                    },
                    "getWebhookInfo" => Scripted::Reply(200, json!({ "ok": true, "result": { "url": webhook } })),
                    _ => Scripted::Reply(200, json!({ "ok": true, "result": { "message_id": next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) } })),
                };
                let (status, body) = match reply {
                    Scripted::Reply(status, body) => (status, body),
                    Scripted::After(wanted, status, body) => {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                        while !seen.lock().unwrap().iter().any(|seen| seen.path.ends_with(&format!("/{wanted}"))) && tokio::time::Instant::now() < deadline {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        (status, body)
                    }
                };
                (status, body.to_string())
            }
            .boxed()
        })
    };
    fake_server(seen, reply).await
}

/// Node 시험 픽스처와 같은 직원 둘. 롤플레이 프로필은 없음(404).
async fn fake_backend(characters_status: u16) -> FakeServer {
    let reply: Reply = Arc::new(move |seen: &Seen| {
        let (status, body) = match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/api/characters") if characters_status != 200 => (characters_status, json!({ "error": "down" })),
            ("GET", "/api/characters") => (200, json!({ "characters": employees() })),
            (_, path) if path.starts_with("/api/roleplay/profiles/") => (404, json!({ "error": "없음" })),
            ("GET", path) if path.starts_with("/api/agent-jobs?") => (200, json!({ "employees": [{ "canReceive": true }] })),
            ("POST", "/api/agent-jobs") => (202, json!({ "turnId": "turn-1", "conversationId": "conversation-1" })),
            ("GET", path) if path.starts_with("/api/live-feed/") => (200, json!({ "turn": { "status": "completed", "response": "끝" } })),
            _ => (404, json!({})),
        };
        async move { (status, body.to_string()) }.boxed()
    });
    fake_server(Arc::default(), reply).await
}

fn owner_update(id: i64, text: &str) -> Value {
    json!({ "update_id": id, "message": { "message_id": id, "date": now_milliseconds() / 1000.0, "chat": { "id": 555, "type": "private" }, "from": { "id": 555 }, "text": text } })
}

fn telegram_methods(server: &FakeServer) -> Vec<(String, Value)> {
    server
        .seen()
        .into_iter()
        .map(|seen| {
            let method = seen.path.rsplit('/').next().unwrap_or_default().to_string();
            assert_eq!(seen.path, format!("/bot{CLI_TOKEN}/{method}"));
            (method, serde_json::from_str(&seen.body).unwrap())
        })
        .collect()
}

fn assert_no_token(lines: &Lines) {
    assert!(lines.lock().unwrap().iter().all(|(_, line)| !line.contains("fake-cli-token")));
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[tokio::test]
async fn cli_lock_and_startup_refusals_follow_node() {
    let (logger, lines) = capture();
    let last = |lines: &Lines| lines.lock().unwrap().last().cloned().unwrap();
    let workspace = Workspace::new(Some(CLI_TOKEN));
    let unused = closed_url().await;
    std::fs::create_dir_all(&workspace.directory).unwrap();
    let lock = workspace.file("telegram-bridge.pid");
    for content in [std::process::id().to_string(), String::new()] {
        std::fs::write(&lock, &content).unwrap();
        assert_eq!(run(workspace.options(&unused, BACKEND_URL, &[], logger.clone())).await, 1);
        assert_eq!(last(&lines), (LogLevel::Error, "Telegram bridge already running".to_string()));
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), content);
    }
    std::fs::write(&lock, "abc").unwrap();
    assert_eq!(run(workspace.options(&unused, BACKEND_URL, &[], logger.clone())).await, 1);
    assert_eq!(last(&lines).1, "Error: The \"pid\" argument must be of type number. Received type number (NaN)");
    assert!(std::path::Path::new(&lock).exists());

    std::fs::write(&lock, dead_pid().to_string()).unwrap();
    assert_eq!(run(workspace.options(&unused, BACKEND_URL, &[], logger.clone())).await, 1);
    assert_eq!(last(&lines).1, "Error: Pair the verified owner before starting Telegram bridge");
    assert!(!std::path::Path::new(&lock).exists());

    assert_eq!(run(workspace.options(&unused, "https://127.0.0.1:4317", &[], logger.clone())).await, 1);
    assert_eq!(last(&lines).1, "Error: Telegram bridge requires a loopback backend");
    let missing = Workspace::new(None);
    assert_eq!(run(missing.options(&unused, BACKEND_URL, &[], logger.clone())).await, 1);
    let configuration = missing.root.join("telegram.env").to_string_lossy().into_owned();
    assert_eq!(last(&lines).1, format!("Error: ENOENT: no such file or directory, open '{configuration}'"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&missing.directory).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(!std::path::Path::new(&missing.file("telegram-bridge.pid")).exists());

    let hooked = fake_telegram(Vec::new(), "https://hook.example/telegram").await;
    workspace.write_state(json!({ "offset": 0, "allowedChats": { "555": "555" }, "chats": {}, "pending": {} }));
    assert_eq!(run(workspace.options(&hooked.url, BACKEND_URL, &[], logger.clone())).await, 1);
    assert_eq!(last(&lines).1, "Error: Existing Telegram webhook detected; refusing to replace it");
    std::fs::write(workspace.file("telegram-state.json"), "{broken").unwrap();
    assert_eq!(run(workspace.options(&hooked.url, BACKEND_URL, &[], logger.clone())).await, 1);
    assert!(last(&lines).1.starts_with("SyntaxError: "));
    workspace.write_state(json!({ "offset": 0, "allowedChats": {} }));
    let plain = fake_telegram(Vec::new(), "").await;
    assert_eq!(run(workspace.options(&plain.url, BACKEND_URL, &[], logger.clone())).await, 1);
    assert_eq!(last(&lines).1, "Error: Explicit Telegram owner allowlist is required");
    assert!(!std::path::Path::new(&lock).exists());
    assert_no_token(&lines);
}

#[tokio::test]
async fn cli_pair_registers_exactly_one_recent_private_owner() {
    let (logger, lines) = capture();
    let now = now_milliseconds() / 1000.0;
    let start = |id: i64, chat: Value, from: Value, date: f64| {
        json!({ "update_id": id, "message": { "message_id": id, "date": date, "chat": chat, "from": from, "text": "/start" } })
    };
    let private = |id: i64| json!({ "id": id, "type": "private" });
    let workspace = Workspace::new(Some(CLI_TOKEN));
    let telegram = fake_telegram(
        vec![updates(json!([
            start(7, private(555), json!({ "id": 555 }), now),
            start(9, json!({ "id": -100, "type": "group" }), json!({ "id": 556 }), now),
            start(8, private(557), json!({ "id": 557, "is_bot": true }), now),
            { "update_id": 11, "message": { "chat": private(555), "from": { "id": 555 }, "text": "hello", "date": now } },
        ]))],
        "",
    )
    .await;
    assert_eq!(run(workspace.options(&telegram.url, BACKEND_URL, &["node", "office-telegram.mjs", "--pair"], logger.clone())).await, 0);
    assert_eq!(lines.lock().unwrap().last().unwrap().1, "Verified private Telegram owner paired");
    assert_eq!(
        std::fs::read_to_string(workspace.file("telegram-state.json")).unwrap(),
        "{\"offset\":12,\"allowedChats\":{\"555\":\"555\"},\"chats\":{},\"pending\":{}}\n"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(workspace.file("telegram-state.json")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(telegram_methods(&telegram), vec![("getUpdates".to_string(), json!({ "timeout": 0, "limit": 100 }))]);
    assert!(!std::path::Path::new(&workspace.file("telegram-bridge.pid")).exists());

    assert_eq!(run(workspace.options(&telegram.url, BACKEND_URL, &["--pair"], logger.clone())).await, 1);
    assert_eq!(lines.lock().unwrap().last().unwrap().1, "Error: Telegram state already exists; owner pairing is never overwritten");

    for result in [
        json!([start(1, private(1), json!({ "id": 1 }), now), start(2, private(2), json!({ "id": 2 }), now)]),
        json!([start(1, private(1), json!({ "id": 1 }), now - 7300.0)]),
        json!([]),
    ] {
        let fresh = Workspace::new(Some(CLI_TOKEN));
        let telegram = fake_telegram(vec![updates(result)], "").await;
        assert_eq!(run(fresh.options(&telegram.url, BACKEND_URL, &["--pair"], logger.clone())).await, 1);
        assert_eq!(lines.lock().unwrap().last().unwrap().1, "Error: Exactly one recent verified owner /start is required");
        assert!(!std::path::Path::new(&fresh.file("telegram-state.json")).exists());
    }
    assert_no_token(&lines);
}

#[tokio::test]
async fn cli_announce_registers_commands_and_sends_menu() {
    let (logger, lines) = capture();
    let workspace = Workspace::new(Some(CLI_TOKEN));
    workspace.write_state(json!({ "offset": 3, "allowedChats": { "555": "555" }, "chats": {}, "pending": {} }));
    let before = std::fs::read_to_string(workspace.file("telegram-state.json")).unwrap();
    let telegram = fake_telegram(Vec::new(), "").await;
    let backend = fake_backend(200).await;
    assert_eq!(run(workspace.options(&telegram.url, &backend.url, &["--announce"], logger.clone())).await, 0);
    assert_eq!(lines.lock().unwrap().last().unwrap().1, "Telegram employee menu sent");
    let methods = telegram_methods(&telegram);
    assert_eq!(methods.iter().map(|(method, _)| method.as_str()).collect::<Vec<_>>(), ["getWebhookInfo", "setMyCommands", "sendMessage"]);
    assert_eq!(methods[0].1, json!({}));
    assert_eq!(
        methods[1].1,
        json!({ "commands": [
            { "command": "office", "description": "대화할 직원 선택" },
            { "command": "status", "description": "직원별 현재 작업 상태" },
            { "command": "new", "description": "선택한 직원과 새 대화" },
        ] })
    );
    assert_eq!(methods[2].1["chat_id"], json!("555"));
    assert_eq!(methods[2].1["reply_markup"]["inline_keyboard"][0][0], json!({ "text": "✓ 백부장", "callback_data": "staff:boss" }));
    assert_eq!(std::fs::read_to_string(workspace.file("telegram-state.json")).unwrap(), before);
    assert_no_token(&lines);
}

#[tokio::test]
async fn cli_polls_dispatches_delivers_and_exits_on_conflict() {
    let (logger, lines) = capture();
    let workspace = Workspace::new(Some(CLI_TOKEN));
    workspace.write_state(json!({ "offset": 0, "allowedChats": { "555": "555" }, "chats": {}, "pending": {} }));
    let telegram = fake_telegram(
        vec![updates(json!([owner_update(1, "질문"), owner_update(2, "hello")["message"]["text"]])), Scripted::After("editMessageText", 409, conflict())],
        "",
    )
    .await;
    let backend = fake_backend(200).await;
    assert_eq!(run(workspace.options(&telegram.url, &backend.url, &[], logger.clone())).await, 0);
    let logged: Vec<String> = lines.lock().unwrap().iter().map(|(_, line)| line.clone()).collect();
    assert!(logged.contains(&"OFFICESTRA Telegram bridge started".to_string()));
    assert!(logged.contains(&"Telegram turn delivered: turn-1".to_string()));
    assert_eq!(logged.last().unwrap(), "Telegram polling failed 409");
    let posted: Vec<Seen> = backend.seen().into_iter().filter(|seen| seen.method == "POST").collect();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].body, r#"{"characterId":"boss","requestSource":"remote","prompt":"질문"}"#);
    let methods = telegram_methods(&telegram);
    let polls: Vec<&Value> = methods.iter().filter(|(method, _)| method == "getUpdates").map(|(_, body)| body).collect();
    assert_eq!(polls[0], &json!({ "offset": 0, "timeout": 25, "allowed_updates": ["message", "callback_query"] }));
    assert_eq!(polls[1]["offset"], json!(2));
    let sent: Vec<&Value> = methods.iter().filter(|(method, _)| method == "sendMessage").map(|(_, body)| body).collect();
    assert_eq!(sent[0], &json!({ "chat_id": "555", "text": "⏳ 백부장에게 전달합니다.", "reply_parameters": { "message_id": 1 } }));
    assert_eq!(sent[1], &json!({ "chat_id": "555", "text": "[백부장]\n끝" }));
    let edit = methods.iter().find(|(method, _)| method == "editMessageText").unwrap();
    assert_eq!(edit.1, json!({ "chat_id": "555", "message_id": 1, "text": "✅ 백부장: 완료" }));
    let state = workspace.state();
    assert_eq!((state["offset"].clone(), state["pending"].clone()), (json!(2), json!({})));
    assert_eq!(state["chats"]["555"]["conversations"], json!({ "boss": "conversation-1" }));
    assert_eq!(state["chats"]["555"]["routes"], json!({ "1": "boss", "2": "boss" }));
    assert!(!std::path::Path::new(&workspace.file("telegram-bridge.pid")).exists());
    assert_no_token(&lines);
}

#[tokio::test]
async fn cli_retries_failed_polls_and_answers_owner_after_handling_error() {
    let (logger, lines) = capture();
    let workspace = Workspace::new(Some(CLI_TOKEN));
    workspace.write_state(json!({ "offset": 0, "allowedChats": { "555": "555" }, "chats": {}, "pending": {} }));
    let telegram = fake_telegram(
        vec![
            Scripted::Reply(500, json!({ "ok": false })),
            Scripted::Reply(429, json!({ "ok": false, "parameters": { "retry_after": 0.02 } })),
            updates(json!([owner_update(1, "/office"), null])),
            Scripted::Reply(401, json!({ "ok": false })),
        ],
        "",
    )
    .await;
    let backend = fake_backend(500).await;
    assert_eq!(run(workspace.options(&telegram.url, &backend.url, &[], logger.clone())).await, 0);
    let logged: Vec<String> = lines.lock().unwrap().iter().map(|(_, line)| line.clone()).collect();
    assert_eq!(
        logged,
        [
            "OFFICESTRA Telegram bridge started",
            "Telegram polling failed 500",
            "Telegram polling failed 429",
            "Telegram update handling failed 500",
            "Telegram update handling failed connection",
            "Telegram polling failed connection",
            "Telegram polling failed 401",
        ]
    );
    let methods = telegram_methods(&telegram);
    let fallback = methods.iter().find(|(method, _)| method == "sendMessage").unwrap();
    assert_eq!(fallback.1, json!({ "chat_id": 555, "text": "연결 문제로 요청 처리를 확인하지 못했습니다. /status와 앱 기록을 확인한 뒤 다시 보내주세요." }));
    assert_eq!(workspace.state()["offset"], json!(2));
    assert!(!std::path::Path::new(&workspace.file("telegram-bridge.pid")).exists());
}

#[tokio::test]
async fn cli_tick_failure_ends_like_unhandled_rejection() {
    let (logger, lines) = capture();
    let workspace = Workspace::new(Some(CLI_TOKEN));
    workspace.write_state(json!({ "offset": 0, "allowedChats": { "555": "555" }, "chats": {}, "pending": { "1": null } }));
    let telegram = fake_telegram(vec![Scripted::After("never", 409, conflict())], "").await;
    let backend = fake_backend(200).await;
    let started = std::time::Instant::now();
    assert_eq!(run(workspace.options(&telegram.url, &backend.url, &[], logger.clone())).await, 1);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(lines.lock().unwrap().last().unwrap().1, "Error: Cannot read properties of null (reading 'stage')");
    assert!(!std::path::Path::new(&workspace.file("telegram-bridge.pid")).exists());
}
