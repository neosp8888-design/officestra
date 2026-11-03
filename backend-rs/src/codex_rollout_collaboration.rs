// Codex 롤아웃의 내부 협업 호출과 회신을 공개 가능한 활동으로 잇는다. Node codex-rollout-collaboration.mjs를 옮겼다.

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::js::{is_js_whitespace, js_length, js_trim, parse_js_json, utf16_prefix};

const MAX_PROMPT_LENGTH: usize = 4_000;
const MAX_RESULT_LENGTH: usize = 12_000;
const MAX_TRACKED_CALLS: usize = 256;

const COLLABORATION_TOOLS: [&str; 6] =
    ["spawn_agent", "send_message", "followup_task", "interrupt_agent", "wait_agent", "list_agents"];

// 비유니코드 /i와 \b는 ASCII 기준이다.
static FAILED_OUTPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i-u:already exists|\berror\b|\bfailed\b|\bunable\b|\bcannot\b)").unwrap());
// JS m 플래그는 \r·U+2028·U+2029도 줄 끝으로 본다. 판정 전에 모두 \n으로 바꾸며, 이 글자들은 \s에도 들어 있어 판정이 같다.
static FINAL_ANSWER: LazyLock<Regex> = LazyLock::new(|| {
    let space = r"[\t\n\v\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
    Regex::new(&format!(r"(?m)^Message Type:{space}*FINAL_ANSWER{space}*$")).unwrap()
});

struct Call {
    name: String,
    task_name: Option<String>,
}

#[derive(Clone)]
struct Agent {
    thread_id: String,
    label: String,
    prompt: Option<String>,
    event_key: String,
}

/// 협업 호출 ID와 하위 에이전트 경로를 기억해 시작·회신·실패를 같은 카드로 잇는다.
#[derive(Default)]
pub struct CodexRolloutCollaborationTracker {
    // JS Map처럼 넣은 순서를 지켜 오래된 호출부터 버린다.
    calls: VecDeque<(String, Call)>,
    agents_by_path: HashMap<String, Agent>,
}

impl CodexRolloutCollaborationTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume(&mut self, record: &Value) -> Vec<Value> {
        let Some(payload) = record.get("payload").filter(|payload| payload.is_object()) else {
            return Vec::new();
        };
        let record_type = record.get("type").and_then(Value::as_str);
        match (record_type, payload.get("type").and_then(Value::as_str)) {
            (Some("response_item"), Some("function_call")) => self.consume_function_call(payload),
            (Some("event_msg"), Some("sub_agent_activity")) => self.consume_agent_activity(payload),
            (Some("response_item"), Some("function_call_output")) => self.consume_function_output(payload),
            (Some("response_item"), Some("agent_message")) => self.consume_agent_message(payload),
            _ => Vec::new(),
        }
    }

    fn call(&self, call_id: &str) -> Option<&Call> {
        self.calls.iter().find(|(id, _)| id == call_id).map(|(_, call)| call)
    }

    fn take_call(&mut self, call_id: &str) -> Option<Call> {
        let index = self.calls.iter().position(|(id, _)| id == call_id)?;
        self.calls.remove(index).map(|(_, call)| call)
    }

    fn consume_function_call(&mut self, payload: &Value) -> Vec<Value> {
        let Some(name) = clean_text(payload.get("name")).map(|name| name.replace('-', "_").to_lowercase()) else {
            return Vec::new();
        };
        if !COLLABORATION_TOOLS.contains(&name.as_str()) && payload.get("namespace") != Some(&json!("collaboration")) {
            return Vec::new();
        }
        let Some(call_id) = clean_text(payload.get("call_id")) else {
            return Vec::new();
        };
        let arguments = parsed_object(payload.get("arguments"));
        let call = Call { name, task_name: clean_text(arguments.as_ref().and_then(|arguments| arguments.get("task_name"))) };
        // Map.set은 이미 있는 열쇠의 자리를 지킨다.
        match self.calls.iter_mut().find(|(id, _)| *id == call_id) {
            Some((_, existing)) => *existing = call,
            None => self.calls.push_back((call_id, call)),
        }
        while self.calls.len() > MAX_TRACKED_CALLS {
            self.calls.pop_front();
        }
        Vec::new()
    }

    fn consume_agent_activity(&mut self, payload: &Value) -> Vec<Value> {
        let call_id = clean_text(payload.get("event_id"));
        let path = clean_text(payload.get("agent_path"));
        let Some((path, thread_id)) = path.clone().zip(clean_text(payload.get("agent_thread_id")).or(path)) else {
            return Vec::new();
        };
        let call = call_id.as_deref().and_then(|call_id| self.call(call_id));
        let call_name = call.map(|call| call.name.clone());
        let task_name = call.and_then(|call| call.task_name.clone()).unwrap_or_else(|| path_leaf(Some(&path)));
        let agent = self.remember_agent(&path, &thread_id, &task_name);
        let kind = clean_text(payload.get("kind")).map(|kind| kind.replace('-', "_").to_lowercase());
        if let Some(call_id) = &call_id {
            self.take_call(call_id);
        }
        match kind.as_deref() {
            Some("started") => vec![collaboration_activity(&agent, "spawn", "running", "running", None, None)],
            Some("interacted") if call_name.as_deref() == Some("followup_task") => {
                vec![collaboration_activity(&agent, "follow_up", "running", "running", Some("추가 검토 요청"), None)]
            }
            // send_message 본문은 암호화되어 있어 화면 활동으로 만들지 않는다.
            Some("interrupted") => vec![collaboration_activity(
                &agent,
                "result",
                "failed",
                "interrupted",
                None,
                Some("협업 검토가 중단되었습니다."),
            )],
            _ => Vec::new(),
        }
    }

    fn consume_function_output(&mut self, payload: &Value) -> Vec<Value> {
        let call_id = clean_text(payload.get("call_id"));
        let call = call_id.as_deref().and_then(|call_id| self.take_call(call_id));
        if let Some(call) = &call
            && call.name == "spawn_agent"
            && failed_function_output(payload.get("output"))
        {
            // 호출이 있으면 call_id도 있다.
            let thread_id = format!("failed:{}", call_id.unwrap_or_default());
            let task_name = call.task_name.clone().unwrap_or_else(|| "검토자".into());
            let agent = self.remember_agent(&format!("/root/{task_name}"), &thread_id, &task_name);
            return vec![collaboration_activity(
                &agent,
                "spawn",
                "failed",
                "errored",
                None,
                Some("협업 검토를 시작하지 못했습니다."),
            )];
        }
        if call.is_none_or(|call| call.name != "list_agents") {
            return Vec::new();
        }
        let output = parsed_object(payload.get("output"));
        let agents = output.as_ref().and_then(|output| output.get("agents")).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut activities = Vec::new();
        for entry in &agents {
            let Some(path) = clean_text(entry.get("agent_name")).filter(|path| path != "/root") else {
                continue;
            };
            let Some((status, agent_status, message)) = completed_agent_status(entry.get("agent_status")) else {
                continue;
            };
            let agent = self.known_agent(&path);
            activities.push(collaboration_activity(&agent, "result", status, agent_status, None, Some(&message)));
        }
        activities
    }

    fn consume_agent_message(&mut self, payload: &Value) -> Vec<Value> {
        let parts: Vec<String> = payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("type") == Some(&json!("input_text")))
            .filter_map(|item| clean_text(item.get("text")))
            .collect();
        let Some((sender, message)) = final_answer_payload(&parts.join("\n")) else {
            return Vec::new();
        };
        let Some(path) = sender.or_else(|| clean_text(payload.get("author"))).filter(|path| path != "/root") else {
            return Vec::new();
        };
        let agent = self.known_agent(&path);
        vec![collaboration_activity(&agent, "result", "completed", "completed", None, Some(&message))]
    }

    fn known_agent(&mut self, path: &str) -> Agent {
        match self.agents_by_path.get(path) {
            Some(agent) => agent.clone(),
            None => self.remember_agent(path, path, &path_leaf(Some(path))),
        }
    }

    /// 부르는 곳마다 thread_id·task_name이 있으므로 Node의 기존 값 대체(`??`)는 쓰이지 않는다.
    fn remember_agent(&mut self, path: &str, thread_id: &str, task_name: &str) -> Agent {
        let label = readable_task_name(task_name);
        let agent = Agent {
            thread_id: thread_id.to_string(),
            prompt: limited_text(Some(&label), MAX_PROMPT_LENGTH),
            label,
            event_key: format!("collaboration:rollout:{thread_id}"),
        };
        self.agents_by_path.insert(path.to_string(), agent.clone());
        agent
    }
}

/// collaborationActivity. prompt를 넘기지 않으면 에이전트 이름을 쓴다.
fn collaboration_activity(
    agent: &Agent,
    action: &str,
    status: &str,
    agent_status: &str,
    prompt: Option<&str>,
    message: Option<&str>,
) -> Value {
    let safe_prompt = match prompt {
        Some(prompt) => limited_text(Some(prompt), MAX_PROMPT_LENGTH),
        None => agent.prompt.as_deref().and_then(|prompt| limited_text(Some(prompt), MAX_PROMPT_LENGTH)),
    };
    let safe_message = message.and_then(|message| limited_text(Some(message), MAX_RESULT_LENGTH));
    let text = match (&safe_message, &safe_prompt) {
        (Some(message), _) => summary_text(message),
        (None, Some(prompt)) => prompt.clone(),
        (None, None) => collaboration_status_text(status).to_string(),
    };
    json!({
        "kind": "collaboration",
        "text": text,
        "eventKey": agent.event_key,
        "status": status,
        "preserveText": false,
        "messageScoped": false,
        "collaboration": {
            "action": action,
            "agentThreadId": agent.thread_id,
            "agentLabel": agent.label,
            "prompt": safe_prompt,
            "message": safe_message,
            "agentStatus": agent_status,
        },
    })
}

/// 하위 에이전트 회신의 머리말에서 보낸 경로를, `\nPayload:\n` 뒤에서 본문을 읽는다.
fn final_answer_payload(text: &str) -> Option<(Option<String>, String)> {
    if text.is_empty() || !FINAL_ANSWER.is_match(&text.replace(['\r', '\u{2028}', '\u{2029}'], "\n")) {
        return None;
    }
    const MARKER: &str = "\nPayload:\n";
    let marker_index = text.find(MARKER)?;
    let message = limited_text(Some(&text[marker_index + MARKER.len()..]), MAX_RESULT_LENGTH)?;
    Some((sender(&text[..marker_index]), message))
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `head.match(/^Sender:\s*(.+)$/m)?.[1]`을 cleanText한 값.
/// 첫 `Sender:` 줄 뒤에 공백이 아닌 글자가 있으면 그 글자부터 줄 끝까지가 잡힌다.
/// 뒤가 끝까지 공백뿐이면 공백만 잡히거나 맞지 않고, 어느 쪽이든 cleanText가 null이다.
fn sender(head: &str) -> Option<String> {
    let starts = std::iter::once(0).chain(head.char_indices().filter(|(_, c)| is_line_terminator(*c)).map(|(index, c)| index + c.len_utf8()));
    for start in starts {
        let Some(rest) = head[start..].strip_prefix("Sender:") else {
            continue;
        };
        let value = rest.trim_start_matches(is_js_whitespace);
        let end = value.find(is_line_terminator).unwrap_or(value.len());
        return clean_str(&value[..end]);
    }
    None
}

fn completed_agent_status(value: Option<&Value>) -> Option<(&'static str, &'static str, String)> {
    let value = value.filter(|value| value.is_object())?;
    [
        ("completed", "completed", "completed"),
        ("failed", "failed", "errored"),
        ("errored", "failed", "errored"),
        ("interrupted", "failed", "interrupted"),
    ]
    .into_iter()
    .find_map(|(key, status, agent_status)| {
        limited_text(value.get(key).and_then(Value::as_str), MAX_RESULT_LENGTH).map(|message| (status, agent_status, message))
    })
}

/// 객체면 그대로, 글자면 JSON으로 읽어 객체일 때만 쓴다.
fn parsed_object(value: Option<&Value>) -> Option<Value> {
    match value? {
        object @ Value::Object(_) => Some(object.clone()),
        Value::String(text) => parse_js_json(text).ok().filter(Value::is_object),
        _ => None,
    }
}

fn failed_function_output(value: Option<&Value>) -> bool {
    match value {
        Some(object @ Value::Object(_)) => {
            object.get("isError") == Some(&json!(true)) || clean_text(object.get("error")).is_some()
        }
        _ => clean_text(value).is_some_and(|text| FAILED_OUTPUT.is_match(&text)),
    }
}

/// `String(value ?? "").split("/").filter(Boolean).at(-1) ?? "검토자"`.
fn path_leaf(value: Option<&str>) -> String {
    value.unwrap_or_default().split('/').filter(|part| !part.is_empty()).next_back().unwrap_or("검토자").to_string()
}

fn readable_task_name(value: &str) -> String {
    let text = collapse_whitespace(&value.replace(['_', '-'], " "));
    let text = js_trim(&text);
    if text.is_empty() { "검토자".into() } else { text.to_string() }
}

/// `replace(/\s+/g, " ")`.
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_js_whitespace(c) {
            if !in_space {
                result.push(' ');
            }
            in_space = true;
        } else {
            result.push(c);
            in_space = false;
        }
    }
    result
}

/// cleanText. 글자가 아니거나 앞뒤 공백을 떼고 비면 None이다.
fn clean_text(value: Option<&Value>) -> Option<String> {
    clean_str(value?.as_str()?)
}

fn clean_str(text: &str) -> Option<String> {
    let text = js_trim(text);
    (!text.is_empty()).then(|| text.to_string())
}

fn limited_text(value: Option<&str>, limit: usize) -> Option<String> {
    let text = clean_str(value?)?.replace("\r\n", "\n");
    Some(if js_length(&text) <= limit { text } else { format!("{}…", utf16_prefix(&text, limit - 1)) })
}

fn summary_text(value: &str) -> String {
    let text = collapse_whitespace(value);
    let text = js_trim(&text);
    if js_length(text) <= 320 { text.to_string() } else { format!("{}…", utf16_prefix(text, 319)) }
}

fn collaboration_status_text(status: &str) -> &'static str {
    match status {
        "running" => "협업 검토 중",
        "failed" => "협업 검토에 실패했습니다.",
        _ => "협업 검토를 마쳤습니다.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, call_id: &str, arguments: Value) -> Value {
        let arguments = match arguments {
            Value::String(text) => text,
            other => other.to_string(),
        };
        json!({ "type": "response_item", "payload": {
            "type": "function_call", "name": name, "namespace": "collaboration", "call_id": call_id, "arguments": arguments,
        } })
    }

    fn activity(event_id: &str, thread: &str, path: &str, kind: &str) -> Value {
        json!({ "type": "event_msg", "payload": {
            "type": "sub_agent_activity", "event_id": event_id, "agent_thread_id": thread, "agent_path": path, "kind": kind,
        } })
    }

    fn agent_message(author: &str, lines: &[&str]) -> Value {
        json!({ "type": "response_item", "payload": {
            "type": "agent_message", "author": author, "content": [{ "type": "input_text", "text": lines.join("\n") }],
        } })
    }

    fn output(call_id: &str, output: &str) -> Value {
        json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": call_id, "output": output } })
    }

    fn encoded(value: &impl serde::Serialize) -> String {
        String::from_utf8(crate::js::to_js_json(value).unwrap()).unwrap()
    }

    #[test]
    fn 현재_롤아웃의_협업_시작과_최종_답변을_같은_카드로_잇는다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        let arguments = json!({ "task_name": "event_schema_review", "fork_turns": "all", "message": "gAAAAABencrypted-internal-prompt" });
        assert_eq!(tracker.consume(&call("spawn_agent", "call-spawn-1", arguments)), Vec::<Value>::new());
        let started = tracker.consume(&activity("call-spawn-1", "thread-review-1", "/root/event_schema_review", "started"));
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["status"], json!("running"));
        assert_eq!(started[0]["eventKey"], json!("collaboration:rollout:thread-review-1"));
        assert_eq!(started[0]["collaboration"]["agentLabel"], json!("event schema review"));
        assert_eq!(started[0]["collaboration"]["prompt"], json!("event schema review"));
        assert!(!encoded(&started).contains("gAAAAAB"));

        let mut message = agent_message(
            "/root/event_schema_review",
            &["Message Type: FINAL_ANSWER", "Task name: /root", "Sender: /root/event_schema_review", "Payload:", "실제 이벤트 형식 검토를 마쳤습니다."],
        );
        message["payload"]["recipient"] = json!("/root");
        let completed = tracker.consume(&message);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["status"], json!("completed"));
        assert_eq!(completed[0]["eventKey"], started[0]["eventKey"]);
        assert_eq!(completed[0]["collaboration"]["message"], json!("실제 이벤트 형식 검토를 마쳤습니다."));
        assert_eq!(completed[0]["collaboration"]["agentThreadId"], json!("thread-review-1"));
    }

    #[test]
    fn 암호화된_중간_협업_메시지는_화면_활동으로_만들지_않는다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        let mut message =
            agent_message("/root/reviewer", &["Message Type: MESSAGE", "Task name: /root", "Sender: /root/reviewer", "Payload:"]);
        message["payload"]["recipient"] = json!("/root");
        message["payload"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "encrypted_content", "encrypted_content": "gAAAAABprivate-progress" }));
        assert_eq!(tracker.consume(&message), Vec::<Value>::new());
    }

    #[test]
    fn 암호화된_send_message는_완료_카드를_다시_실행_중으로_만들지_않는다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        tracker.consume(&call("send_message", "call-follow-up", json!({ "target": "reviewer", "message": "gAAAAABprivate-follow-up" })));
        assert_eq!(
            tracker.consume(&activity("call-follow-up", "thread-reviewer", "/root/reviewer", "interacted")),
            Vec::<Value>::new()
        );
    }

    #[test]
    fn followup_task_재호출은_같은_카드를_실행_중으로_되돌린_뒤_다시_완료한다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        tracker.consume(&call("spawn_agent", "call-spawn-follow-up", json!({ "task_name": "reviewer" })));
        let started = tracker.consume(&activity("call-spawn-follow-up", "thread-reviewer", "/root/reviewer", "started")).remove(0);
        let first =
            tracker.consume(&agent_message("/root/reviewer", &["Message Type: FINAL_ANSWER", "Sender: /root/reviewer", "Payload:", "첫 검토 완료"])).remove(0);
        tracker.consume(&call("followup_task", "call-follow-up-task", json!({ "target": "reviewer", "message": "gAAAAABprivate-follow-up" })));
        let follow_up = tracker.consume(&activity("call-follow-up-task", "thread-reviewer", "/root/reviewer", "interacted")).remove(0);
        let second = tracker
            .consume(&agent_message("/root/reviewer", &["Message Type: FINAL_ANSWER", "Sender: /root/reviewer", "Payload:", "추가 검토 완료"]))
            .remove(0);
        assert_eq!(first["status"], json!("completed"));
        assert_eq!(follow_up["status"], json!("running"));
        assert_eq!(follow_up["collaboration"]["action"], json!("follow_up"));
        assert_eq!(follow_up["collaboration"]["prompt"], json!("추가 검토 요청"));
        assert!(!encoded(&follow_up).contains("gAAAAAB"));
        assert_eq!(second["status"], json!("completed"));
        assert_eq!(second["collaboration"]["message"], json!("추가 검토 완료"));
        assert_eq!(started["eventKey"], first["eventKey"]);
        assert_eq!(started["eventKey"], follow_up["eventKey"]);
        assert_eq!(started["eventKey"], second["eventKey"]);
    }

    #[test]
    fn 시작_이벤트_없이_실패한_spawn_agent도_안전한_실패_카드로_남긴다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        tracker.consume(&call("spawn_agent", "call-spawn-failed", json!({ "task_name": "duplicate_review", "message": "gAAAAABprivate-prompt" })));
        let activities = tracker.consume(&output("call-spawn-failed", "agent path `/root/duplicate_review` already exists"));
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0]["status"], json!("failed"));
        assert_eq!(activities[0]["collaboration"]["agentLabel"], json!("duplicate review"));
        assert_eq!(activities[0]["collaboration"]["agentStatus"], json!("errored"));
        assert_eq!(activities[0]["collaboration"]["message"], json!("협업 검토를 시작하지 못했습니다."));
        let encoded = encoded(&activities);
        assert!(!encoded.contains("gAAAAAB") && !encoded.contains("already exists"));
    }

    #[test]
    fn list_agents_완료_결과는_최종_회신_누락을_복구한다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        tracker.consume(&call("spawn_agent", "call-spawn-2", json!({ "task_name": "fallback_review" })));
        tracker.consume(&activity("call-spawn-2", "thread-review-2", "/root/fallback_review", "started"));
        tracker.consume(&call("list_agents", "call-list-1", json!("{}")));
        let listed = json!({ "agents": [
            { "agent_name": "/root", "agent_status": "running" },
            { "agent_name": "/root/fallback_review", "agent_status": { "completed": "누락된 결과를 복구했습니다." } },
        ] });
        let activities = tracker.consume(&output("call-list-1", &encoded(&listed)));
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0]["status"], json!("completed"));
        assert_eq!(activities[0]["eventKey"], json!("collaboration:rollout:thread-review-2"));
        assert_eq!(activities[0]["collaboration"]["message"], json!("누락된 결과를 복구했습니다."));
    }

    #[test]
    fn wait_agent_결과는_협업_카드에_관리용_기록을_만들지_않는다() {
        let mut tracker = CodexRolloutCollaborationTracker::new();
        tracker.consume(&call("wait_agent", "call-wait-1", json!({ "timeout_ms": 10_000 })));
        let result = encoded(&json!({ "message": "Wait completed.", "timed_out": false }));
        assert_eq!(tracker.consume(&output("call-wait-1", &result)), Vec::<Value>::new());
    }
}
