// Codex app-server 대화 상태 시험. 입출력 없이 응답·알림을 넣고 보낼 요청과 exec 모양 줄을 보고,
// 가짜 app-server 스크립트로 입출력 순서도 본다. 실제 Codex 시험은 #[ignore]로 두고 손으로 돌린다.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use super::*;

/// 요청마다 정해 둔 응답을 낸다. MODE=early면 turn/start 뒤 바로 끝난다.
const FAKE_APP_SERVER: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"id":1,"result":{}}' ;;
    *'"method":"thread/start"'*) echo '{"id":2,"result":{"thread":{"id":"t1"}}}' ;;
    *'"method":"turn/start"'*)
      echo '{"id":3,"result":{"turn":{"id":"u1"}}}'
      if [ "$MODE" = early ]; then exit 3; fi
      echo '{"method":"item/started","params":{"threadId":"t1","item":{"type":"commandExecution","id":"c1","command":"sleep 1"}}}' ;;
    *'"method":"turn/steer"'*)
      echo '{"id":4,"result":{"turnId":"u1"}}'
      echo '{"method":"item/started","params":{"threadId":"t1","item":{"type":"userMessage","id":"m2","clientId":"s1"}}}'
      echo '{"method":"item/completed","params":{"threadId":"t1","item":{"type":"agentMessage","id":"a1","text":"끝"}}}'
      echo '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"completed"}}}' ;;
  esac
done
"#;

/// app-server를 띄워 한 턴을 돌린다. 명령 시작 줄이 오면 끼어들기 하나를 보낸다.
async fn run_turn(
    mut command: tokio::process::Command,
    request: CodexTurnRequest,
    steer_text: &str,
) -> (Result<bool, String>, Vec<Value>, Option<Result<(), String>>) {
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    let received = Arc::new(Mutex::new(Vec::new()));
    let answer: Arc<Mutex<Option<oneshot::Receiver<Result<(), String>>>>> = Arc::default();
    let (lines, steer_answer) = (Arc::clone(&received), Arc::clone(&answer));
    let steer_text = steer_text.to_string();
    let on_line = move |line: String| {
        let value: Value = serde_json::from_str(&line).unwrap();
        let command_started = value["type"] == "item.started" && value["item"]["type"] == "command_execution";
        lines.lock().unwrap().push(value);
        let mut slot = steer_answer.lock().unwrap();
        if command_started && slot.is_none() {
            let (reply, receiver) = oneshot::channel();
            let _ = sender.send(CodexSteer { steer_id: "s1".into(), input: codex_turn_input(&steer_text, &[]), reply });
            *slot = Some(receiver);
        }
        std::future::ready(Ok(()))
    };
    let result = drive(child.stdin.take(), child.stdout.take().unwrap(), request, receiver, on_line).await;
    let _ = child.wait().await;
    let receiver = answer.lock().unwrap().take();
    let steer = match receiver {
        Some(receiver) => Some(receiver.await.unwrap()),
        None => None,
    };
    let lines = received.lock().unwrap().clone();
    (result, lines, steer)
}

fn fake(mode: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/bin/sh");
    command.args(["-c", FAKE_APP_SERVER]).env("MODE", mode);
    command
}

#[tokio::test]
async fn 가짜_app_server와_끼어들기까지_한_턴을_주고받는다() {
    let (result, lines, steer) = run_turn(fake("ok"), request(None), "이것도").await;
    assert_eq!(result, Ok(true));
    assert_eq!(steer, Some(Ok(())));
    let kinds: Vec<&str> = lines.iter().map(|line| line["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["thread.started", "item.started", STEER_DELIVERED_EVENT, "item.completed", "turn.completed"]);
}

#[tokio::test]
async fn 턴_완료_없이_끝난_app_server는_미완료로_돌려준다() {
    let (result, lines, steer) = run_turn(fake("early"), request(None), "이것도").await;
    assert_eq!(result, Ok(false));
    assert_eq!(steer, None);
    assert_eq!(lines, vec![json!({ "type": "thread.started", "thread_id": "t1" })]);
}

/// 실제 Codex 시험. 토큰을 쓰므로 손으로만 돌린다.
/// `cargo test --offline -- --ignored 실제_codex로_끼어들기를_전달한다 --nocapture`
#[tokio::test]
#[ignore]
async fn 실제_codex로_끼어들기를_전달한다() {
    let directory = std::env::temp_dir().join(format!("officestra-codex-steer-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let cwd = directory.to_str().unwrap().to_string();
    let mut command = tokio::process::Command::new("codex");
    command.args(["app-server", "-c", "model_reasoning_effort=\"low\"", "--stdio"]).current_dir(&directory);
    let request = CodexTurnRequest {
        thread_id: None,
        cwd,
        sandbox: "workspace-write".into(),
        input: codex_turn_input("Run the shell command `sleep 6 && echo one`, then reply with its output in one line.", &[]),
    };
    let (result, lines, steer) = run_turn(command, request, "Also add the word BANANA at the end of your reply.").await;
    let _ = std::fs::remove_dir_all(&directory);
    for line in &lines {
        println!("{line}");
    }
    assert_eq!(result, Ok(true));
    assert_eq!(steer, Some(Ok(())));
    assert!(lines.iter().any(|line| line["type"] == STEER_DELIVERED_EVENT));
    let completed = lines.iter().find(|line| line["type"] == "turn.completed").unwrap();
    assert!(completed["usage"]["input_tokens"].as_u64().unwrap() > 0);
    let last_message = lines.iter().rev().find(|line| line["item"]["type"] == "agent_message").unwrap();
    assert!(last_message["item"]["text"].as_str().unwrap().contains("BANANA"));
}

fn request(thread_id: Option<&str>) -> CodexTurnRequest {
    CodexTurnRequest {
        thread_id: thread_id.map(str::to_string),
        cwd: "/work".into(),
        sandbox: "workspace-write".into(),
        input: codex_turn_input("질문", &["/work/a.png".into()]),
    }
}

fn sends(out: &[Output]) -> Vec<Value> {
    out.iter().filter_map(|output| if let Output::Send(value) = output { Some(value.clone()) } else { None }).collect()
}

fn lines(out: &[Output]) -> Vec<Value> {
    out.iter().filter_map(|output| if let Output::Line(value) = output { Some(value.clone()) } else { None }).collect()
}

/// initialize → thread → turn/start 응답까지 넣은 세션.
fn started(thread_id: Option<&str>) -> (Session, Vec<Output>) {
    let mut session = Session::new(request(thread_id));
    let mut out = Vec::new();
    session.start(&mut out);
    session.handle_message(&json!({ "id": 1, "result": { "userAgent": "test" } }), &mut out);
    session.handle_message(&json!({ "id": 2, "result": { "thread": { "id": "thread-1" } } }), &mut out);
    session.handle_message(&json!({ "id": 3, "result": { "turn": { "id": "turn-1", "status": "inProgress" } } }), &mut out);
    (session, out)
}

fn notify(session: &mut Session, method: &str, params: Value) -> Vec<Output> {
    let mut out = Vec::new();
    session.handle_message(&json!({ "method": method, "params": params }), &mut out);
    out
}

#[test]
fn 새_스레드는_시작_요청_순서대로_보내고_thread_started를_낸다() {
    let (_, out) = started(None);
    assert_eq!(
        sends(&out),
        vec![
            json!({ "id": 1, "method": "initialize", "params": {
                "clientInfo": { "name": "officestra", "title": "OFFICESTRA", "version": "1" },
                "capabilities": { "experimentalApi": false, "requestAttestation": false } } }),
            json!({ "method": "initialized" }),
            json!({ "id": 2, "method": "thread/start", "params": { "cwd": "/work", "sandbox": "workspace-write", "approvalPolicy": "never" } }),
            json!({ "id": 3, "method": "turn/start", "params": { "threadId": "thread-1", "input": [
                { "type": "text", "text": "질문" }, { "type": "localImage", "path": "/work/a.png" }] } }),
        ]
    );
    assert_eq!(lines(&out), vec![json!({ "type": "thread.started", "thread_id": "thread-1" })]);
}

#[test]
fn 이어_가는_스레드는_thread_resume으로_연다() {
    let (_, out) = started(Some("thread-1"));
    assert_eq!(
        sends(&out)[2],
        json!({ "id": 2, "method": "thread/resume", "params": { "cwd": "/work", "sandbox": "workspace-write", "approvalPolicy": "never", "threadId": "thread-1" } })
    );
}

#[test]
fn item_알림을_exec_item으로_바꾸고_다른_스레드와_첫_질문은_버린다() {
    let (mut session, _) = started(None);
    let out = notify(&mut session, "item/started", json!({ "threadId": "other", "item": { "type": "agentMessage", "id": "m0", "text": "남" } }));
    assert!(out.is_empty());
    let out = notify(&mut session, "item/started", json!({ "threadId": "thread-1", "item": { "type": "userMessage", "id": "u1", "clientId": null } }));
    assert!(out.is_empty());
    let out = notify(
        &mut session,
        "item/completed",
        json!({ "threadId": "thread-1", "item": { "type": "commandExecution", "id": "c1", "command": "ls", "exitCode": 0, "status": "completed" } }),
    );
    assert_eq!(
        lines(&out),
        vec![json!({ "type": "item.completed", "item": { "type": "command_execution", "id": "c1", "command": "ls", "exitCode": 0, "status": "completed" } })]
    );
    let out = notify(
        &mut session,
        "item/completed",
        json!({ "threadId": "thread-1", "item": { "type": "fileChange", "id": "f1", "status": "completed", "changes": [{ "path": "/work/a.txt", "kind": { "type": "add" }, "diff": "" }] } }),
    );
    assert_eq!(lines(&out)[0]["item"]["changes"][0]["kind"], json!("add"));
    let out = notify(
        &mut session,
        "item/completed",
        json!({ "threadId": "thread-1", "item": { "type": "collabAgentToolCall", "id": "k1", "tool": "wait", "agentsStates": { "t2": { "status": "pendingInit" } } } }),
    );
    assert_eq!(lines(&out)[0]["item"]["agentsStates"]["t2"]["status"], json!("pending_init"));
    let out = notify(&mut session, "item/completed", json!({ "threadId": "thread-1", "item": { "type": "contextCompaction", "id": "x" } }));
    assert!(out.is_empty());
}

#[test]
fn 계획은_todo_list로_재시도_오류는_경고로_바꾼다() {
    let (mut session, _) = started(None);
    let out = notify(
        &mut session,
        "turn/plan/updated",
        json!({ "threadId": "thread-1", "turnId": "turn-1", "plan": [{ "step": "읽기", "status": "completed" }, { "step": "고치기", "status": "inProgress" }] }),
    );
    assert_eq!(
        lines(&out),
        vec![json!({ "type": "item.completed", "item": { "id": "plan:turn-1", "type": "todo_list", "items": [
            { "text": "읽기", "completed": true }, { "text": "고치기", "completed": false }] } })]
    );
    let out = notify(&mut session, "error", json!({ "threadId": "thread-1", "willRetry": true, "error": { "message": "다시 시도" } }));
    assert_eq!(lines(&out), vec![json!({ "type": "error", "message": "다시 시도" })]);
}

#[test]
fn 턴_완료는_마지막_누적_사용량을_exec_칸_이름으로_낸다() {
    let (mut session, _) = started(None);
    let usage = |input: u64| {
        json!({ "threadId": "thread-1", "turnId": "turn-1", "tokenUsage": { "total": {
            "totalTokens": input + 5, "inputTokens": input, "cachedInputTokens": 10, "cacheWriteInputTokens": 0, "outputTokens": 5, "reasoningOutputTokens": 1 } } })
    };
    notify(&mut session, "thread/tokenUsage/updated", usage(100));
    notify(&mut session, "thread/tokenUsage/updated", usage(300));
    let out = notify(&mut session, "turn/completed", json!({ "threadId": "thread-1", "turn": { "id": "turn-1", "status": "completed" } }));
    assert_eq!(
        lines(&out),
        vec![json!({ "type": "turn.completed", "usage": {
            "input_tokens": 300, "cached_input_tokens": 10, "cache_write_input_tokens": 0, "output_tokens": 5, "reasoning_output_tokens": 1 } })]
    );
    assert!(session.done);
}

#[test]
fn 실패한_턴은_turn_failed를_낸다() {
    let (mut session, _) = started(None);
    let out = notify(
        &mut session,
        "turn/completed",
        json!({ "threadId": "thread-1", "turn": { "id": "turn-1", "status": "failed", "error": { "message": "한도 초과" } } }),
    );
    assert_eq!(lines(&out), vec![json!({ "type": "turn.completed" }), json!({ "type": "turn.failed", "error": { "message": "한도 초과" } })]);
}

#[test]
fn 요청_오류와_서버_요청을_처리한다() {
    let mut session = Session::new(request(None));
    let mut out = Vec::new();
    session.start(&mut out);
    session.handle_message(&json!({ "id": 1, "error": { "message": "로그인이 필요합니다." } }), &mut out);
    assert_eq!(lines(&out), vec![json!({ "type": "turn.failed", "error": { "message": "로그인이 필요합니다." } })]);
    assert!(session.done);

    let (mut session, _) = started(None);
    let mut out = Vec::new();
    session.handle_message(&json!({ "id": "srv-1", "method": "item/commandExecution/requestApproval", "params": {} }), &mut out);
    assert_eq!(sends(&out), vec![json!({ "id": "srv-1", "error": { "code": -32601, "message": SERVER_REQUEST_REFUSED } })]);
}

#[tokio::test]
async fn 끼어들기는_턴_번호를_안_뒤_보내고_받아_간_시점을_알린다() {
    let mut session = Session::new(request(None));
    let mut out = Vec::new();
    session.start(&mut out);
    let (reply, answer) = oneshot::channel();
    session.steer(CodexSteer { steer_id: "s1".into(), input: codex_turn_input("이것도", &[]), reply }, &mut out);
    assert_eq!(sends(&out).len(), 1, "턴 ID 전에는 보내지 않는다");
    session.handle_message(&json!({ "id": 1, "result": {} }), &mut out);
    session.handle_message(&json!({ "id": 2, "result": { "thread": { "id": "thread-1" } } }), &mut out);
    session.handle_message(&json!({ "id": 3, "result": { "turn": { "id": "turn-1" } } }), &mut out);
    assert_eq!(
        sends(&out).last().unwrap(),
        &json!({ "id": 4, "method": "turn/steer", "params": {
            "threadId": "thread-1", "expectedTurnId": "turn-1", "clientUserMessageId": "s1", "input": [{ "type": "text", "text": "이것도" }] } })
    );
    session.handle_message(&json!({ "id": 4, "result": { "turnId": "turn-1" } }), &mut out);
    assert_eq!(answer.await.unwrap(), Ok(()));
    let out = notify(&mut session, "item/started", json!({ "threadId": "thread-1", "item": { "type": "userMessage", "id": "u2", "clientId": "s1" } }));
    assert_eq!(lines(&out), vec![json!({ "type": STEER_DELIVERED_EVENT, "steerId": "s1" })]);
}

#[tokio::test]
async fn 거절되거나_턴이_끝난_뒤의_끼어들기는_실패로_답한다() {
    let (mut session, _) = started(None);
    let mut out = Vec::new();
    let (reply, rejected) = oneshot::channel();
    session.steer(CodexSteer { steer_id: "s1".into(), input: Vec::new(), reply }, &mut out);
    session.handle_message(&json!({ "id": 4, "error": { "message": "turn mismatch" } }), &mut out);
    assert_eq!(rejected.await.unwrap(), Err("turn mismatch".into()));
    assert!(!session.done, "끼어들기 거절은 턴을 끝내지 않는다");

    let (reply, unanswered) = oneshot::channel();
    session.steer(CodexSteer { steer_id: "s2".into(), input: Vec::new(), reply }, &mut out);
    notify(&mut session, "turn/completed", json!({ "threadId": "thread-1", "turn": { "id": "turn-1", "status": "completed" } }));
    assert_eq!(unanswered.await.unwrap(), Err(TURN_ENDED.into()));

    let (reply, late) = oneshot::channel();
    session.steer(CodexSteer { steer_id: "s3".into(), input: Vec::new(), reply }, &mut out);
    assert_eq!(late.await.unwrap(), Err(TURN_ENDED.into()));
}
