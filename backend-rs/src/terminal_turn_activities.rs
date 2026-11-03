// 터미널 원본에서 기존 GUI가 표시할 수 있는 공개 활동만 추출한다. Node terminal-turn-activities.mjs를 옮겼다.
// 원시 추론/암호화 내용, 도구 응답 전문, 시스템 지침은 저장하지 않는다.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::agent_event_parser::{
    claude_message_model, claude_message_usage, claude_session_usage_key, decode_agent_response, parse_agent_event,
    template_text,
};
use crate::js::{
    date_parse, is_js_integer, js_join, js_keys, js_length, js_property, js_string, js_string_or_empty, js_trim,
    js_truthy, parse_js_json, to_js_json, utf16_prefix,
};
use crate::terminal_usage::{Lines, add_usage};

const MAX_ACTIVITIES: usize = 500;
const MAX_TEXT: usize = 6_000;
const KINDS: [&str; 5] = ["message", "thinking", "command", "tool", "suggestion"];
/// 시작 위치를 잃었을 때 끝에서부터 다시 읽는 최대 범위.
const RECOVERY_BYTES: u64 = 4 * 1024 * 1024;
pub const WAIT_TIMEOUT_MS: u64 = 1_200;
pub const WAIT_POLL_MS: u64 = 100;

/// JS `left ?? right`. 없는 값과 null만 오른쪽으로 넘어간다.
fn coalesce<'a>(left: Option<&'a Value>, right: Option<&'a Value>) -> Option<&'a Value> {
    left.filter(|value| !value.is_null()).or(right)
}

/// `String(value ?? "").trim()`을 6,000자(UTF-16)로 자른다.
fn clipped(value: Option<&Value>) -> String {
    clipped_text(&js_string_or_empty(value))
}

fn clipped_text(text: &str) -> String {
    let text = js_trim(text);
    if js_length(text) > MAX_TEXT { format!("{}…", utf16_prefix(text, MAX_TEXT - 1)) } else { text.to_string() }
}

/// 배열 안 text 계열 조각의 글자를 줄바꿈으로 잇는다.
fn content_text(content: Option<&Value>) -> String {
    let Some(Value::Array(parts)) = content else {
        return String::new();
    };
    let texts: Vec<Value> = parts
        .iter()
        .filter(|part| {
            matches!(part.get("type").and_then(Value::as_str), Some("text" | "Text" | "output_text" | "summary_text"))
        })
        .map(|part| part.get("text").cloned().unwrap_or(Value::Null))
        .collect();
    js_trim(&js_join(&texts, "\n")).to_string()
}

fn decoded_text(value: &str) -> String {
    decode_agent_response(value).get("text").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn json_line(value: &Value) -> Result<String, String> {
    let bytes = to_js_json(value).map_err(|error| error.to_string())?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

/// isTerminalFinalMessage. 최종 답변과 같은 메시지는 작업 내역에서 뺀다.
pub fn is_terminal_final_message(activity: &Value, final_response: &str) -> bool {
    if activity.get("kind").and_then(Value::as_str) != Some("message") || final_response.is_empty() {
        return false;
    }
    let final_text = decoded_text(final_response);
    let text = decoded_text(&js_string_or_empty(activity.get("text")));
    !final_text.is_empty() && (text == final_text || text == clipped_text(&final_text))
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    kind: &'static str,
    text: String,
    event_key: String,
    status: &'static str,
}

/// 말풍선에 띄울 가장 최근 활동.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LatestActivity {
    pub kind: &'static str,
    pub text: String,
    pub status: &'static str,
}

/// TerminalActivityCollector. 같은 eventKey는 처음 나온 자리를 지키며 갱신하고 500개를 넘으면 개수만 센다.
#[derive(Clone, Debug, Default)]
pub struct TerminalActivityCollector {
    pub workdir: Option<String>,
    entries: Vec<Entry>,
    positions: HashMap<String, usize>,
    omitted: usize,
    revision: u64,
    latest: Option<LatestActivity>,
}

impl TerminalActivityCollector {
    pub fn new(workdir: Option<String>) -> Self {
        Self { workdir, ..Self::default() }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn latest(&self) -> Option<&LatestActivity> {
        self.latest.as_ref()
    }

    fn entry(&self, key: &str) -> Option<&Entry> {
        self.positions.get(key).map(|index| &self.entries[*index])
    }

    pub fn add(&mut self, activity: &Value) {
        let Some(kind) = activity.get("kind").and_then(Value::as_str).and_then(|kind| KINDS.into_iter().find(|known| *known == kind))
        else {
            return;
        };
        let key = match activity.get("eventKey") {
            Some(value) if !value.is_null() => js_string(value),
            _ => format!("{kind}:{}", template_text(activity.get("text"))),
        };
        let old = self.entry(&key).cloned();
        let text = if js_truthy(activity.get("preserveText"))
            && let Some(old) = &old
        {
            old.text.clone()
        } else {
            clipped(activity.get("text"))
        };
        if text.is_empty() {
            return;
        }
        let status = match activity.get("status").and_then(Value::as_str) {
            Some("running") => "running",
            Some("failed") => "failed",
            _ => "completed",
        };
        if old.as_ref().is_some_and(|old| old.kind == kind && old.text == text && old.status == status) {
            return;
        }
        self.latest = Some(LatestActivity { kind, text: text.clone(), status });
        if old.is_none() && self.entries.len() >= MAX_ACTIVITIES {
            self.omitted += 1;
            self.revision += 1;
            return;
        }
        self.revision += 1;
        let entry = Entry { kind, text, event_key: key.clone(), status };
        match self.positions.get(&key) {
            Some(index) => self.entries[*index] = entry,
            None => {
                self.positions.insert(key, self.entries.len());
                self.entries.push(entry);
            }
        }
    }

    /// parseAgentEvent 결과를 받는다. 파서가 내는 activities는 늘 배열이다.
    pub fn parsed(&mut self, event: Option<&Value>) {
        let Some(event) = event else {
            return;
        };
        let mut count = 0;
        if let Some(Value::Array(items)) = event.get("activities") {
            for item in items {
                self.add(item);
            }
            count = items.len();
        }
        let activity = event.get("activity");
        if js_truthy(activity) {
            self.add(activity.unwrap_or(&Value::Null));
        }
        let operation = if js_truthy(activity) || count > 0 { self.latest.clone() } else { None };
        if js_truthy(event.get("agentMessage")) {
            let message = js_string(event.get("agentMessage").unwrap_or(&Value::Null));
            let key = event.get("agentMessageKey").filter(|key| js_truthy(Some(key)));
            self.add(&json!({
                "kind": "message",
                "text": decoded_text(&message),
                "eventKey": key.map(|key| format!("message:{}", js_string(key))),
            }));
        }
        // 같은 응답 안에서 시작한 작업을 앞선 글보다 말풍선에 먼저 보이되 기록 순서는 지킨다.
        if let Some(operation) = operation
            && operation.status == "running"
        {
            self.latest = Some(operation);
        }
    }

    /// Codex rollout 한 줄. record가 null이면 Node처럼 TypeError 문구를 낸다.
    pub fn codex(&mut self, record: &Value) -> Result<(), String> {
        let empty = Value::Object(Map::new());
        let item = coalesce(record.get("payload"), None).unwrap_or(&empty);
        let record_type = js_property(record, "type")?.and_then(Value::as_str);
        let item_type = item.get("type").and_then(Value::as_str);
        if record_type == Some("response_item") {
            match item_type {
                Some("message")
                    if item.get("role").and_then(Value::as_str) == Some("assistant")
                        && item.get("phase").and_then(Value::as_str) == Some("commentary") =>
                {
                    self.add(&json!({ "kind": "message", "text": content_text(item.get("content")) }));
                }
                // summary는 CLI 공개 요약이다. encrypted_content/text는 읽지 않는다.
                Some("reasoning") => self.add(&json!({ "kind": "thinking", "text": content_text(item.get("summary")) })),
                Some("function_call" | "custom_tool_call") => self.tool_call(item)?,
                Some("function_call_output" | "custom_tool_call_output") => self.tool_output(item),
                _ => {}
            }
            return Ok(());
        }
        if record_type != Some("event_msg") {
            return Ok(());
        }
        match item_type {
            Some("item_completed") => self.completed_item(coalesce(item.get("item"), None).unwrap_or(&empty)),
            Some("patch_apply_end") => self.patch_applied(item),
            _ => Ok(()),
        }
    }

    fn tool_call(&mut self, item: &Value) -> Result<(), String> {
        let arguments = coalesce(item.get("arguments"), None).map_or_else(|| "{}".to_string(), js_string);
        let arguments = parse_js_json(&arguments).ok();
        let names: Vec<Value> =
            [item.get("namespace"), item.get("name")].into_iter().flatten().filter(|value| js_truthy(Some(value))).cloned().collect();
        let name = js_join(&names, ".");
        let command = if ["exec_command", "shell_command", "shell"].iter().any(|suffix| name.ends_with(suffix)) {
            arguments.as_ref().and_then(|arguments| coalesce(arguments.get("cmd"), arguments.get("command")))
        } else {
            None
        };
        let command = command.filter(|command| js_truthy(Some(command)));
        let text = match command {
            Some(command) => {
                let mut started = Map::new();
                started.insert("type".into(), "command_execution".into());
                if let Some(id) = item.get("call_id") {
                    started.insert("id".into(), id.clone());
                }
                started.insert("command".into(), js_string(command).into());
                let line = json_line(&json!({ "type": "item.started", "item": started }))?;
                let parsed = parse_agent_event(&line, "codex", self.workdir.as_deref())?;
                let parsed = parsed.as_ref().and_then(|event| event.get("activity"));
                coalesce(parsed.and_then(|activity| activity.get("text")), None)
                    .cloned()
                    .unwrap_or_else(|| "명령 실행".into())
            }
            None => format!("도구 · {}", if name.is_empty() { "실행" } else { &name }).into(),
        };
        self.add(&json!({
            "kind": if command.is_some() { "command" } else { "tool" },
            "text": text,
            "eventKey": format!("call:{}", template_text(coalesce(item.get("call_id"), item.get("id")))),
            "status": "running",
        }));
        Ok(())
    }

    fn tool_output(&mut self, item: &Value) {
        let output = match item.get("output") {
            Some(Value::String(text)) => parse_js_json(text).ok(),
            other => other.cloned(),
        };
        let key = format!("call:{}", template_text(item.get("call_id")));
        let kind = self.entry(&key).map_or("tool", |entry| entry.kind);
        let output = output.as_ref();
        let failed = output.and_then(|output| output.get("isError")) == Some(&Value::Bool(true))
            || output
                .and_then(|output| output.get("exit_code"))
                .and_then(Value::as_f64)
                .is_some_and(|code| is_js_integer(code) && code != 0.0);
        self.add(&json!({
            "kind": kind,
            "text": "도구 완료",
            "eventKey": key,
            "preserveText": true,
            "status": if failed { "failed" } else { "completed" },
        }));
    }

    fn completed_item(&mut self, value: &Value) -> Result<(), String> {
        let item_type = js_string_or_empty(value.get("type")).replace(['_', '-'], "").to_lowercase();
        let public_text = |content: &str| {
            let text = content_text(value.get(content));
            if text.is_empty() { value.get("text").cloned() } else { Some(text.into()) }
        };
        let activity = |kind: &str, text: Option<Value>| {
            let mut activity = Map::new();
            activity.insert("kind".into(), kind.into());
            if let Some(text) = text {
                activity.insert("text".into(), text);
            }
            Value::Object(activity)
        };
        if item_type == "agentmessage" && value.get("phase").and_then(Value::as_str) == Some("commentary") {
            self.add(&activity("message", public_text("content")));
            return Ok(());
        }
        if item_type == "reasoning" {
            self.add(&activity("thinking", public_text("summary")));
            return Ok(());
        }
        // `types[type]`는 객체 리터럴이라 "constructor"가 Object 함수로 참이 되고 JSON.stringify가 그 type 칸을 지운다.
        let renamed = match item_type.as_str() {
            "commandexecution" => Some("command_execution"),
            "filechange" => Some("file_change"),
            "mcptoolcall" => Some("mcp_tool_call"),
            "websearch" => Some("web_search"),
            "constructor" => None,
            _ => return Ok(()),
        };
        let mut copy = value.as_object().cloned().unwrap_or_default();
        match renamed {
            Some(name) => {
                copy.insert("type".into(), name.into());
            }
            None => {
                copy.shift_remove("type");
            }
        }
        let line = json_line(&json!({ "type": "item.completed", "item": copy }))?;
        let parsed = parse_agent_event(&line, "codex", self.workdir.as_deref())?;
        self.parsed(parsed.as_ref());
        Ok(())
    }

    fn patch_applied(&mut self, item: &Value) -> Result<(), String> {
        // Object.entries(item.changes ?? {}) — 글자는 UTF-16 색인, 배열은 색인, 객체는 Object.keys 순서다.
        let entries: Vec<(String, Option<&Value>)> = match coalesce(item.get("changes"), None) {
            Some(Value::Object(fields)) => js_keys(fields).into_iter().map(|key| (key.to_string(), fields.get(key))).collect(),
            Some(Value::Array(values)) => values.iter().enumerate().map(|(index, value)| (index.to_string(), Some(value))).collect(),
            Some(Value::String(text)) => (0..js_length(text)).map(|index| (index.to_string(), None)).collect(),
            _ => Vec::new(),
        };
        let mut changes = Vec::with_capacity(entries.len());
        for (path, change) in entries {
            let kind = match change {
                Some(change) => coalesce(js_property(change, "type")?, None).cloned(),
                None => None,
            };
            changes.push(json!({ "path": path, "kind": kind.unwrap_or_else(|| "update".into()) }));
        }
        let mut changed = Map::new();
        changed.insert("type".into(), "file_change".into());
        if let Some(id) = item.get("call_id") {
            changed.insert("id".into(), id.clone());
        }
        changed.insert("changes".into(), changes.into());
        let failed = item.get("success") == Some(&Value::Bool(false));
        changed.insert("status".into(), if failed { "failed" } else { "completed" }.into());
        let line = json_line(&json!({ "type": "item.completed", "item": changed }))?;
        let parsed = parse_agent_event(&line, "codex", self.workdir.as_deref())?;
        self.parsed(parsed.as_ref());
        Ok(())
    }

    pub fn snapshot(&self, final_response: &str, final_: bool) -> Vec<Value> {
        let mut signatures = HashSet::new();
        let mut result = Vec::new();
        for entry in &self.entries {
            let text = if entry.kind == "message" { decoded_text(&entry.text) } else { entry.text.clone() };
            let raw = json!({ "kind": entry.kind, "text": entry.text });
            if text.is_empty() || is_terminal_final_message(&raw, final_response) {
                continue;
            }
            // 같은 공개 메시지가 여러 CLI 기록 형식에 나타나도 한 번만 표시한다.
            let signature = format!("{}\n{}\n{}", entry.kind, text, entry.status);
            if matches!(entry.kind, "message" | "thinking") && signatures.contains(&signature) {
                continue;
            }
            signatures.insert(signature);
            result.push(json!({
                "kind": entry.kind,
                "text": text,
                "eventKey": format!("terminal:{}", entry.event_key),
                "status": if final_ && entry.status == "running" { "completed" } else { entry.status },
            }));
        }
        if self.omitted > 0 {
            result.push(json!({
                "kind": "tool",
                "text": format!("추가 활동 {}개 표시 생략", self.omitted),
                "status": "completed",
                "eventKey": "terminal:omitted",
            }));
        }
        result
    }

    pub fn finish(&self, final_response: &str) -> Vec<Value> {
        self.snapshot(final_response, true)
    }
}

/// readClaudeTerminalTurn의 선택 인자.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeTurnRead {
    /// None은 Node의 null(시작 위치를 모름)이다. 기본값은 0이다.
    pub offset: Option<u64>,
    pub session_id: Option<String>,
    pub workdir: Option<String>,
    pub final_response: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub require_timestamps: bool,
    /// None은 Node의 undefined(비교하지 않음)다.
    pub inode: Option<u64>,
}

impl Default for ClaudeTurnRead {
    fn default() -> Self {
        Self {
            offset: Some(0),
            session_id: None,
            workdir: None,
            final_response: None,
            started_at: None,
            ended_at: None,
            require_timestamps: false,
            inode: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTerminalTurn {
    pub activities: Vec<Value>,
    pub usage: Option<Value>,
    pub model: Option<String>,
    pub final_found: bool,
}

/// readClaudeTerminalActivities. UserPromptSubmit 시점의 바이트 위치부터 읽어 같은 세션의 과거 턴을 섞지 않는다.
pub fn read_claude_terminal_activities(path: Option<&str>, options: &ClaudeTurnRead) -> Vec<Value> {
    let options = ClaudeTurnRead {
        offset: options.offset,
        session_id: options.session_id.clone(),
        workdir: options.workdir.clone(),
        final_response: options.final_response.clone(),
        ..ClaudeTurnRead::default()
    };
    read_claude_terminal_turn(path, &options).activities
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// `record.sessionId === sessionID`. 둘 다 없으면 같다.
fn same_session(session: Option<&Value>, session_id: Option<&str>) -> bool {
    match (session, session_id) {
        (None, None) => true,
        (Some(Value::String(session)), Some(session_id)) => session == session_id,
        _ => false,
    }
}

/// readClaudeTerminalTurn. 부가 기록을 읽지 못해도 원래 최종 답변 저장은 막지 않으므로 읽기 오류는 그때까지 모은 값으로 끝낸다.
pub fn read_claude_terminal_turn(path: Option<&str>, options: &ClaudeTurnRead) -> ClaudeTerminalTurn {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return ClaudeTerminalTurn::default();
    };
    let mut collector = TerminalActivityCollector::new(options.workdir.clone());
    let mut requests = Map::new();
    let (mut final_found, mut needs_earlier_boundary, mut earlier_boundary, mut model) = (false, false, false, None);
    let started = non_empty(&options.started_at).map(|text| date_parse(text).unwrap_or(f64::NAN));
    let ended = non_empty(&options.ended_at).map(|text| date_parse(text).unwrap_or(f64::NAN));
    let final_text = js_trim(options.final_response.as_deref().unwrap_or_default()).to_string();
    'read: {
        let Ok(metadata) = std::fs::metadata(path) else {
            break 'read;
        };
        let mut offset = options.offset;
        let mut require_timestamps = options.require_timestamps;
        if offset.is_none_or(|offset| offset > metadata.len()) || options.inode.is_some_and(|inode| inode != metadata.ino()) {
            // 시작 위치가 없거나 압축으로 파일이 바뀌었다. 끝 쪽만 읽고 같은 세션·시각을 요구해 과거 기록을 들이지 않는다.
            if started.is_none() || ended.is_none() || non_empty(&options.session_id).is_none() {
                return ClaudeTerminalTurn::default();
            }
            let start = metadata.len().saturating_sub(RECOVERY_BYTES);
            offset = Some(start);
            require_timestamps = true;
            needs_earlier_boundary = start > 0;
        }
        let Ok(mut lines) = Lines::open(path, offset.unwrap_or(0)) else {
            break 'read;
        };
        while let Ok(Some(line)) = lines.next_line() {
            let Ok(record) = parse_js_json(&line) else {
                continue;
            };
            let Ok(sidechain) = js_property(&record, "isSidechain") else {
                break;
            };
            let session = record.get("sessionId");
            let same = same_session(session, options.session_id.as_deref());
            if js_truthy(sidechain) || (js_truthy(session) && !same) {
                continue;
            }
            let at = record.get("timestamp").map_or(f64::NAN, |value| date_parse(&js_string(value)).unwrap_or(f64::NAN));
            if same && at.is_finite() && started.is_some_and(|start| at < start) {
                earlier_boundary = true;
            }
            if require_timestamps && (!at.is_finite() || !same) {
                continue;
            }
            if at.is_finite() && (started.is_some_and(|start| at < start) || ended.is_some_and(|end| at > end)) {
                continue;
            }
            let record_type = record.get("type").and_then(Value::as_str);
            if !matches!(record_type, Some("assistant" | "user")) {
                continue;
            }
            let Ok(event) = parse_agent_event(&line, "claude", options.workdir.as_deref()) else {
                break;
            };
            collector.parsed(event.as_ref());
            if record_type != Some("assistant") {
                continue;
            }
            model = claude_message_model(&record).or(model);
            let content = record.get("message").and_then(|message| message.get("content"));
            if !final_text.is_empty() && content_text(content) == final_text {
                final_found = true;
            }
            // 한 응답의 스트리밍 조각은 message.id를 같이 쓴다. 처음 부분값이 아니라 최신 값을 지키고 사고·본문 중복은 더하지 않는다.
            if let (Some(Value::Object(usage)), Some(key)) = (claude_message_usage(&record), claude_session_usage_key(&record))
                && !key.is_empty()
                && let Value::Object(request) = requests.entry(key).or_insert_with(|| Value::Object(Map::new()))
            {
                for (field, value) in usage {
                    if !value.is_null() {
                        request.insert(field, value);
                    }
                }
            }
        }
    }
    let usage = requests.values().fold(None, |total, value| add_usage(total, Some(value)));
    if needs_earlier_boundary && !earlier_boundary {
        return ClaudeTerminalTurn::default();
    }
    ClaudeTerminalTurn {
        activities: collector.finish(options.final_response.as_deref().unwrap_or_default()),
        usage,
        model,
        final_found,
    }
}

/// waitForClaudeTerminalTurn. 최종 답변과 사용량이 원본에 쓰일 때까지 짧게 다시 읽는다.
pub async fn wait_for_claude_terminal_turn(
    path: Option<String>,
    options: ClaudeTurnRead,
    timeout_ms: u64,
    poll_ms: u64,
) -> ClaudeTerminalTurn {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let (reading, settings) = (path.clone(), options.clone());
        let result = match tokio::task::spawn_blocking(move || read_claude_terminal_turn(reading.as_deref(), &settings)).await {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => return ClaudeTerminalTurn::default(),
        };
        if result.final_found && result.usage.is_some() {
            return result;
        }
        let now = Instant::now();
        if now >= deadline || path.as_deref().is_none_or(str::is_empty) {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(poll_ms).min(deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temporary_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("officestra-{name}-{}-{}", std::process::id(), crate::js::now_milliseconds()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn line(value: Value) -> String {
        format!("{}\n", String::from_utf8(to_js_json(&value).unwrap()).unwrap())
    }

    fn kinds(activities: &[Value]) -> Vec<&str> {
        activities.iter().map(|activity| activity["kind"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn 늦게_쓰인_사고와_본문을_다시_읽고_응답마다_최신_사용량만_센다() {
        let root = temporary_root("claude-flush");
        let path = root.join("session.jsonl");
        std::fs::write(&path, "").unwrap();
        let record = |content: Value, output: u64| {
            line(json!({
                "type": "assistant", "sessionId": "session", "timestamp": "2026-09-14T00:00:01Z",
                "message": { "id": "same", "content": content, "usage": { "input_tokens": 15731, "output_tokens": output, "cache_read_input_tokens": 0 } },
            }))
        };
        let delayed = record(json!([{ "type": "thinking", "thinking": "공개 사고" }]), 20) + &record(json!([{ "type": "text", "text": "최종" }]), 175);
        let writing_path = path.clone();
        let writing = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            std::fs::OpenOptions::new().append(true).open(writing_path).unwrap().write_all(delayed.as_bytes()).unwrap();
        });
        let options = ClaudeTurnRead { session_id: Some("session".into()), final_response: Some("최종".into()), ..Default::default() };
        let result = wait_for_claude_terminal_turn(Some(path.to_str().unwrap().into()), options, 500, 20).await;
        writing.await.unwrap();
        assert!(result.final_found);
        assert_eq!(result.usage.as_ref().unwrap()["inputTokens"].as_f64(), Some(15731.0));
        assert_eq!(result.usage.as_ref().unwrap()["outputTokens"].as_f64(), Some(175.0));
        assert_eq!(kinds(&result.activities), ["thinking"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn 시작_위치를_잃으면_시각_세션_하위_작업을_걸러_다시_읽는다() {
        let root = temporary_root("claude-scope");
        let path = root.join("session.jsonl");
        let row = |text: &str, changes: Value| {
            let mut value = json!({
                "type": "assistant", "sessionId": "session", "timestamp": "2026-09-14T00:00:01Z",
                "message": { "id": text, "model": "claude-opus-5-5", "content": [{ "type": "text", "text": text }], "usage": { "input_tokens": 10, "output_tokens": 2 } },
            });
            for (key, change) in changes.as_object().unwrap() {
                value[key] = change.clone();
            }
            line(value)
        };
        let side = json!({ "isSidechain": true, "message": { "id": "side", "model": "claude-haiku-4-5", "content": [{ "type": "text", "text": "side" }] } });
        let rows = [
            row("old", json!({ "timestamp": "2026-09-13T00:00:00Z" })),
            row("other", json!({ "sessionId": "other" })),
            row("side", side),
            row("no-time", json!({ "timestamp": null })),
            row("final", json!({})),
        ];
        std::fs::write(&path, rows.concat()).unwrap();
        let path = path.to_str().unwrap();
        for offset in [None, Some(999_999)] {
            let options = ClaudeTurnRead {
                offset,
                session_id: Some("session".into()),
                started_at: Some("2026-09-14T00:00:00Z".into()),
                ended_at: Some("2026-09-14T00:00:02Z".into()),
                final_response: Some("final".into()),
                ..Default::default()
            };
            let result = read_claude_terminal_turn(Some(path), &options);
            assert!(result.final_found);
            assert_eq!(result.usage.as_ref().unwrap()["inputTokens"].as_f64(), Some(10.0));
            assert_eq!(result.model.as_deref(), Some("claude-opus-5-5"));
            assert_eq!(result.activities, Vec::<Value>::new());
        }
        let unbounded = ClaudeTurnRead { offset: None, session_id: Some("session".into()), ..Default::default() };
        assert_eq!(read_claude_terminal_turn(Some(path), &unbounded).usage, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn 끝_범위만_읽은_큰_턴은_완료로_보고하지_않는다() {
        let root = temporary_root("claude-large");
        let path = root.join("session.jsonl");
        let row = |text: &str| {
            line(json!({
                "type": "assistant", "sessionId": "session", "timestamp": "2026-09-14T00:00:01Z",
                "message": { "id": text.chars().take(8).collect::<String>(), "content": [{ "type": "text", "text": text }], "usage": { "input_tokens": 10, "output_tokens": 2 } },
            }))
        };
        std::fs::write(&path, row(&"x".repeat(4 * 1024 * 1024)) + &row("final")).unwrap();
        let options = ClaudeTurnRead {
            offset: None,
            session_id: Some("session".into()),
            started_at: Some("2026-09-14T00:00:00Z".into()),
            ended_at: Some("2026-09-14T00:00:02Z".into()),
            final_response: Some("final".into()),
            ..Default::default()
        };
        let result = read_claude_terminal_turn(path.to_str(), &options);
        assert_eq!(result.usage, None);
        assert!(!result.final_found);
        assert_eq!(result.activities, Vec::<Value>::new());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_공개_요약_진행_도구_결과만_기존_활동_형식으로_바꾼다() {
        let mut collector = TerminalActivityCollector::new(Some("/repo".into()));
        let mut add = |payload: Value| collector.codex(&json!({ "type": "response_item", "payload": payload })).unwrap();
        add(json!({ "type": "message", "role": "system", "content": [{ "type": "text", "text": "시스템 지침" }] }));
        add(json!({ "type": "reasoning", "text": "원시 추론", "encrypted_content": "암호화 내용", "summary": [{ "type": "summary_text", "text": "공개 요약" }] }));
        add(json!({ "type": "message", "role": "assistant", "phase": "commentary", "content": [{ "type": "output_text", "text": "확인 중입니다" }] }));
        add(json!({ "type": "message", "role": "assistant", "phase": "analysis", "content": [{ "type": "output_text", "text": "비공개" }] }));
        add(json!({ "type": "message", "role": "assistant", "phase": "final_answer", "content": [{ "type": "output_text", "text": "최종 답변" }] }));
        add(json!({ "type": "function_call", "name": "exec_command", "call_id": "call1", "arguments": r#"{"cmd":"git diff --check"}"# }));
        add(json!({ "type": "function_call_output", "call_id": "call1", "output": r#"{"exit_code":1,"output":"응답 전문은 제외"}"# }));
        let activities = collector.finish("최종 답변");
        let summary: Vec<Value> = activities
            .iter()
            .map(|activity| json!({ "kind": activity["kind"], "text": activity["text"], "status": activity["status"] }))
            .collect();
        assert_eq!(
            summary,
            [
                json!({ "kind": "thinking", "text": "공개 요약", "status": "completed" }),
                json!({ "kind": "message", "text": "확인 중입니다", "status": "completed" }),
                json!({ "kind": "command", "text": "git diff --check", "status": "failed" }),
            ]
        );
        assert!(!String::from_utf8(to_js_json(&activities).unwrap()).unwrap().contains("암호화"));
    }

    #[test]
    fn 중복_메시지_기계_블록_최종_답변은_작업_내역에서_반복하지_않는다() {
        let mut collector = TerminalActivityCollector::default();
        collector.add(&json!({ "kind": "message", "text": "진행 중", "eventKey": "one" }));
        collector.add(&json!({ "kind": "message", "text": "진행 중", "eventKey": "two" }));
        collector.add(&json!({ "kind": "message", "text": "완료\n[NEED_INPUT]", "eventKey": "final" }));
        collector.add(&json!({ "kind": "message", "text": "확인\n[OFFICE_SOURCES]\n[{\"kind\":\"file\",\"title\":\"파일\",\"locator\":\"a.mjs\"}]" }));
        let texts: Vec<Value> = collector.finish("완료").into_iter().map(|activity| activity["text"].clone()).collect();
        assert_eq!(texts, [json!("진행 중"), json!("확인")]);
    }

    #[test]
    fn 긴_터미널_기록은_표시량을_제한하고_생략_사실을_남긴다() {
        let mut collector = TerminalActivityCollector::default();
        for index in 0..505 {
            collector.add(&json!({ "kind": "tool", "text": "a".repeat(7000), "eventKey": index.to_string() }));
        }
        let activities = collector.finish("");
        assert_eq!(activities.len(), 501);
        assert_eq!(js_length(activities[0]["text"].as_str().unwrap()), 6000);
        assert_eq!(activities[500]["text"], json!("추가 활동 5개 표시 생략"));
    }

    #[test]
    fn claude는_시작_위치_이후_같은_세션의_공개_활동만_읽는다() {
        let root = temporary_root("terminal-activities");
        let path = root.join("transcript.jsonl");
        let assistant = |id: &str, content: Value, extra: Value| {
            let mut value = json!({ "type": "assistant", "sessionId": "session", "message": { "id": id, "content": content } });
            for (key, change) in extra.as_object().unwrap() {
                value[key] = change.clone();
            }
            value
        };
        let previous = line(assistant("old", json!([{ "type": "text", "text": "이전 턴" }]), json!({})));
        let records = [
            assistant(
                "a",
                json!([
                    { "type": "thinking", "thinking": "공개된 추론 요약" },
                    { "type": "text", "text": "파일 확인 중" },
                    { "type": "tool_use", "id": "tool1", "name": "Read", "input": { "file_path": "/repo/a.mjs" } },
                ]),
                json!({}),
            ),
            json!({ "type": "user", "sessionId": "session", "message": { "content": [{ "type": "tool_result", "tool_use_id": "tool1", "is_error": true, "content": "도구 전문" }] } }),
            assistant("side", json!([{ "type": "text", "text": "별도 작업" }]), json!({ "isSidechain": true })),
            assistant("other", json!([{ "type": "text", "text": "다른 세션" }]), json!({ "sessionId": "other" })),
            assistant("last", json!([{ "type": "text", "text": "최종 답변" }]), json!({})),
        ];
        let appended: Vec<String> = records.iter().map(|record| String::from_utf8(to_js_json(record).unwrap()).unwrap()).collect();
        std::fs::write(&path, format!("{previous}{}\n{{partial", appended.join("\n"))).unwrap();
        let options = ClaudeTurnRead {
            offset: Some(previous.len() as u64),
            session_id: Some("session".into()),
            workdir: Some("/repo".into()),
            final_response: Some("최종 답변".into()),
            ..Default::default()
        };
        let activities = read_claude_terminal_activities(path.to_str(), &options);
        assert_eq!(kinds(&activities), ["thinking", "tool", "message"]);
        let tool = activities.iter().find(|activity| activity["kind"] == "tool").unwrap();
        assert_eq!(tool["status"], json!("failed"));
        assert!(tool["text"].as_str().unwrap().contains("a.mjs"));
        let encoded = String::from_utf8(to_js_json(&activities).unwrap()).unwrap();
        for hidden in ["이전 턴", "별도 작업", "다른 세션", "최종 답변", "도구 전문"] {
            assert!(!encoded.contains(hidden), "{hidden}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claude_원본이_없어도_최종_응답_저장을_막지_않는다() {
        let options = ClaudeTurnRead::default();
        assert_eq!(read_claude_terminal_activities(Some("/nonexistent/officestra-fixture.jsonl"), &options), Vec::<Value>::new());
    }
}
