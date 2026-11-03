// 터미널 턴의 사용량을 각 CLI가 디스크에 남긴 기록에서 턴 구간만큼 합산한다. Node terminal-usage.mjs를 옮겼다.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use serde_json::{Map, Value, json};

use crate::agent_event_parser::{
    USAGE_COUNT_FIELDS, USAGE_TEXT_FIELDS, claude_message_model, claude_message_usage, claude_session_usage_key,
    codex_request_usage,
};
use crate::js::{date_parse, js_string, js_to_number, now_milliseconds, parse_js_json, to_js_json};

/// 턴 구간. 밀리초이고 잘못된 값은 None과 같다. 끝을 모르면 지금까지다.
#[derive(Clone, Copy, Debug, Default)]
pub struct TurnWindow {
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
}

/// Codex 턴 경계를 rollout의 task_started~task_complete로 잡을지(Task), 시각 구간으로 잡을지(Window).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodexScope {
    #[default]
    Task,
    Window,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeTranscriptTurn {
    pub usage: Option<Value>,
    pub model: Option<String>,
}

/// addUsage. 숫자 칸은 더하고 글자 칸은 처음 나온 값을 지킨다. reportedCostUsd는 늘 null이다.
/// 들어오는 사용량은 normalizedUsage를 거친 값이라 숫자 칸은 수나 null뿐이다.
pub fn add_usage(total: Option<Value>, usage: Option<&Value>) -> Option<Value> {
    let Some(usage) = usage.filter(|usage| crate::js::js_truthy(Some(usage))) else {
        return total;
    };
    let mut result = total.unwrap_or_else(empty_usage);
    let Some(fields) = result.as_object_mut() else {
        return Some(result);
    };
    for field in USAGE_COUNT_FIELDS {
        let Some(value) = usage.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let current = fields.get(field).filter(|current| !current.is_null()).map_or(0.0, js_to_number);
        fields.insert(field.into(), Value::from(current + js_to_number(value)));
    }
    for field in USAGE_TEXT_FIELDS {
        if fields.get(field).is_none_or(Value::is_null) {
            fields.insert(field.into(), usage.get(field).cloned().unwrap_or(Value::Null));
        }
    }
    Some(result)
}

fn empty_usage() -> Value {
    let mut usage = Map::new();
    for field in USAGE_COUNT_FIELDS.iter().chain(USAGE_TEXT_FIELDS.iter()).chain(["reportedCostUsd"].iter()) {
        usage.insert((*field).into(), Value::Null);
    }
    Value::Object(usage)
}

pub fn claude_transcript_turn_usage(path: Option<&str>, window: TurnWindow) -> Option<Value> {
    claude_transcript_turn(path, window).usage
}

/// 사용량을 더하는 같은 순회에서 턴 구간에 마지막으로 응답한 실제 모델 ID도 읽는다.
pub fn claude_transcript_turn(path: Option<&str>, window: TurnWindow) -> ClaudeTranscriptTurn {
    let mut model = None;
    let usage = sum_turn_usage(path, window, |object| {
        if object.get("type") != Some(&json!("assistant")) {
            return None;
        }
        if let Some(found) = claude_message_model(object) {
            model = Some(found);
        }
        Some((claude_message_usage(object), claude_session_usage_key(object)))
    });
    ClaudeTranscriptTurn { usage, model }
}

/// 턴 구간 안의 기록만 골라 사용량을 더한다. 기록마다 다른 사용량과 중복 열쇠 뽑기만 호출자가 넘긴다.
fn sum_turn_usage(
    path: Option<&str>,
    window: TurnWindow,
    mut extract: impl FnMut(&Value) -> Option<(Option<Value>, Option<String>)>,
) -> Option<Value> {
    let from = finite(window.started_at);
    let to = finite(window.ended_at).unwrap_or_else(now_milliseconds);
    let (path, from) = (path.filter(|path| !path.is_empty())?, from?);
    let mut seen = HashSet::new();
    let mut total = None;
    let mut lines = Lines::open(path, 0).ok()?;
    while let Some(line) = lines.next_line().ok()? {
        let Some(object) = parsed_line(&line) else { continue };
        let Some(at) = boundary(object.get("timestamp")) else { continue };
        if at < from || at > to {
            continue;
        }
        let Some((Some(usage), key)) = extract(&object) else { continue };
        if let Some(key) = key
            && !seen.insert(key)
        {
            continue;
        }
        total = add_usage(total, Some(&usage));
    }
    total
}

/// Codex 턴 종료 시각은 초 단위로 잘려 저장되어 마지막 token_count를 놓칠 수 있다.
/// 그래서 기본은 rollout이 스스로 남긴 task_started~task_complete 구간을 턴 경계로 삼는다.
pub fn codex_rollout_turn_usage(
    path: Option<&str>,
    window: TurnWindow,
    scope: CodexScope,
    start_offset: u64,
) -> Option<Value> {
    let from = finite(window.started_at);
    let (path, from) = (path.filter(|path| !path.is_empty())?, from?);
    let started_second = (from / 1_000.0).floor();
    let to = finite(window.ended_at).unwrap_or_else(now_milliseconds);
    let mut inside = false;
    let mut total = None;
    let mut context = json!({});
    let mut seen = HashSet::new();
    let mut request_usages = Vec::new();
    let mut lines = Lines::open(path, start_offset).ok()?;
    while let Some(line) = lines.next_line().ok()? {
        let Some(object) = parsed_line(&line) else { continue };
        let payload = object.get("payload");
        let kind = object.get("type");
        if kind == Some(&json!("turn_context")) {
            context = payload.filter(|payload| !payload.is_null()).cloned().unwrap_or_else(|| json!({}));
            continue;
        }
        let Some(payload) = payload.filter(|payload| crate::js::js_truthy(Some(payload))) else { continue };
        if kind != Some(&json!("event_msg")) {
            continue;
        }
        if scope == CodexScope::Window {
            let Some(at) = boundary(object.get("timestamp")) else { continue };
            if at < from || at > to {
                continue;
            }
            inside = true;
        }
        let payload_type = payload.get("type");
        if payload_type == Some(&json!("task_started")) {
            if scope == CodexScope::Task {
                inside = payload.get("started_at").map_or(f64::NAN, js_to_number) == started_second;
            }
            continue;
        }
        if !inside {
            continue;
        }
        if payload_type == Some(&json!("task_complete")) && scope == CodexScope::Task {
            break;
        }
        let Some(usage) = codex_request_usage(&object) else { continue };
        // 사용량 한도 갱신은 마지막 요청 사용량을 그대로 되풀이할 수 있다. 다른 요청은 누적 총량이 다르다.
        let snapshot = payload.get("info").and_then(|info| info.get("total_token_usage")).filter(|value| !value.is_null());
        let key = Value::Array(vec![
            context.get("turn_id").cloned().unwrap_or(Value::Null),
            snapshot.or(object.get("timestamp")).cloned().unwrap_or(Value::Null),
        ]);
        if !seen.insert(to_js_json(&key).ok()?) {
            continue;
        }
        total = add_usage(total, Some(&usage));
        let tier = usage.get("serviceTier").filter(|tier| !tier.is_null()).or_else(|| context.get("service_tier"));
        let mut request = Map::new();
        request.insert("usage".into(), usage.clone());
        if let Some(model) = context.get("model").filter(|model| model.is_string()) {
            request.insert("model".into(), model.clone());
        }
        match tier.and_then(Value::as_str) {
            Some("fast") => _ = request.insert("fastMode".into(), json!(true)),
            Some("default") => _ = request.insert("fastMode".into(), json!(false)),
            _ => {}
        }
        if let Some(timestamp) = object.get("timestamp") {
            request.insert("pricedAt".into(), timestamp.clone());
        }
        request_usages.push(Value::Object(request));
    }
    let mut total = total?;
    if let Some(fields) = total.as_object_mut() {
        fields.insert("requestUsages".into(), Value::Array(request_usages));
    }
    Some(total)
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

/// 빈 줄과 JSON이 아닌 줄은 건너뛴다. 짝 없는 대리 문자 이스케이프도 Node JSON.parse처럼 읽는다.
fn parsed_line(line: &str) -> Option<Value> {
    if line.is_empty() { None } else { parse_js_json(line).ok() }
}

/// `new Date(value ?? NaN).getTime()`. 글자는 Date.parse, 수는 TimeClip, 배열·객체는 글자로 바꿔 읽는다.
fn boundary(value: Option<&Value>) -> Option<f64> {
    let time = match value? {
        Value::Null => return None,
        Value::String(text) => date_parse(text)?,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Number(number) => number.as_f64().filter(|time| time.abs() <= 8.64e15)?.trunc() + 0.0,
        other => date_parse(&js_string(other))?,
    };
    time.is_finite().then_some(time)
}

/// Node readline(crlfDelay: Infinity)처럼 `\r\n`, `\n`, 단독 `\r`, U+2028, U+2029를 줄 끝으로 본다. 파일 끝에 붙은 줄 끝은 빈 줄을 만들지 않는다.
/// 글자는 UTF-8로 읽고 깨진 바이트는 U+FFFD다.
pub(crate) struct Lines {
    reader: BufReader<File>,
    pending: std::collections::VecDeque<String>,
}

impl Lines {
    pub(crate) fn open(path: &str, start: u64) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        Ok(Self { reader: BufReader::new(file), pending: Default::default() })
    }

    pub(crate) fn next_line(&mut self) -> std::io::Result<Option<String>> {
        while self.pending.is_empty() {
            let mut bytes = Vec::new();
            if self.reader.read_until(b'\n', &mut bytes)? == 0 {
                return Ok(None);
            }
            // 조각은 늘 `\n`에서 끝나므로 여러 바이트 글자나 `\r\n`이 조각 사이에 걸치지 않는다.
            let text = String::from_utf8_lossy(&bytes);
            let mut line = String::new();
            let mut characters = text.chars().peekable();
            while let Some(character) = characters.next() {
                match character {
                    '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                        if character == '\r' && characters.peek() == Some(&'\n') {
                            characters.next();
                        }
                        self.pending.push_back(std::mem::take(&mut line));
                    }
                    other => line.push(other),
                }
            }
            if !line.is_empty() {
                self.pending.push_back(line);
            }
        }
        Ok(self.pending.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED_AT: &str = "2026-09-02T14:00:00.000Z";
    const ENDED_AT: &str = "2026-09-02T14:01:00.000Z";

    fn window(started_at: Option<&str>, ended_at: Option<&str>) -> TurnWindow {
        TurnWindow { started_at: started_at.and_then(date_parse), ended_at: ended_at.and_then(date_parse) }
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str, body: &str) -> Self {
            let directory = std::env::temp_dir().join(format!("officestra-terminal-usage-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("session.jsonl"), body).unwrap();
            Self(directory)
        }

        fn path(&self) -> String {
            self.0.join("session.jsonl").to_str().unwrap().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn 줄_끝은_node_readline처럼_crlf_lf_cr_u2028_u2029다() {
        // Node v24 readline(crlfDelay: Infinity)이 같은 바이트에서 낸 ["a","","b","c","d","e"]다.
        let scratch = Scratch::new("line-endings", "a\u{2028}\nb\r\nc\rd\u{2029}e\r");
        let mut lines = Lines::open(&scratch.path(), 0).unwrap();
        let mut read = Vec::new();
        while let Some(line) = lines.next_line().unwrap() {
            read.push(line);
        }
        assert_eq!(read, ["a", "", "b", "c", "d", "e"]);
    }

    fn claude_line(timestamp: &str, id: &str, usage: Value, model: Option<&str>, sidechain: bool) -> String {
        let mut object = json!({ "type": "assistant", "timestamp": timestamp, "uuid": format!("{id}-{timestamp}") });
        if sidechain {
            object["isSidechain"] = json!(true);
        }
        object["message"] = json!({ "id": id, "usage": usage });
        if let Some(model) = model {
            object["message"]["model"] = json!(model);
        }
        object.to_string()
    }

    fn codex_line(timestamp: &str, last: Value, total: Value) -> String {
        json!({ "timestamp": timestamp, "type": "event_msg", "payload": {
            "type": "token_count", "info": { "last_token_usage": last, "total_token_usage": total },
        } })
        .to_string()
    }

    fn codex_task_line(timestamp: &str, kind: &str, seconds: f64) -> String {
        json!({ "timestamp": timestamp, "type": "event_msg", "payload": { "type": kind, "started_at": seconds } }).to_string()
    }

    fn codex_rollout() -> String {
        let second = (date_parse(STARTED_AT).unwrap() / 1_000.0).floor();
        [
            codex_task_line("2026-09-02T13:58:00.000Z", "task_started", second - 200.0),
            codex_line("2026-09-02T13:59:00.000Z", json!({ "input_tokens": 5_000, "output_tokens": 500 }), json!({ "input_tokens": 5_000, "output_tokens": 500 })),
            codex_task_line("2026-09-02T13:59:01.000Z", "task_complete", second - 200.0),
            codex_task_line("2026-09-02T14:00:00.100Z", "task_started", second),
            codex_line(
                "2026-09-02T14:00:20.000Z",
                json!({ "input_tokens": 1_200, "output_tokens": 30, "cached_input_tokens": 900, "cache_write_input_tokens": 10, "reasoning_output_tokens": 12 }),
                json!({ "input_tokens": 400, "output_tokens": 10 }),
            ),
            codex_line(
                "2026-09-02T14:01:00.700Z",
                json!({ "input_tokens": 800, "output_tokens": 20, "cached_input_tokens": 600, "reasoning_output_tokens": 8 }),
                json!({ "input_tokens": 1_200, "output_tokens": 30 }),
            ),
            codex_task_line("2026-09-02T14:01:00.850Z", "task_complete", second),
            codex_task_line("2026-09-02T14:02:00.000Z", "task_started", second + 120.0),
            codex_line("2026-09-02T14:02:10.000Z", json!({ "input_tokens": 9_000, "output_tokens": 900 }), json!({ "input_tokens": 10_200, "output_tokens": 930 })),
        ]
        .join("\n")
    }

    #[test]
    fn codex_drops_repeated_token_counts_but_keeps_same_size_requests() {
        let prefix = "이전 기록\n";
        let last = json!({ "input_tokens": 150000, "output_tokens": 100 });
        let first = codex_line("2026-09-02T14:00:10.000Z", last.clone(), last.clone());
        let context = json!({ "type": "turn_context", "payload": { "turn_id": "turn-1", "model": "gpt-6-astra", "service_tier": "fast" } }).to_string();
        let body = [
            context,
            first.clone(),
            first,
            codex_line("2026-09-02T14:00:20.000Z", last.clone(), json!({ "input_tokens": 300000, "output_tokens": 200 })),
            codex_line("2026-09-02T14:02:20.000Z", last, json!({ "input_tokens": 450000, "output_tokens": 300 })),
        ]
        .join("\n");
        let scratch = Scratch::new("repeat", &(prefix.to_string() + &body));
        let usage = codex_rollout_turn_usage(
            Some(&scratch.path()),
            window(Some(STARTED_AT), Some(ENDED_AT)),
            CodexScope::Window,
            prefix.len() as u64,
        )
        .unwrap();
        assert_eq!(usage["inputTokens"], json!(300000.0));
        assert_eq!(usage["outputTokens"], json!(200.0));
        let requests = usage["requestUsages"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], json!("gpt-6-astra"));
        assert_eq!(requests[0]["fastMode"], json!(true));
    }

    #[test]
    fn claude_counts_only_turn_window_and_each_response_once() {
        let body = [
            claude_line("2026-09-02T13:59:59.000Z", "msg_before", json!({ "input_tokens": 900, "output_tokens": 900 }), None, false),
            claude_line(
                "2026-09-02T14:00:10.000Z",
                "msg_1",
                json!({ "input_tokens": 2, "output_tokens": 100, "cache_read_input_tokens": 1_000, "cache_creation_input_tokens": 40,
                    "cache_creation": { "ephemeral_5m_input_tokens": 10, "ephemeral_1h_input_tokens": 30 } }),
                None,
                false,
            ),
            claude_line(
                "2026-09-02T14:00:11.000Z",
                "msg_1",
                json!({ "input_tokens": 2, "output_tokens": 100, "cache_read_input_tokens": 1_000, "cache_creation_input_tokens": 40 }),
                None,
                false,
            ),
            claude_line("2026-09-02T14:00:30.000Z", "msg_2", json!({ "input_tokens": 3, "output_tokens": 50, "cache_read_input_tokens": 500 }), None, false),
            claude_line("2026-09-02T14:01:30.000Z", "msg_after", json!({ "input_tokens": 700, "output_tokens": 700 }), None, false),
            "깨진 줄".to_string(),
        ]
        .join("\n");
        let scratch = Scratch::new("claude", &body);
        let usage = claude_transcript_turn_usage(Some(&scratch.path()), window(Some(STARTED_AT), Some(ENDED_AT))).unwrap();
        let counts: Vec<Value> = ["inputTokens", "outputTokens", "cachedInputTokens", "cacheWriteInputTokens", "cacheWrite5mInputTokens", "cacheWrite1hInputTokens"]
            .iter()
            .map(|field| usage[*field].clone())
            .collect();
        assert_eq!(counts, [json!(5.0), json!(150.0), json!(1_500.0), json!(40.0), json!(10.0), json!(30.0)]);
    }

    #[test]
    fn codex_sums_request_usage_inside_task_boundaries() {
        let scratch = Scratch::new("codex", &codex_rollout());
        let usage = codex_rollout_turn_usage(Some(&scratch.path()), window(Some(STARTED_AT), None), CodexScope::Task, 0).unwrap();
        let counts: Vec<Value> = ["inputTokens", "outputTokens", "cachedInputTokens", "cacheWriteInputTokens", "reasoningOutputTokens"]
            .iter()
            .map(|field| usage[*field].clone())
            .collect();
        assert_eq!(counts, [json!(2_000.0), json!(50.0), json!(1_500.0), json!(10.0), json!(20.0)]);
        assert!(usage["inputTokens"].as_f64().unwrap() > 1_200.0);
        let later = codex_rollout_turn_usage(Some(&scratch.path()), window(Some("2026-09-02T15:00:00.000Z"), None), CodexScope::Task, 0);
        assert_eq!(later, None);
    }

    #[test]
    fn missing_records_or_unknown_window_give_no_usage() {
        let scratch = Scratch::new(
            "missing",
            &claude_line("2026-09-02T14:00:10.000Z", "msg_1", json!({ "input_tokens": 2, "output_tokens": 100 }), None, false),
        );
        let path = scratch.path();
        let turn = window(Some(STARTED_AT), Some(ENDED_AT));
        assert_eq!(claude_transcript_turn_usage(None, turn), None);
        let absent = scratch.0.join("없음.jsonl");
        assert_eq!(claude_transcript_turn_usage(absent.to_str(), turn), None);
        assert_eq!(claude_transcript_turn_usage(Some(&path), window(None, Some(ENDED_AT))), None);
        assert_eq!(codex_rollout_turn_usage(None, window(Some(STARTED_AT), None), CodexScope::Task, 0), None);
        assert_eq!(codex_rollout_turn_usage(Some(&path), window(Some(STARTED_AT), None), CodexScope::Task, 0), None);
    }

    #[test]
    fn claude_reads_last_main_conversation_model() {
        let usage = json!({ "input_tokens": 1, "output_tokens": 1 });
        let body = [
            claude_line("2026-09-02T13:59:59.000Z", "msg_before", usage.clone(), Some("claude-sonnet-5"), false),
            claude_line("2026-09-02T14:00:10.000Z", "msg_1", usage.clone(), Some("claude-opus-5-5"), false),
            claude_line("2026-09-02T14:00:20.000Z", "msg_2", usage.clone(), Some("claude-haiku-4-5-20251001"), true),
            claude_line("2026-09-02T14:00:30.000Z", "msg_3", usage, Some("<synthetic>"), false),
        ]
        .join("\n");
        let scratch = Scratch::new("model", &body);
        let turn = claude_transcript_turn(Some(&scratch.path()), window(Some(STARTED_AT), Some(ENDED_AT)));
        assert_eq!(turn.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_transcript_turn(None, window(Some(STARTED_AT), None)), ClaudeTranscriptTurn::default());
    }

    #[test]
    fn readline_breaks_and_js_date_boundaries() {
        let scratch = Scratch::new("lines", "a\r\r\nb\rc\n\nd");
        let mut lines = Lines::open(&scratch.path(), 0).unwrap();
        let mut seen = Vec::new();
        while let Some(line) = lines.next_line().unwrap() {
            seen.push(line);
        }
        assert_eq!(seen, ["a", "", "b", "c", "", "d"]);
        assert_eq!(boundary(Some(&json!(1.9))), Some(1.0));
        assert_eq!(boundary(Some(&json!(true))), Some(1.0));
        assert_eq!(boundary(Some(&json!(8.64e15 + 1.0))), None);
        assert_eq!(boundary(Some(&json!(["2026-09-02T14:00:10.000Z"]))), date_parse("2026-09-02T14:00:10.000Z"));
        assert_eq!(boundary(Some(&json!({}))), None);
        assert_eq!(boundary(Some(&Value::Null)), None);
    }
}
