// GUI Codex 턴 하나를 codex app-server로 돌린다. 알림을 exec --json과 같은 줄로 바꿔 턴 상태에 넘기고,
// 실행 중 끼어들기(turn/steer)를 받는다. 프로세스는 턴마다 새로 띄우고 턴이 끝나면 입력을 닫아 끝낸다.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

use super::lifecycle::LineSplitter;
use crate::claude_persistent_worker::STEER_DELIVERED_EVENT;
use crate::js::{js_string, js_truthy, parse_js_json, to_js_json};

const SERVER_REQUEST_REFUSED: &str = "OFFICESTRA GUI 업무는 Codex의 승인·입력 요청에 답하지 않습니다.";
const TURN_ENDED: &str = "Codex 업무가 이미 끝나 끼어들기를 전달하지 못했습니다.";

/// 실행 중인 Codex 턴에 끼워 넣을 메시지. 답은 Codex가 turn/steer를 받아들였는지다.
pub struct CodexSteer {
    pub steer_id: String,
    pub input: Vec<Value>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// 턴 시작 요청. thread_id가 있으면 그 스레드를 이어 간다.
#[derive(Debug, Clone)]
pub struct CodexTurnRequest {
    pub thread_id: Option<String>,
    pub cwd: String,
    pub sandbox: String,
    pub input: Vec<Value>,
}

/// 질문과 Codex 이미지 첨부를 app-server 입력 목록으로 만든다.
pub fn codex_turn_input(prompt: &str, image_paths: &[String]) -> Vec<Value> {
    let mut input = vec![json!({ "type": "text", "text": prompt })];
    input.extend(image_paths.iter().map(|path| json!({ "type": "localImage", "path": path })));
    input
}

#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Initialize,
    Thread,
    Turn,
    Steer(String),
}

/// 세션이 내보내는 일. Send는 app-server 입력, Line은 exec --json 모양의 턴 상태 입력이다.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Output {
    Send(Value),
    Line(Value),
}

/// JSON-RPC 대화 상태. 입출력 없이 메시지를 받아 할 일을 쌓아서 시험할 수 있게 한다.
pub(crate) struct Session {
    request: CodexTurnRequest,
    thread_id: Option<String>,
    turn_id: Option<String>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    queued_steers: Vec<CodexSteer>,
    steer_replies: HashMap<String, oneshot::Sender<Result<(), String>>>,
    usage: Option<Value>,
    pub(crate) done: bool,
}

impl Session {
    pub(crate) fn new(request: CodexTurnRequest) -> Self {
        Self {
            request,
            thread_id: None,
            turn_id: None,
            next_id: 1,
            pending: HashMap::new(),
            queued_steers: Vec::new(),
            steer_replies: HashMap::new(),
            usage: None,
            done: false,
        }
    }

    pub(crate) fn start(&mut self, out: &mut Vec<Output>) {
        let params = json!({
            "clientInfo": { "name": "officestra", "title": "OFFICESTRA", "version": "1" },
            "capabilities": { "experimentalApi": false, "requestAttestation": false },
        });
        self.request("initialize", params, Pending::Initialize, out);
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending, out: &mut Vec<Output>) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        out.push(Output::Send(json!({ "id": id, "method": method, "params": params })));
    }

    /// 끼어들기. 턴 ID를 아직 모르면 turn/start 응답 뒤에 보낸다.
    pub(crate) fn steer(&mut self, steer: CodexSteer, out: &mut Vec<Output>) {
        if self.done {
            let _ = steer.reply.send(Err(TURN_ENDED.into()));
            return;
        }
        let (Some(thread_id), Some(turn_id)) = (self.thread_id.clone(), self.turn_id.clone()) else {
            self.queued_steers.push(steer);
            return;
        };
        let params = json!({
            "threadId": thread_id,
            "expectedTurnId": turn_id,
            "clientUserMessageId": steer.steer_id,
            "input": steer.input,
        });
        self.steer_replies.insert(steer.steer_id.clone(), steer.reply);
        self.request("turn/steer", params, Pending::Steer(steer.steer_id), out);
    }

    pub(crate) fn handle_message(&mut self, message: &Value, out: &mut Vec<Output>) {
        let Some(fields) = message.as_object() else {
            return;
        };
        let method = fields.get("method").and_then(Value::as_str);
        match (fields.get("id"), method) {
            // 승인·사용자 입력 요청. exec처럼 묻지 않고 거절한다.
            (Some(id), Some(_)) => out.push(Output::Send(
                json!({ "id": id, "error": { "code": -32601, "message": SERVER_REQUEST_REFUSED } }),
            )),
            (Some(id), None) => self.handle_response(id, fields, out),
            (None, Some(method)) => self.handle_notification(method, fields.get("params").unwrap_or(&Value::Null), out),
            (None, None) => {}
        }
    }

    fn handle_response(&mut self, id: &Value, fields: &Map<String, Value>, out: &mut Vec<Output>) {
        let Some(pending) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
            return;
        };
        let error = fields.get("error").filter(|error| js_truthy(Some(error))).map(|error| {
            error.get("message").filter(|message| !message.is_null()).map_or_else(|| "Codex 요청이 실패했습니다.".to_string(), js_string)
        });
        let result = fields.get("result").unwrap_or(&Value::Null);
        match pending {
            Pending::Steer(steer_id) => {
                if let Some(reply) = self.steer_replies.remove(&steer_id) {
                    let _ = reply.send(error.map_or(Ok(()), Err));
                }
            }
            _ if error.is_some() => self.fail(error.unwrap_or_default(), out),
            Pending::Initialize => {
                out.push(Output::Send(json!({ "method": "initialized" })));
                let mut params = json!({ "cwd": self.request.cwd, "sandbox": self.request.sandbox, "approvalPolicy": "never" });
                let method = match &self.request.thread_id {
                    Some(thread_id) => {
                        params["threadId"] = json!(thread_id);
                        "thread/resume"
                    }
                    None => "thread/start",
                };
                self.request(method, params, Pending::Thread, out);
            }
            Pending::Thread => {
                let Some(thread_id) = text(result.get("thread").and_then(|thread| thread.get("id"))) else {
                    return self.fail("Codex가 스레드 ID를 알려 주지 않았습니다.".into(), out);
                };
                self.thread_id = Some(thread_id.clone());
                out.push(Output::Line(json!({ "type": "thread.started", "thread_id": thread_id })));
                let params = json!({ "threadId": thread_id, "input": self.request.input });
                self.request("turn/start", params, Pending::Turn, out);
            }
            Pending::Turn => {
                // 응답보다 turn/started 알림이 먼저 왔으면 그 값을 쓴다.
                if self.turn_id.is_none() {
                    self.turn_id = text(result.get("turn").and_then(|turn| turn.get("id")));
                }
                for steer in std::mem::take(&mut self.queued_steers) {
                    self.steer(steer, out);
                }
            }
        }
    }

    fn handle_notification(&mut self, method: &str, params: &Value, out: &mut Vec<Output>) {
        // 협업 하위 스레드의 알림도 같은 통로로 온다. exec --json처럼 이 턴의 스레드만 본다.
        if self.thread_id.is_none() || text(params.get("threadId")) != self.thread_id {
            return;
        }
        match method {
            "turn/started" => {
                if self.turn_id.is_none() {
                    self.turn_id = text(params.get("turn").and_then(|turn| turn.get("id")));
                }
            }
            "item/started" | "item/completed" => {
                let Some(item) = params.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("userMessage") {
                    if method == "item/started"
                        && let Some(steer_id) = text(item.get("clientId"))
                    {
                        out.push(Output::Line(json!({ "type": STEER_DELIVERED_EVENT, "steerId": steer_id })));
                    }
                    return;
                }
                if let Some(item) = exec_item(item) {
                    let kind = if method == "item/started" { "item.started" } else { "item.completed" };
                    out.push(Output::Line(json!({ "type": kind, "item": item })));
                }
            }
            "turn/plan/updated" => {
                let entries: Vec<Value> = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|entry| json!({ "text": entry.get("step").cloned().unwrap_or(Value::Null), "completed": entry.get("status") == Some(&json!("completed")) }))
                    .collect();
                let id = format!("plan:{}", text(params.get("turnId")).or_else(|| self.turn_id.clone()).unwrap_or_default());
                out.push(Output::Line(json!({ "type": "item.completed", "item": { "id": id, "type": "todo_list", "items": entries } })));
            }
            "thread/tokenUsage/updated" => {
                if let Some(total) = params.get("tokenUsage").and_then(|usage| usage.get("total")) {
                    self.usage = Some(exec_usage(total));
                }
            }
            "error" if params.get("willRetry") == Some(&Value::Bool(true)) => {
                let message = params.get("error").and_then(|error| error.get("message")).cloned().unwrap_or(Value::Null);
                out.push(Output::Line(json!({ "type": "error", "message": message })));
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&Value::Null);
                if self.turn_id.is_some() && text(turn.get("id")) != self.turn_id {
                    return;
                }
                let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed");
                let mut completed = json!({ "type": "turn.completed" });
                if let Some(usage) = self.usage.take() {
                    completed["usage"] = usage;
                }
                out.push(Output::Line(completed));
                if status != "completed" {
                    let fallback = if status == "interrupted" { "Codex 작업이 중단됐습니다." } else { "Codex 작업이 완료되지 못했습니다." };
                    let message = turn.get("error").and_then(|error| error.get("message")).and_then(Value::as_str).unwrap_or(fallback);
                    out.push(Output::Line(json!({ "type": "turn.failed", "error": { "message": message } })));
                }
                self.finish();
            }
            _ => {}
        }
    }

    fn fail(&mut self, message: String, out: &mut Vec<Output>) {
        out.push(Output::Line(json!({ "type": "turn.failed", "error": { "message": message } })));
        self.finish();
    }

    /// 턴이 끝났다. 아직 답을 못 받은 끼어들기는 전달 실패로 돌려준다.
    pub(crate) fn finish(&mut self) {
        self.done = true;
        for steer in std::mem::take(&mut self.queued_steers) {
            let _ = steer.reply.send(Err(TURN_ENDED.into()));
        }
        for (_, reply) in std::mem::take(&mut self.steer_replies) {
            let _ = reply.send(Err(TURN_ENDED.into()));
        }
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

/// app-server ThreadItem을 exec --json item 모양으로 바꾼다. 화면에 안 쓰는 종류는 버린다.
fn exec_item(item: &Value) -> Option<Value> {
    let kind = match item.get("type").and_then(Value::as_str)? {
        "agentMessage" => "agent_message",
        "reasoning" => "reasoning",
        "commandExecution" => "command_execution",
        "fileChange" => "file_change",
        "mcpToolCall" => "mcp_tool_call",
        "collabAgentToolCall" => "collab_tool_call",
        "webSearch" => "web_search",
        _ => return None,
    };
    let mut item = item.clone();
    let fields = item.as_object_mut()?;
    fields.insert("type".into(), json!(kind));
    if kind == "file_change"
        && let Some(Value::Array(changes)) = fields.get_mut("changes")
    {
        // exec는 kind가 "add"·"update"·"delete" 글자다. app-server는 { type, move_path } 객체다.
        for change in changes.iter_mut() {
            if let Some(kind) = change.get("kind").and_then(|kind| kind.get("type")).cloned() {
                change["kind"] = kind;
            }
        }
    }
    if kind == "collab_tool_call"
        && let Some(Value::Object(states)) = fields.get_mut("agentsStates")
    {
        for state in states.values_mut() {
            if let Some(status) = state.get("status").and_then(Value::as_str).map(snake_case) {
                state["status"] = json!(status);
            }
        }
    }
    Some(item)
}

fn snake_case(text: &str) -> String {
    let mut snake = String::with_capacity(text.len() + 4);
    for character in text.chars() {
        if character.is_ascii_uppercase() {
            snake.push('_');
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
    }
    snake
}

/// thread/tokenUsage/updated의 누적 사용량을 exec turn.completed usage 칸 이름으로 바꾼다.
fn exec_usage(total: &Value) -> Value {
    let mut usage = Map::new();
    for (camel, snake) in [
        ("inputTokens", "input_tokens"),
        ("cachedInputTokens", "cached_input_tokens"),
        ("cacheWriteInputTokens", "cache_write_input_tokens"),
        ("outputTokens", "output_tokens"),
        ("reasoningOutputTokens", "reasoning_output_tokens"),
    ] {
        if let Some(value) = total.get(camel).filter(|value| !value.is_null()) {
            usage.insert(snake.into(), value.clone());
        }
    }
    Value::Object(usage)
}

/// app-server와 한 턴을 주고받는다. exec 모양 줄은 on_line에 넘기고, 처리 오류가 나면 나머지 출력은 읽어 버리고 첫 오류를 돌려준다.
/// Ok(false)는 턴 완료 알림 없이 app-server가 끝났다는 뜻이다.
pub(crate) async fn drive<F, Fut>(
    stdin: Option<ChildStdin>,
    mut stdout: impl AsyncRead + Unpin,
    request: CodexTurnRequest,
    mut steers: mpsc::UnboundedReceiver<CodexSteer>,
    mut on_line: F,
) -> Result<bool, String>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut stdin = stdin;
    let mut session = Session::new(request);
    let mut splitter = LineSplitter::default();
    let mut failure: Option<String> = None;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut out = Vec::new();
    let mut steers_open = true;
    let mut ended = false;
    session.start(&mut out);
    loop {
        for output in out.drain(..) {
            match output {
                Output::Send(message) => {
                    if let Some(pipe) = stdin.as_mut()
                        && write_message(pipe, &message).await.is_err()
                    {
                        // 입력이 닫혔다면 app-server가 끝난 것이다. 남은 출력을 읽고 종료로 처리한다.
                        stdin = None;
                    }
                }
                Output::Line(line) => {
                    let text = String::from_utf8(to_js_json(&line).map_err(|error| error.to_string())?).unwrap_or_default();
                    if failure.is_none()
                        && let Err(error) = on_line(text).await
                    {
                        failure = Some(error);
                    }
                }
            }
        }
        if ended {
            break;
        }
        if session.done {
            // 입력을 닫으면 app-server가 끝나 출력도 닫힌다.
            stdin = None;
        }
        tokio::select! {
            read = stdout.read(&mut buffer) => {
                let count = match read {
                    Ok(count) => count,
                    Err(error) => {
                        failure.get_or_insert(error.to_string());
                        0
                    }
                };
                let lines = if count == 0 {
                    ended = true;
                    splitter.finish().into_iter().collect()
                } else {
                    splitter.push(&buffer[..count])
                };
                for line in lines {
                    if let Ok(message) = parse_js_json(&line) {
                        session.handle_message(&message, &mut out);
                    }
                }
                if ended {
                    // 끝난 뒤에는 보낼 곳이 없으니 남은 줄 처리만 한 번 더 돈다.
                    out.retain(|output| matches!(output, Output::Line(_)));
                }
            }
            steer = steers.recv(), if steers_open => match steer {
                Some(steer) => session.steer(steer, &mut out),
                None => steers_open = false,
            },
        }
    }
    let completed = session.done;
    session.finish();
    // 채널에 남은 끼어들기에도 답한다.
    steers.close();
    while let Ok(steer) = steers.try_recv() {
        let _ = steer.reply.send(Err(TURN_ENDED.into()));
    }
    failure.map_or(Ok(completed), Err)
}

async fn write_message(stdin: &mut ChildStdin, message: &Value) -> std::io::Result<()> {
    let mut line = to_js_json(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await
}

#[cfg(test)]
mod tests;
