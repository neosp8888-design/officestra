// Codex notify를 정식 rollout 턴과 대조해 터미널 대화만 받아들인다. Node codex-rollout-turns.mjs를 옮겼다.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::antigravity_local_state::is_session_id;
use crate::js::{join_path, js_string_or_empty, js_to_number, js_trim, now_milliseconds, parse_js_json, resolve_path, uv_error};
use crate::local_artifacts::home_directory;
use crate::terminal_turn_activities::TerminalActivityCollector;
use crate::terminal_usage::Lines;

pub const DEFAULT_RETRY_COUNT: u32 = 7;
pub const DEFAULT_RETRY_DELAY_MS: u64 = 500;
const DAY_MILLISECONDS: f64 = 24.0 * 60.0 * 60.0 * 1_000.0;

pub type RolloutPathCache = Mutex<HashMap<String, String>>;

static ROLLOUT_PATH_CACHE: LazyLock<RolloutPathCache> = LazyLock::new(Default::default);

/// Node 모듈 전역 rolloutPathCache. 같은 프로세스에서 찾은 rollout 경로를 다시 찾지 않는다.
pub fn rollout_path_cache() -> &'static RolloutPathCache {
    &ROLLOUT_PATH_CACHE
}

/// `join(homedir(), ".codex", "sessions")`.
pub fn default_sessions_root() -> String {
    join_path(&[&home_directory(), ".codex", "sessions"])
}

/// cleanID. 앞뒤 공백을 지운 UUID 모양(대소문자 무관)만 받는다.
fn clean_id(value: &str) -> Option<String> {
    let candidate = js_trim(value);
    is_session_id(candidate).then(|| candidate.to_string())
}

fn clean_value_id(value: Option<&Value>) -> Option<String> {
    clean_id(&js_string_or_empty(value))
}

/// JS `left ?? right`.
fn coalesce<'a>(left: Option<&'a Value>, right: Option<&'a Value>) -> Option<&'a Value> {
    left.filter(|value| !value.is_null()).or(right)
}

fn trimmed(value: Option<&Value>) -> String {
    js_trim(&js_string_or_empty(value)).to_string()
}

/// `new Date(value)`의 현지 연·월·일. Invalid Date면 셋 다 "NaN"이다.
fn date_parts(milliseconds: f64) -> [String; 3] {
    let invalid = || ["NaN".to_string(), "NaN".to_string(), "NaN".to_string()];
    if !milliseconds.is_finite() || milliseconds.trunc().abs() > 8.64e15 {
        return invalid();
    }
    let seconds = (milliseconds.trunc() / 1_000.0).floor() as libc::time_t;
    // SAFETY: localtime_r는 넘긴 두 포인터만 읽고 쓰며 tm은 0으로 채운 값에서 시작한다.
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut parts) }.is_null() {
        return invalid();
    }
    [
        (i64::from(parts.tm_year) + 1900).to_string(),
        format!("{:02}", parts.tm_mon + 1),
        format!("{:02}", parts.tm_mday),
    ]
}

/// findInDirectory. 폴더를 열지 못하면 없는 것으로 보고, 읽는 중 오류는 Node처럼 그대로 넘긴다.
fn find_in_directory(directory: &str, suffix: &str, recursive: bool) -> io::Result<Option<String>> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(None);
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = join_path(&[directory, &name]);
        let file_type = entry.file_type()?;
        if file_type.is_file() && name.ends_with(suffix) {
            return Ok(Some(path));
        }
        if recursive
            && file_type.is_dir()
            && let Some(nested) = find_in_directory(&path, suffix, true)?
        {
            return Ok(Some(nested));
        }
    }
    Ok(None)
}

/// findCodexRolloutPath. 오늘·어제 날짜 폴더를 먼저 보고 없으면 전체를 훑는다.
pub fn find_codex_rollout_path(
    thread_id: Option<&str>,
    sessions_root: &str,
    now: f64,
    cache: &RolloutPathCache,
) -> io::Result<Option<String>> {
    let Some(id) = clean_id(thread_id.unwrap_or_default()) else {
        return Ok(None);
    };
    let cache_key = format!("{}:{id}", resolve_path(sessions_root));
    if let Some(cached) = cache.lock().unwrap_or_else(PoisonError::into_inner).get(&cache_key).filter(|path| !path.is_empty())
    {
        return Ok(Some(cached.clone()));
    }
    let remember = |path: &str| {
        cache.lock().unwrap_or_else(PoisonError::into_inner).insert(cache_key.clone(), path.to_string());
    };
    let suffix = format!("-{id}.jsonl");
    for offset in [0.0, -DAY_MILLISECONDS] {
        let [year, month, day] = date_parts(now + offset);
        let directory = join_path(&[sessions_root, &year, &month, &day]);
        if let Some(candidate) = find_in_directory(&directory, &suffix, false)? {
            remember(&candidate);
            return Ok(Some(candidate));
        }
    }
    let candidate = find_in_directory(sessions_root, &suffix, true)?;
    if let Some(candidate) = candidate.as_deref().filter(|path| !path.is_empty()) {
        remember(candidate);
    }
    Ok(candidate)
}

fn text_content(content: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = content else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for item in items {
        let item_type = js_string_or_empty(item.get("type")).to_lowercase();
        if matches!(item_type.as_str(), "text" | "input_text" | "output_text") {
            let text = trimmed(item.get("text"));
            if !text.is_empty() {
                values.push(text);
            }
        } else if item_type == "local_image" {
            let path = trimmed(item.get("path"));
            if !path.is_empty() {
                values.push(format!("[첨부: {path}]"));
            }
        }
    }
    values
}

fn joined(values: &[String]) -> String {
    js_trim(&values.join("\n")).to_string()
}

fn item_type(item: &Value) -> String {
    js_string_or_empty(item.get("type")).replace(['_', '-'], "").to_lowercase()
}

fn metadata_turn_id(payload: &Value) -> Option<&Value> {
    payload.get("internal_chat_message_metadata_passthrough").and_then(|metadata| metadata.get("turn_id"))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CodexUserPrompt {
    #[serde(rename = "turnID")]
    pub turn_id: String,
    pub text: String,
}

/// codexRolloutUserPrompt. rollout 한 줄이 사용자 메시지면 턴 id와 본문을 돌려준다. 두 기록 형태를 모두 읽는다.
pub fn codex_rollout_user_prompt(record: &Value) -> Option<CodexUserPrompt> {
    let empty = Value::Object(Map::new());
    let payload = coalesce(record.get("payload"), None).unwrap_or(&empty);
    let record_type = record.get("type").and_then(Value::as_str);
    let payload_type = payload.get("type").and_then(Value::as_str);
    let (turn_id, text) = if record_type == Some("response_item") && payload_type == Some("message") {
        if js_string_or_empty(payload.get("role")).to_lowercase() != "user" {
            return None;
        }
        (trimmed(metadata_turn_id(payload)), joined(&text_content(payload.get("content"))))
    } else {
        if record_type != Some("event_msg") || payload_type != Some("item_completed") {
            return None;
        }
        let item = coalesce(payload.get("item"), None).unwrap_or(&empty);
        if item_type(item) != "usermessage" {
            return None;
        }
        (trimmed(payload.get("turn_id")), joined(&text_content(item.get("content"))))
    };
    (!turn_id.is_empty() && !text.is_empty()).then_some(CodexUserPrompt { turn_id, text })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRolloutTurn {
    pub rollout_path: String,
    pub source: Option<String>,
    #[serde(rename = "turnID")]
    pub turn_id: String,
    pub cwd: Option<String>,
    pub prompt: String,
    pub response: String,
    pub started_at: Option<f64>,
    pub completed_at: Option<f64>,
    pub activities: Vec<Value>,
    /// 관문을 통과한 턴에만 붙는다.
    #[serde(rename = "threadID", skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// `Number(value) || fallback`.
fn truthy_number(value: Option<&Value>, fallback: Option<f64>) -> Option<f64> {
    let number = value.map_or(f64::NAN, js_to_number);
    if number != 0.0 && !number.is_nan() { Some(number) } else { fallback }
}

fn stream_error(error: &io::Error, opened: Option<&str>) -> String {
    let (code, description) = uv_error(error);
    match opened {
        Some(path) => format!("{code}: {description}, open '{path}'"),
        None => format!("{code}: {description}, read"),
    }
}

/// readCodexRolloutTurn. 파일이 없으면 None, 다른 읽기 오류와 활동 해석의 TypeError는 Err다.
pub fn read_codex_rollout_turn(path: Option<&str>, turn_id: &str) -> Result<Option<CodexRolloutTurn>, String> {
    let wanted = js_trim(turn_id);
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    if wanted.is_empty() {
        return Ok(None);
    }
    let wanted_value = Value::String(wanted.to_string());
    let mut source: Option<String> = None;
    let mut cwd: Option<String> = None;
    let (mut started_at, mut completed_at) = (None, None);
    let mut task_complete: Option<Value> = None;
    let (mut event_prompts, mut event_final_answers) = (Vec::new(), Vec::new());
    let (mut response_item_prompts, mut response_item_final_answers) = (Vec::new(), Vec::new());
    let mut activities = TerminalActivityCollector::default();
    // activeTurnID는 찾는 턴과 같은지로만 쓰인다.
    let mut active = false;
    let mut lines = match Lines::open(path, 0) {
        Ok(lines) => lines,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(stream_error(&error, Some(path))),
    };
    let empty = Value::Object(Map::new());
    loop {
        let line = match lines.next_line() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(stream_error(&error, None)),
        };
        // 활동 줄에는 turn_id가 없는 형식도 있다. 명시적인 턴 경계 안에서만 읽고 다른 턴의 큰 본문은 JSON 해석도 하지 않는다.
        if line.is_empty()
            || (!active
                && source.is_some()
                && !line.contains(wanted)
                && !line.contains("\"turn_context\"")
                && !line.contains("\"task_started\""))
        {
            continue;
        }
        let Ok(record) = parse_js_json(&line) else {
            continue;
        };
        let payload = coalesce(record.get("payload"), None).unwrap_or(&empty);
        let record_type = record.get("type").and_then(Value::as_str);
        let payload_type = payload.get("type").and_then(Value::as_str);
        if record_type == Some("session_meta") && source.is_none() {
            source = Some(js_string_or_empty(payload.get("source")));
            continue;
        }
        if record_type == Some("turn_context") {
            active = payload.get("turn_id") == Some(&wanted_value);
            if active {
                cwd = Some(trimmed(payload.get("cwd"))).filter(|cwd| !cwd.is_empty());
                activities.workdir = cwd.clone();
            }
            continue;
        }
        if record_type == Some("event_msg") && payload_type == Some("task_started") {
            active = payload.get("turn_id") == Some(&wanted_value);
        }
        let explicit = coalesce(payload.get("turn_id"), metadata_turn_id(payload));
        let current = match explicit.filter(|value| !value.is_null()) {
            Some(value) => value == &wanted_value,
            None => active,
        };
        if !current {
            continue;
        }
        activities.codex(&record)?;
        if record_type == Some("response_item") && payload_type == Some("message") {
            if trimmed(metadata_turn_id(payload)) != wanted {
                continue;
            }
            let role = js_string_or_empty(payload.get("role")).to_lowercase();
            if role == "user" {
                response_item_prompts.extend(text_content(payload.get("content")));
            } else if role == "assistant" && js_string_or_empty(payload.get("phase")).to_lowercase() == "final_answer" {
                response_item_final_answers.extend(text_content(payload.get("content")));
            }
            continue;
        }
        if record_type != Some("event_msg") || payload.get("turn_id") != Some(&wanted_value) {
            continue;
        }
        match payload_type {
            Some("task_started") => started_at = truthy_number(payload.get("started_at"), started_at),
            Some("task_complete") => {
                task_complete = Some(payload.clone());
                started_at = truthy_number(payload.get("started_at"), started_at);
                completed_at = truthy_number(payload.get("completed_at"), completed_at);
                active = false;
            }
            Some("item_completed") => {
                let item = coalesce(payload.get("item"), None).unwrap_or(&empty);
                let kind = item_type(item);
                if kind == "usermessage" {
                    event_prompts.extend(text_content(item.get("content")));
                } else if kind == "agentmessage" && js_string_or_empty(item.get("phase")).to_lowercase() == "final_answer" {
                    event_final_answers.extend(text_content(item.get("content")));
                }
            }
            _ => {}
        }
    }
    let Some(task_complete) = task_complete else {
        return Ok(None);
    };
    let prompts = if response_item_prompts.is_empty() { event_prompts } else { response_item_prompts };
    let final_answers = if response_item_final_answers.is_empty() { event_final_answers } else { response_item_final_answers };
    let last_message = trimmed(task_complete.get("last_agent_message"));
    let response = if last_message.is_empty() { joined(&final_answers) } else { last_message };
    Ok(Some(CodexRolloutTurn {
        rollout_path: path.to_string(),
        source,
        turn_id: wanted.to_string(),
        cwd,
        prompt: joined(&prompts),
        activities: activities.finish(&response),
        response,
        started_at,
        completed_at,
        thread_id: None,
    }))
}

/// canonicalPath. 실제 경로를 못 구하면 절대 경로로 비교한다.
async fn canonical_path(value: Option<&str>) -> String {
    let path = resolve_path(value.unwrap_or_default());
    match tokio::fs::canonicalize(&path).await {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => path,
    }
}

/// notifyPrompt. resume 세션의 notify는 과거 입력까지 실어 주므로 마지막 입력만 현재 턴으로 본다.
fn notify_prompt(payload: &Value) -> String {
    let Some(Value::Array(messages)) = payload.get("input-messages") else {
        return String::new();
    };
    messages.iter().map(|message| trimmed(Some(message))).filter(|message| !message.is_empty()).last().unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub enum GateDecision {
    Rejected(&'static str),
    Accepted { bound_external_session_id: Option<String>, turn: CodexRolloutTurn },
}

#[derive(Clone)]
pub struct GateOptions {
    pub cwd: Option<String>,
    pub external_session_id: Option<String>,
    /// None이면 `~/.codex/sessions`다.
    pub sessions_root: Option<String>,
    pub retry_count: u32,
    pub retry_delay_ms: u64,
    pub now: fn() -> f64,
    pub cache: Arc<RolloutPathCache>,
}

impl Default for GateOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            external_session_id: None,
            sessions_root: None,
            retry_count: DEFAULT_RETRY_COUNT,
            retry_delay_ms: DEFAULT_RETRY_DELAY_MS,
            now: now_milliseconds,
            cache: Arc::default(),
        }
    }
}

/// CodexTerminalTurnGate. notify가 가리킨 턴을 rollout에서 찾아 같은 작업 폴더·세션인지 확인한다.
pub struct CodexTerminalTurnGate {
    pub cwd: Option<String>,
    pub external_session_id: Option<String>,
    sessions_root: Option<String>,
    retry_count: u32,
    retry_delay_ms: u64,
    now: fn() -> f64,
    cache: Arc<RolloutPathCache>,
    seen: HashSet<String>,
    pub rollout_path: Option<String>,
}

impl CodexTerminalTurnGate {
    pub fn new(options: GateOptions) -> Self {
        Self {
            cwd: options.cwd,
            external_session_id: options.external_session_id.as_deref().and_then(clean_id),
            sessions_root: options.sessions_root,
            retry_count: options.retry_count,
            retry_delay_ms: options.retry_delay_ms,
            now: options.now,
            cache: options.cache,
            seen: HashSet::new(),
            rollout_path: None,
        }
    }

    async fn find_path(&self, thread_id: &str) -> Result<Option<String>, String> {
        let (thread_id, root, now, cache) = (
            thread_id.to_string(),
            self.sessions_root.clone().unwrap_or_else(default_sessions_root),
            (self.now)(),
            Arc::clone(&self.cache),
        );
        blocking(move || find_codex_rollout_path(Some(&thread_id), &root, now, &cache).map_err(|error| error.to_string())).await
    }

    pub async fn accept(&mut self, payload: &Value) -> Result<GateDecision, String> {
        if payload.get("type").and_then(Value::as_str) != Some("agent-turn-complete") {
            return Ok(GateDecision::Rejected("unsupported-event"));
        }
        let (Some(thread_id), Some(turn_id)) = (clean_value_id(payload.get("thread-id")), clean_value_id(payload.get("turn-id")))
        else {
            return Ok(GateDecision::Rejected("invalid-identifiers"));
        };
        if self.external_session_id.as_ref().is_some_and(|external| *external != thread_id) {
            return Ok(GateDecision::Rejected("thread-mismatch"));
        }
        let dedupe_key = format!("{thread_id}:{turn_id}");
        if self.seen.contains(&dedupe_key) {
            return Ok(GateDecision::Rejected("duplicate"));
        }
        let mut turn = None;
        let mut path = self.rollout_path.clone();
        for attempt in 0..self.retry_count {
            if path.is_none() {
                path = self.find_path(&thread_id).await?;
            }
            let (reading, wanted) = (path.clone(), turn_id.clone());
            turn = blocking(move || read_codex_rollout_turn(reading.as_deref(), &wanted)).await?;
            if turn.is_some() {
                break;
            }
            if attempt + 1 < self.retry_count {
                tokio::time::sleep(Duration::from_millis(self.retry_delay_ms)).await;
            }
        }
        let Some(mut turn) = turn else {
            return Ok(GateDecision::Rejected("rollout-turn-not-found"));
        };
        if canonical_path(turn.cwd.as_deref()).await != canonical_path(self.cwd.as_deref()).await {
            return Ok(GateDecision::Rejected("cwd-mismatch"));
        }
        let is_new_session = self.external_session_id.is_none();
        if is_new_session && turn.source.as_deref() != Some("cli") {
            return Ok(GateDecision::Rejected("new-session-not-cli"));
        }
        if is_new_session {
            self.external_session_id = Some(thread_id.clone());
        }
        self.rollout_path = path;
        self.seen.insert(dedupe_key);
        if turn.prompt.is_empty() {
            turn.prompt = notify_prompt(payload);
        }
        if turn.response.is_empty() {
            turn.response = trimmed(payload.get("last-assistant-message"));
        }
        turn.thread_id = Some(thread_id.clone());
        Ok(GateDecision::Accepted { bound_external_session_id: is_new_session.then_some(thread_id), turn })
    }
}

/// 파일 읽기를 tokio 작업 스레드 밖에서 돌린다. 안에서 난 패닉은 그대로 다시 일으킨다.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(error.to_string()),
    }
}

/// path.basename. 끝의 '/'는 무시한다.
fn basename(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    path.rsplit('/').next().unwrap_or_default()
}

/// codexRolloutFilenameMatches.
pub fn codex_rollout_filename_matches(path: Option<&str>, thread_id: Option<&str>) -> bool {
    clean_id(thread_id.unwrap_or_default()).is_some_and(|id| basename(path.unwrap_or_default()).ends_with(&format!("-{id}.jsonl")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const THREAD_ID: &str = "01a043c6-c6f2-7882-b4d7-a3e118bd103f";
    const TURN_ID: &str = "01a061d3-17cc-7023-8623-42e1572aeac4";
    const TITLE_THREAD_ID: &str = "01a061d3-1b81-7a93-aaa9-af4aa826e297";
    const TITLE_TURN_ID: &str = "01a061d3-1c3c-74c1-8842-4744d9e37945";
    const CWD: &str = "/tmp/officestra-terminal-codex";

    fn rollout_lines(source: &str, actual_cwd: &str) -> Vec<Value> {
        vec![
            json!({ "type": "session_meta", "payload": { "id": THREAD_ID, "source": source, "cwd": "/old" } }),
            json!({ "type": "turn_context", "payload": { "turn_id": TURN_ID, "cwd": actual_cwd } }),
            json!({ "type": "event_msg", "payload": { "type": "task_started", "turn_id": TURN_ID, "started_at": 1_788_347_510 } }),
            json!({ "type": "event_msg", "payload": { "type": "item_completed", "turn_id": TURN_ID, "item": { "type": "UserMessage", "content": [{ "type": "text", "text": "정식 질문" }, { "type": "local_image", "path": "/tmp/a.png" }] } } }),
            json!({ "type": "event_msg", "payload": { "type": "item_completed", "turn_id": TURN_ID, "item": { "type": "AgentMessage", "phase": "final_answer", "content": [{ "type": "Text", "text": "정식 답변" }] } } }),
            json!({ "type": "event_msg", "payload": { "type": "task_complete", "turn_id": TURN_ID, "last_agent_message": "정식 답변", "started_at": 1_788_347_510, "completed_at": 1_788_347_513, "duration_ms": 3_000 } }),
        ]
    }

    fn response_item_rollout_lines() -> Vec<Value> {
        let metadata = json!({ "turn_id": TURN_ID, "create_time": 1_788_347_511 });
        vec![
            json!({ "type": "session_meta", "payload": { "id": THREAD_ID, "source": "exec", "cwd": "/old" } }),
            json!({ "type": "event_msg", "payload": { "type": "task_started", "turn_id": TURN_ID, "started_at": 1_788_347_510 } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "내부 지침" }], "internal_chat_message_metadata_passthrough": metadata } }),
            json!({ "type": "turn_context", "payload": { "turn_id": TURN_ID, "cwd": CWD } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "오 이제 보이네" }], "internal_chat_message_metadata_passthrough": metadata } }),
            json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "phase": "final_answer", "content": [{ "type": "output_text", "text": "확인했습니다." }], "internal_chat_message_metadata_passthrough": metadata } }),
            json!({ "type": "event_msg", "payload": { "type": "task_complete", "turn_id": TURN_ID, "last_agent_message": "확인했습니다.", "started_at": 1_788_347_510, "completed_at": 1_788_347_513 } }),
        ]
    }

    struct Fixture {
        root: String,
        path: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(lines: &[Value]) -> Fixture {
        let root = std::env::temp_dir().join(format!("codex-rollout-turns-{}-{}-{}", std::process::id(), now_milliseconds(), next_fixture()));
        let day = root.join("2026").join("09").join("02");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(CWD).unwrap();
        let path = day.join(format!("rollout-measure-{THREAD_ID}.jsonl"));
        let encoded: Vec<String> = lines.iter().map(|line| String::from_utf8(crate::js::to_js_json(line).unwrap()).unwrap()).collect();
        std::fs::write(&path, encoded.join("\n") + "\n").unwrap();
        Fixture { root: root.to_string_lossy().into_owned(), path: path.to_string_lossy().into_owned() }
    }

    fn next_fixture() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    fn real_notify() -> Value {
        json!({
            "type": "agent-turn-complete", "thread-id": THREAD_ID, "turn-id": TURN_ID, "cwd": CWD,
            "input-messages": ["정식 질문"], "last-assistant-message": "정식 답변",
        })
    }

    fn gate(root: &str, external_session_id: Option<&str>) -> CodexTerminalTurnGate {
        CodexTerminalTurnGate::new(GateOptions {
            cwd: Some(CWD.into()),
            external_session_id: external_session_id.map(str::to_string),
            sessions_root: Some(root.into()),
            retry_count: 1,
            ..Default::default()
        })
    }

    fn rejected(decision: GateDecision) -> &'static str {
        match decision {
            GateDecision::Rejected(reason) => reason,
            GateDecision::Accepted { .. } => "accepted",
        }
    }

    fn accepted(decision: GateDecision) -> (Option<String>, CodexRolloutTurn) {
        match decision {
            GateDecision::Accepted { bound_external_session_id, turn } => (bound_external_session_id, turn),
            GateDecision::Rejected(reason) => panic!("거절됨 {reason}"),
        }
    }

    #[test]
    fn rollout의_정식_turn_context와_task_complete를_읽는다() {
        let fixture = fixture(&rollout_lines("exec", CWD));
        let cache = RolloutPathCache::default();
        assert_eq!(
            find_codex_rollout_path(Some(THREAD_ID), &fixture.root, now_milliseconds(), &cache).unwrap().as_deref(),
            Some(fixture.path.as_str())
        );
        let turn = read_codex_rollout_turn(Some(&fixture.path), TURN_ID).unwrap().unwrap();
        assert_eq!(turn.cwd.as_deref(), Some(CWD));
        assert_eq!(turn.prompt, "정식 질문\n[첨부: /tmp/a.png]");
        assert_eq!(turn.response, "정식 답변");
        assert_eq!(turn.started_at, Some(1_788_347_510.0));
        assert_eq!(turn.completed_at, Some(1_788_347_513.0));
    }

    #[tokio::test]
    async fn 현재_response_item_형식에서_해당_턴_사용자_입력만_읽는다() {
        let fixture = fixture(&response_item_rollout_lines());
        let turn = read_codex_rollout_turn(Some(&fixture.path), TURN_ID).unwrap().unwrap();
        assert_eq!(turn.prompt, "오 이제 보이네");
        assert_eq!(turn.response, "확인했습니다.");
        let mut notify = real_notify();
        notify["input-messages"] = json!(["과거 질문 1", "링크드인 과거 질문", "오 이제 보이네"]);
        let (_, turn) = accepted(gate(&fixture.root, Some(THREAD_ID)).accept(&notify).await.unwrap());
        assert_eq!(turn.prompt, "오 이제 보이네");
    }

    #[tokio::test]
    async fn rollout에_사용자_본문이_없으면_notify의_마지막_입력만_쓴다() {
        let lines: Vec<Value> =
            rollout_lines("exec", CWD).into_iter().filter(|record| record["payload"]["type"] != "item_completed").collect();
        let fixture = fixture(&lines);
        let mut notify = real_notify();
        notify["input-messages"] = json!(["과거 질문", "현재 질문"]);
        let (_, turn) = accepted(gate(&fixture.root, Some(THREAD_ID)).accept(&notify).await.unwrap());
        assert_eq!(turn.prompt, "현재 질문");
    }

    #[tokio::test]
    async fn 정식_notify만_받고_rollout_없는_제목_생성_notify는_버린다() {
        let fixture = fixture(&rollout_lines("exec", CWD));
        let mut gate = gate(&fixture.root, Some(THREAD_ID));
        accepted(gate.accept(&real_notify()).await.unwrap());
        let title = json!({
            "type": "agent-turn-complete", "thread-id": TITLE_THREAD_ID, "turn-id": TITLE_TURN_ID,
            "input-messages": ["Generate a concise task title"], "last-assistant-message": "{\"title\":\"측정\"}",
        });
        assert_eq!(rejected(gate.accept(&title).await.unwrap()), "thread-mismatch");
    }

    #[tokio::test]
    async fn cwd_불일치와_재개_세션_thread_id_불일치를_버린다() {
        let fixture = fixture(&rollout_lines("exec", "/tmp/wrong"));
        assert_eq!(rejected(gate(&fixture.root, Some(THREAD_ID)).accept(&real_notify()).await.unwrap()), "cwd-mismatch");
        let mut thread_gate = gate(&fixture.root, Some("01a00000-0000-7000-8000-000000000000"));
        assert_eq!(rejected(thread_gate.accept(&real_notify()).await.unwrap()), "thread-mismatch");
    }

    #[tokio::test]
    async fn 신규_cli_세션_첫_정식_이벤트를_묶고_turn_id_중복을_버린다() {
        let fixture = fixture(&rollout_lines("cli", CWD));
        let mut gate = gate(&fixture.root, None);
        let (bound, _) = accepted(gate.accept(&real_notify()).await.unwrap());
        assert_eq!(bound.as_deref(), Some(THREAD_ID));
        assert_eq!(gate.external_session_id.as_deref(), Some(THREAD_ID));
        assert_eq!(rejected(gate.accept(&real_notify()).await.unwrap()), "duplicate");
    }

    #[tokio::test]
    async fn 신규_세션은_rollout_source_cli만_허용한다() {
        let fixture = fixture(&rollout_lines("exec", CWD));
        assert_eq!(rejected(gate(&fixture.root, None).accept(&real_notify()).await.unwrap()), "new-session-not-cli");
    }

    #[test]
    fn 턴_id_없는_활동은_현재_턴_경계_안에서만_모으고_최종_답변은_지킨다() {
        let mut lines = response_item_rollout_lines();
        let activity = |text: &str| {
            json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "phase": "commentary", "content": [{ "type": "output_text", "text": text }] } })
        };
        let mut other_turn = activity("다른 턴");
        other_turn["payload"]["internal_chat_message_metadata_passthrough"] = json!({ "turn_id": TITLE_TURN_ID });
        let inserted = [
            activity("파일을 확인합니다"),
            json!({ "type": "response_item", "payload": { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "공개 요약" }], "encrypted_content": "private" } }),
            json!({ "type": "response_item", "payload": { "type": "custom_tool_call", "call_id": "c1", "name": "exec", "input": "비공개 도구 원문" } }),
            json!({ "type": "response_item", "payload": { "type": "custom_tool_call_output", "call_id": "c1", "output": "도구 출력 전문" } }),
            other_turn,
        ];
        lines.splice(4..4, inserted);
        lines.extend([
            activity("끝난 뒤 메시지"),
            json!({ "type": "turn_context", "payload": { "turn_id": TITLE_TURN_ID, "cwd": CWD } }),
            activity("다음 턴 메시지"),
        ]);
        let fixture = fixture(&lines);
        let turn = read_codex_rollout_turn(Some(&fixture.path), TURN_ID).unwrap().unwrap();
        assert_eq!(turn.prompt, "오 이제 보이네");
        assert_eq!(turn.response, "확인했습니다.");
        let summary: Vec<Value> =
            turn.activities.iter().map(|activity| json!({ "kind": activity["kind"], "text": activity["text"] })).collect();
        assert_eq!(
            summary,
            [
                json!({ "kind": "message", "text": "파일을 확인합니다" }),
                json!({ "kind": "thinking", "text": "공개 요약" }),
                json!({ "kind": "tool", "text": "도구 · exec" }),
            ]
        );
    }

    #[test]
    fn 날짜_폴더는_현지_시각을_따르고_잘못된_시각은_nan이다() {
        assert_eq!(date_parts(f64::NAN), ["NaN", "NaN", "NaN"]);
        assert_eq!(date_parts(8.64e15 + 1.0), ["NaN", "NaN", "NaN"]);
        let [year, month, day] = date_parts(1_788_347_510_000.0);
        assert_eq!((year.len(), month.len(), day.len()), (4, 2, 2));
        assert!(codex_rollout_filename_matches(Some(&format!("/a/rollout-x-{THREAD_ID}.jsonl/")), Some(&format!(" {THREAD_ID} "))));
        assert!(!codex_rollout_filename_matches(Some("/a/rollout.jsonl"), Some("not-an-id")));
    }
}
