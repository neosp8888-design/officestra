// 로컬 모델 응답의 사용량 표기를 검증된 방언대로 바꾼다. Node local-usage-normalizer.mjs·local-responses-usage.mjs와 같다.
// 요청·본문·도구·역할은 건드리지 않는다. 사용량이 없으면 만들어 내지 않는다.

use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

use crate::js::{parse_js_json, to_js_json};

/// LM Studio 0.4.24+1 Messages. input_tokens가 캐시를 포함한다.
pub const INCLUSIVE_INPUT_PROFILE: &str = "lmstudio-0.4.24+1-inclusive-input";
/// 고정한 b10982 /v1/messages. 캐시 안 된 입력만 input_tokens로 따로 보고한다.
pub const LLAMA_MESSAGES_PROFILE: &str = "llama.cpp-b10982-messages-exclusive-input";
/// LM Studio 0.4.24+1 /v1/responses. 입력은 캐시를, 출력은 추론 토큰을 포함한다.
pub const RESPONSES_INCLUSIVE_PROFILE: &str = "lmstudio-0.4.24+1-responses-inclusive";
pub const LLAMA_RESPONSES_PROFILE: &str = "llama.cpp-b10982-responses-inclusive";

/// SSE 한 묶음의 한도. Node는 UTF-16 길이로 재지만 여기서는 바이트로 잰다.
pub(crate) const MAX_SSE_FRAME: usize = 8 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

static FRAME_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n\r?\n").expect("SSE 구분 정규식"));

pub fn verified_responses_profile(profile: &str) -> bool {
    matches!(profile, RESPONSES_INCLUSIVE_PROFILE | LLAMA_RESPONSES_PROFILE)
}

/// 감사용 사용량 기록. Node onUsage에 넘기는 객체와 같은 내용이다.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    pub profile: String,
    /// Messages의 message id. 원래 값을 그대로 둔다.
    pub message_id: Option<Value>,
    /// Responses의 response id. 문자열일 때만 있다.
    pub response_id: Option<String>,
    pub raw: Value,
    pub normalized: Value,
    pub total_input_tokens: u64,
}

pub type UsageCallback = Box<dyn FnMut(UsageRecord) + Send>;
pub type CompletedCallback = Box<dyn FnMut() + Send>;

/// Number.isSafeInteger(value) && value >= 0.
pub(crate) fn safe_nonnegative_integer(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return (number <= MAX_SAFE_INTEGER).then_some(number);
    }
    let number = value.as_f64()?;
    (number.is_finite() && number.trunc() == number && (0.0..=MAX_SAFE_INTEGER as f64).contains(&number)).then_some(number as u64)
}

fn tokens(value: &Value, name: &str) -> Result<u64, String> {
    safe_nonnegative_integer(value).ok_or_else(|| format!("Invalid {name}"))
}

fn plain_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// createUsageNormalizer. 한 응답 안에서 message_start·message_delta 사이 입력 값을 이어 받는다.
pub struct UsageNormalizer {
    profile: &'static str,
    raw_input: Option<u64>,
    cache_read: u64,
    cache_write: u64,
    message_id: Option<Value>,
    on_usage: Option<UsageCallback>,
}

impl UsageNormalizer {
    pub fn new(profile: &str, on_usage: Option<UsageCallback>) -> Result<Self, String> {
        let profile = match profile {
            INCLUSIVE_INPUT_PROFILE => INCLUSIVE_INPUT_PROFILE,
            LLAMA_MESSAGES_PROFILE => LLAMA_MESSAGES_PROFILE,
            _ => return Err("Explicit verified local usage profile required".into()),
        };
        Ok(Self { profile, raw_input: None, cache_read: 0, cache_write: 0, message_id: None, on_usage })
    }

    /// 바뀐 사건이면 Some, 그대로면 None(Node `next === event`)이다.
    pub fn apply(&mut self, event: &Value) -> Result<Option<Value>, String> {
        let kind = event.get("type").and_then(Value::as_str);
        let start = kind == Some("message_start");
        let complete = kind == Some("message");
        if !start && !complete && kind != Some("message_delta") {
            return Ok(None);
        }
        if start || complete {
            self.raw_input = None;
            self.cache_read = 0;
            self.cache_write = 0;
            let holder = if start { event.get("message") } else { Some(event) };
            self.message_id = holder.and_then(Value::as_object).and_then(|fields| fields.get("id")).cloned();
        }
        let usage_value = if start { event.get("message").and_then(Value::as_object).and_then(|message| message.get("usage")) } else { event.get("usage") };
        let Some(usage) = plain_object(usage_value) else {
            return Ok(None);
        };
        let raw = Value::Object(usage.clone());
        if let Some(value) = usage.get("input_tokens") {
            self.raw_input = Some(tokens(value, "input_tokens")?);
        }
        if let Some(value) = usage.get("cache_read_input_tokens") {
            self.cache_read = tokens(value, "cache_read_input_tokens")?;
        }
        if let Some(value) = usage.get("cache_creation_input_tokens") {
            self.cache_write = tokens(value, "cache_creation_input_tokens")?;
        }
        // 관측한 방언에는 캐시 생성이 없다. 새 의미를 짐작하지 않는다.
        if self.cache_write != 0 {
            return Err("Unverified local cache-creation semantics".into());
        }
        if !usage.contains_key("input_tokens") && !usage.contains_key("cache_read_input_tokens") {
            return Ok(None);
        }
        let exclusive = self.profile == LLAMA_MESSAGES_PROFILE;
        let raw_input = match self.raw_input {
            Some(raw_input) if exclusive || self.cache_read <= raw_input => raw_input,
            _ => return Err("Inconsistent inclusive input/cache counts".into()),
        };
        let total = if exclusive { raw_input + self.cache_read } else { raw_input };
        if total > MAX_SAFE_INTEGER {
            return Err("Invalid total input tokens".into());
        }
        let mut normalized = usage.clone();
        normalized.insert("input_tokens".into(), Value::from(if exclusive { raw_input } else { raw_input - self.cache_read }));
        let normalized = Value::Object(normalized);
        if let Some(on_usage) = self.on_usage.as_mut() {
            on_usage(UsageRecord {
                profile: self.profile.to_string(),
                message_id: self.message_id.clone(),
                response_id: None,
                raw,
                normalized: normalized.clone(),
                total_input_tokens: total,
            });
        }
        let mut next = event.clone();
        let target = if start { next.get_mut("message").and_then(Value::as_object_mut) } else { next.as_object_mut() };
        if let Some(target) = target {
            target.insert("usage".into(), normalized);
        }
        Ok(Some(next))
    }
}

/// extractResponsesUsage. 사용량이 없으면 None이다.
pub fn extract_responses_usage(usage: Option<&Value>, profile: &str) -> Result<Option<UsageRecord>, String> {
    if !verified_responses_profile(profile) {
        return Err("Explicit verified local usage profile required".into());
    }
    let Some(fields) = plain_object(usage) else {
        return Ok(None);
    };
    let input = fields.get("input_tokens").map(|value| tokens(value, "input_tokens")).transpose()?;
    let output = fields.get("output_tokens").map(|value| tokens(value, "output_tokens")).transpose()?;
    let total = fields.get("total_tokens").map(|value| tokens(value, "total_tokens")).transpose()?;
    let mut cached = 0;
    if let Some(details) = plain_object(fields.get("input_tokens_details"))
        && let Some(value) = details.get("cached_tokens")
    {
        cached = tokens(value, "cached_tokens")?;
    }
    let mut reasoning = 0;
    if let Some(details) = plain_object(fields.get("output_tokens_details"))
        && let Some(value) = details.get("reasoning_tokens")
    {
        reasoning = tokens(value, "reasoning_tokens")?;
    }
    if input.is_none() && output.is_none() && total.is_none() {
        return Ok(None);
    }
    let (Some(input), Some(output)) = (input, output) else {
        return Err("Malformed Responses usage: input/output tokens required".into());
    };
    if cached > input {
        return Err("Inconsistent inclusive input/cache counts".into());
    }
    if reasoning > output {
        return Err("Inconsistent inclusive output/reasoning counts".into());
    }
    let mut normalized = fields.clone();
    // 캐시 부분만 포함 입력에서 뺀다. 추론 토큰은 실제 생성 출력이라 출력에 남긴다.
    normalized.insert("input_tokens".into(), Value::from(input - cached));
    if total.is_some_and(|total| total != input + output) {
        return Err("Inconsistent Responses usage total".into());
    }
    Ok(Some(UsageRecord {
        profile: profile.to_string(),
        message_id: None,
        response_id: None,
        raw: Value::Object(fields.clone()),
        normalized: Value::Object(normalized),
        total_input_tokens: input,
    }))
}

/// normalizeResponsesUsage. 실패·오류 응답은 성공으로 감사하지 않는다.
pub fn normalize_responses_usage(response: &Value, profile: &str) -> Result<Option<UsageRecord>, String> {
    let Some(fields) = response.as_object() else {
        return Err("Invalid Responses payload".into());
    };
    if !verified_responses_profile(profile) {
        return Err("Explicit verified local usage profile required".into());
    }
    let status = fields.get("status");
    if status.and_then(Value::as_str) == Some("failed") || crate::js::js_truthy(fields.get("error")) {
        return Ok(None);
    }
    if let Some(status) = status
        && status.as_str() != Some("completed")
    {
        return Err(format!("Unverified Responses completion status: {}", crate::js::js_string(status)));
    }
    let Some(mut record) = extract_responses_usage(fields.get("usage"), profile)? else {
        return Ok(None);
    };
    record.response_id = fields.get("id").and_then(Value::as_str).map(str::to_string);
    Ok(Some(record))
}

/// StringDecoder("utf8"). 글자 경계에 걸친 바이트는 다음 조각까지 미루고, 끝에 남으면 U+FFFD 하나다.
#[derive(Default)]
pub(crate) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(crate) fn write(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, invalid) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{FFFD}');
                            rest = &invalid[length..];
                        }
                        None => {
                            rest = invalid;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        text
    }

    pub(crate) fn end(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            self.pending.clear();
            "\u{FFFD}".into()
        }
    }
}

/// 바이트 흐름 변환. 조각마다 내보낼 바이트를 돌려주고, 끝에서 남은 바이트를 내보낸다.
pub trait StreamTransform: Send {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String>;
    fn finish(&mut self) -> Result<Vec<u8>, String>;
}

/// SSE 묶음 나누기. 빈 줄(`\r?\n\r?\n`)로 끊고 묶음마다 frame을 부른다.
pub(crate) struct SseFramer {
    decoder: Utf8Decoder,
    pending: String,
    oversized: &'static str,
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::new("Oversized SSE frame")
    }
}

impl SseFramer {
    pub(crate) fn new(oversized: &'static str) -> Self {
        Self { decoder: Utf8Decoder::default(), pending: String::new(), oversized }
    }

    pub(crate) fn push(&mut self, chunk: &[u8], frame: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<Vec<u8>, String> {
        let text = self.decoder.write(chunk);
        self.pending.push_str(&text);
        self.drain(frame, false)
    }

    pub(crate) fn finish(&mut self, frame: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<Vec<u8>, String> {
        let text = self.decoder.end();
        self.pending.push_str(&text);
        self.drain(frame, true)
    }

    fn drain(&mut self, frame: &mut dyn FnMut(&str) -> Result<String, String>, last: bool) -> Result<Vec<u8>, String> {
        let mut output = String::new();
        let mut consumed = 0;
        while let Some(found) = FRAME_END.find(&self.pending[consumed..]) {
            if found.start() > MAX_SSE_FRAME {
                return Err(self.oversized.into());
            }
            let start = consumed + found.start();
            let end = consumed + found.end();
            output.push_str(&frame(&self.pending[consumed..start])?);
            output.push_str(&self.pending[start..end]);
            consumed = end;
        }
        self.pending.drain(..consumed);
        if self.pending.len() > MAX_SSE_FRAME {
            return Err(self.oversized.into());
        }
        if last && !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            output.push_str(&frame(&rest)?);
        }
        Ok(output.into_bytes())
    }
}

/// 묶음의 data 줄을 합친 본문. data 줄이 없으면 None이다.
pub(crate) fn frame_data(input: &str) -> Option<String> {
    let lines: Vec<&str> = split_lines(input).filter(|line| line.starts_with("data:")).collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.iter().map(|line| line[5..].strip_prefix(' ').unwrap_or(&line[5..])).collect::<Vec<_>>().join("\n"))
}

/// input.split(/\r?\n/).
pub(crate) fn split_lines(input: &str) -> impl Iterator<Item = &str> {
    input.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// 첫 data 줄만 새 JSON으로 바꾸고 나머지 data 줄은 지운다. 줄바꿈은 묶음 안에 \r\n이 있으면 그것을 쓴다.
pub(crate) fn rewrite_frame(input: &str, next: &Value) -> String {
    let eol = if input.contains("\r\n") { "\r\n" } else { "\n" };
    let encoded = String::from_utf8(to_js_json(next).unwrap_or_default()).unwrap_or_default();
    let mut emitted = false;
    let mut lines = Vec::new();
    for line in split_lines(input) {
        if !line.starts_with("data:") {
            lines.push(line.to_string());
        } else if !emitted {
            emitted = true;
            lines.push(format!("data: {encoded}"));
        }
    }
    lines.join(eol)
}

/// createUsageSSETransform. 사용량 표기만 바꾸고, llama.cpp는 message_stop이 없으면 실패다.
pub struct UsageSseTransform {
    framer: SseFramer,
    normalizer: UsageNormalizer,
    llama: bool,
    completed: bool,
    on_completed: Option<CompletedCallback>,
}

impl UsageSseTransform {
    pub fn new(profile: &str, on_usage: Option<UsageCallback>, on_completed: Option<CompletedCallback>) -> Result<Self, String> {
        Ok(Self {
            framer: SseFramer::default(),
            normalizer: UsageNormalizer::new(profile, on_usage)?,
            llama: profile == LLAMA_MESSAGES_PROFILE,
            completed: false,
            on_completed,
        })
    }

    fn frame(&mut self, input: &str) -> Result<String, String> {
        let Some(raw) = frame_data(input) else {
            return Ok(input.to_string());
        };
        let Ok(event) = parse_js_json(&raw) else {
            return Ok(input.to_string());
        };
        if self.llama && event.get("type").and_then(Value::as_str) == Some("message_stop") {
            self.completed = true;
            if let Some(on_completed) = self.on_completed.as_mut() {
                on_completed();
            }
        }
        match self.normalizer.apply(&event)? {
            Some(next) => Ok(rewrite_frame(input, &next)),
            None => Ok(input.to_string()),
        }
    }
}

impl StreamTransform for UsageSseTransform {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::take(&mut self.framer);
        let result = framer.push(chunk, &mut |input| self.frame(input));
        self.framer = framer;
        result
    }

    fn finish(&mut self) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::take(&mut self.framer);
        let result = framer.finish(&mut |input| self.frame(input));
        self.framer = framer;
        let output = result?;
        if self.llama && !self.completed {
            return Err("Incomplete local Messages stream".into());
        }
        Ok(output)
    }
}

/// createResponsesUsageSSETransform. 원래 바이트를 그대로 내보내고 response.completed의 사용량만 감사한다.
pub struct ResponsesUsageSseTransform {
    framer: SseFramer,
    profile: String,
    saw_completion: bool,
    on_usage: Option<UsageCallback>,
    on_completed: Option<CompletedCallback>,
}

impl ResponsesUsageSseTransform {
    pub fn new(profile: &str, on_usage: Option<UsageCallback>, on_completed: Option<CompletedCallback>) -> Result<Self, String> {
        if !verified_responses_profile(profile) {
            return Err("Explicit verified local usage profile required".into());
        }
        Ok(Self { framer: SseFramer::default(), profile: profile.to_string(), saw_completion: false, on_usage, on_completed })
    }

    /// 완료를 먼저 처리한 뒤 내보낸다. Codex는 이 묶음을 받자마자 연결을 닫을 수 있고, 그것은 생성 취소가 아니다.
    fn frame(&mut self, input: &str) -> Result<String, String> {
        let forward = || Ok(input.to_string());
        let Some(raw) = frame_data(input) else {
            return forward();
        };
        let Ok(event) = parse_js_json(&raw) else {
            return forward();
        };
        let kind = event.get("type").and_then(Value::as_str);
        if kind == Some("response.failed") {
            self.saw_completion = true;
            return forward();
        }
        let response = event.get("response").filter(|response| crate::js::js_truthy(Some(response)));
        let (Some("response.completed"), Some(response)) = (kind, response) else {
            return forward();
        };
        self.saw_completion = true;
        if crate::js::js_truthy(response.get("error")) || response.get("status").and_then(Value::as_str) == Some("failed") {
            return forward();
        }
        if let Some(record) = normalize_responses_usage(response, &self.profile)?
            && let Some(on_usage) = self.on_usage.as_mut()
        {
            on_usage(record);
        }
        if let Some(on_completed) = self.on_completed.as_mut() {
            on_completed();
        }
        forward()
    }
}

impl StreamTransform for ResponsesUsageSseTransform {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::take(&mut self.framer);
        let result = framer.push(chunk, &mut |input| self.frame(input));
        self.framer = framer;
        result
    }

    fn finish(&mut self) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::take(&mut self.framer);
        let result = framer.finish(&mut |input| self.frame(input));
        self.framer = framer;
        let output = result?;
        // response.completed·response.failed 없이 끝난 흐름은 완료 턴이 아니다.
        if !self.saw_completion {
            return Err("Responses stream ended without completion".into());
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn collect(transform: &mut dyn StreamTransform, chunks: &[&[u8]]) -> Result<String, String> {
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend(transform.push(chunk)?);
        }
        output.extend(transform.finish()?);
        Ok(String::from_utf8(output).unwrap())
    }

    fn recorder() -> (Arc<Mutex<Vec<UsageRecord>>>, UsageCallback) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (seen, Box::new(move |record| sink.lock().unwrap().push(record)))
    }

    #[test]
    fn inclusive_input_split_keeps_reasoning_in_output() {
        let usage = json!({ "input_tokens": 55, "output_tokens": 16, "total_tokens": 71,
            "input_tokens_details": { "cached_tokens": 42 }, "output_tokens_details": { "reasoning_tokens": 16 } });
        let record = extract_responses_usage(Some(&usage), RESPONSES_INCLUSIVE_PROFILE).unwrap().unwrap();
        assert_eq!(record.profile, RESPONSES_INCLUSIVE_PROFILE);
        assert_eq!(record.raw, usage);
        assert_eq!(record.normalized["input_tokens"], 13);
        assert_eq!(record.normalized["output_tokens"], 16);
        assert_eq!(record.total_input_tokens, 55);
    }

    #[test]
    fn absent_or_malformed_responses_usage() {
        for usage in [None, Some(json!(null)), Some(json!({}))] {
            assert_eq!(extract_responses_usage(usage.as_ref(), RESPONSES_INCLUSIVE_PROFILE).unwrap(), None);
        }
        assert!(extract_responses_usage(Some(&json!({ "input_tokens": 1, "output_tokens": 1 })), "").unwrap_err().contains("profile"));
        let error = |usage: Value| extract_responses_usage(Some(&usage), RESPONSES_INCLUSIVE_PROFILE).unwrap_err();
        assert!(error(json!({ "input_tokens": 55 })).contains("input/output tokens required"));
        assert!(error(json!({ "input_tokens": 55, "output_tokens": 16, "input_tokens_details": { "cached_tokens": 99 } })).contains("Inconsistent inclusive input/cache"));
        assert!(error(json!({ "input_tokens": 55, "output_tokens": 16, "output_tokens_details": { "reasoning_tokens": 99 } })).contains("Inconsistent inclusive output/reasoning"));
        assert!(error(json!({ "input_tokens": -1, "output_tokens": 16 })).contains("Invalid input_tokens"));
        assert!(error(json!({ "input_tokens": 55, "output_tokens": 16, "total_tokens": 99 })).contains("Inconsistent Responses usage total"));
        assert!(error(json!({ "input_tokens": null, "output_tokens": 16 })).contains("Invalid input_tokens"));
        assert!(error(json!({ "input_tokens": 1.5, "output_tokens": 16 })).contains("Invalid input_tokens"));
    }

    #[test]
    fn responses_completion_status() {
        let record = normalize_responses_usage(
            &json!({ "id": "resp_123", "status": "completed", "usage": { "input_tokens": 55, "output_tokens": 16, "total_tokens": 71 } }),
            RESPONSES_INCLUSIVE_PROFILE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(record.response_id.as_deref(), Some("resp_123"));
        assert_eq!(record.total_input_tokens, 55);
        let usage = json!({ "input_tokens": 1, "output_tokens": 1 });
        assert_eq!(normalize_responses_usage(&json!({ "status": "failed", "usage": usage }), RESPONSES_INCLUSIVE_PROFILE).unwrap(), None);
        assert_eq!(
            normalize_responses_usage(&json!({ "status": "completed", "error": { "message": "boom" }, "usage": usage }), RESPONSES_INCLUSIVE_PROFILE).unwrap(),
            None
        );
        assert!(
            normalize_responses_usage(&json!({ "status": "in_progress", "usage": usage }), RESPONSES_INCLUSIVE_PROFILE)
                .unwrap_err()
                .contains("Unverified Responses completion status")
        );
        assert_eq!(normalize_responses_usage(&json!([]), RESPONSES_INCLUSIVE_PROFILE).unwrap_err(), "Invalid Responses payload");
    }

    const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_9\",\"status\":\"completed\",\"usage\":{\"input_tokens\":55,\"output_tokens\":16,\"total_tokens\":71}}}\n\n";

    #[test]
    fn responses_stream_passes_bytes_and_audits_once() {
        let frames = [
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"msg_1\"}}\n\n",
            COMPLETED,
        ];
        let (seen, sink) = recorder();
        let mut transform = ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, Some(sink), None).unwrap();
        let joined = frames.concat();
        assert_eq!(collect(&mut transform, &[joined.as_bytes()]).unwrap(), joined);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].response_id.as_deref(), Some("resp_9"));
        assert_eq!(seen[0].total_input_tokens, 55);
    }

    #[test]
    fn responses_stream_crlf_multi_chunk_and_utf8_split() {
        let crlf = COMPLETED.replace('\n', "\r\n");
        let (seen, sink) = recorder();
        let mut transform = ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, Some(sink), None).unwrap();
        assert_eq!(collect(&mut transform, &[crlf.as_bytes()]).unwrap(), crlf);
        assert_eq!(seen.lock().unwrap().len(), 1);

        let bytes = COMPLETED.as_bytes();
        let (seen, sink) = recorder();
        let mut transform = ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, Some(sink), None).unwrap();
        assert_eq!(collect(&mut transform, &[&bytes[..10], &bytes[10..40], &bytes[40..]]).unwrap(), COMPLETED);
        assert_eq!(seen.lock().unwrap().len(), 1);

        let frame = "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"content\":[{\"text\":\"안녕\"}]}}\n\n";
        let split = frame.bytes().position(|byte| byte & 0xc0 == 0x80).unwrap();
        let (seen, sink) = recorder();
        let mut transform = ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, Some(sink), None).unwrap();
        let text = collect(&mut transform, &[&frame.as_bytes()[..split], &frame.as_bytes()[split..], COMPLETED.as_bytes()]).unwrap();
        assert_eq!(text, format!("{frame}{COMPLETED}"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn responses_stream_failed_error_and_incomplete() {
        let failed = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let errored = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"error\":{\"message\":\"x\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        for frame in [failed, errored] {
            let (seen, sink) = recorder();
            let completed = Arc::new(Mutex::new(0));
            let counter = Arc::clone(&completed);
            let mut transform =
                ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, Some(sink), Some(Box::new(move || *counter.lock().unwrap() += 1))).unwrap();
            assert_eq!(collect(&mut transform, &[frame.as_bytes()]).unwrap(), frame);
            assert!(seen.lock().unwrap().is_empty());
            assert_eq!(*completed.lock().unwrap(), 0);
        }
        let mut transform = ResponsesUsageSseTransform::new(RESPONSES_INCLUSIVE_PROFILE, None, None).unwrap();
        let partial = "data: {\"type\":\"response.created\"}\n\n";
        assert_eq!(collect(&mut transform, &[partial.as_bytes()]).unwrap_err(), "Responses stream ended without completion");
        assert!(ResponsesUsageSseTransform::new("", None, None).is_err());
    }

    #[test]
    fn messages_inclusive_normalizer_splits_cache_and_keeps_order() {
        let (seen, sink) = recorder();
        let mut normalizer = UsageNormalizer::new(INCLUSIVE_INPUT_PROFILE, Some(sink)).unwrap();
        let start = json!({ "type": "message_start", "message": { "id": "m1", "usage": { "input_tokens": 50, "cache_read_input_tokens": 20, "output_tokens": 1 } } });
        let next = normalizer.apply(&start).unwrap().unwrap();
        assert_eq!(next["message"]["usage"], json!({ "input_tokens": 30, "cache_read_input_tokens": 20, "output_tokens": 1 }));
        let delta = json!({ "type": "message_delta", "usage": { "output_tokens": 9 } });
        assert_eq!(normalizer.apply(&delta).unwrap(), None);
        assert_eq!(normalizer.apply(&json!({ "type": "content_block_delta" })).unwrap(), None);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].message_id, Some(json!("m1")));
        assert_eq!(seen[0].total_input_tokens, 50);
        let mut normalizer = UsageNormalizer::new(INCLUSIVE_INPUT_PROFILE, None).unwrap();
        let bad = json!({ "type": "message", "usage": { "input_tokens": 1, "cache_read_input_tokens": 2 } });
        assert_eq!(normalizer.apply(&bad).unwrap_err(), "Inconsistent inclusive input/cache counts");
        let write = json!({ "type": "message", "usage": { "input_tokens": 1, "cache_creation_input_tokens": 2 } });
        assert_eq!(normalizer.apply(&write).unwrap_err(), "Unverified local cache-creation semantics");
        assert!(UsageNormalizer::new("raw", None).is_err());
    }

    #[test]
    fn llama_messages_stream_is_exclusive_and_requires_stop() {
        let (seen, sink) = recorder();
        let mut transform = UsageSseTransform::new(LLAMA_MESSAGES_PROFILE, Some(sink), None).unwrap();
        let start = "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":3}}}\r\n\r\n";
        let stop = "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        let text = collect(&mut transform, &[start.as_bytes(), stop.as_bytes()]).unwrap();
        assert_eq!(
            text,
            format!("event: message_start\r\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"usage\":{{\"input_tokens\":7,\"cache_read_input_tokens\":3}}}}}}\r\n\r\n{stop}")
        );
        assert_eq!(seen.lock().unwrap()[0].total_input_tokens, 10);
        let mut transform = UsageSseTransform::new(LLAMA_MESSAGES_PROFILE, None, None).unwrap();
        assert_eq!(collect(&mut transform, &[start.as_bytes()]).unwrap_err(), "Incomplete local Messages stream");
        let mut transform = UsageSseTransform::new(INCLUSIVE_INPUT_PROFILE, None, None).unwrap();
        let inclusive = collect(&mut transform, &[start.as_bytes()]).unwrap();
        assert!(inclusive.contains("\"input_tokens\":4"));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut transform = UsageSseTransform::new(INCLUSIVE_INPUT_PROFILE, None, None).unwrap();
        let big = vec![b'a'; MAX_SSE_FRAME + 1];
        assert_eq!(transform.push(&big).unwrap_err(), "Oversized SSE frame");
    }
}
