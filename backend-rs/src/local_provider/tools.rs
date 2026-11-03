// 로컬 Responses 요청의 도구 목록을 함수 도구로 펼치고 응답에서 Codex 원래 이름으로 되돌린다. Node local-responses-tools.mjs와 같다.
// 다리는 도구를 실행하지 않고 권한 확인도 바꾸지 않는다.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::LocalError;
use super::usage::{SseFramer, StreamTransform, rewrite_frame, split_lines};
use crate::js::{is_js_whitespace, js_string, js_truthy, parse_js_json, to_js_json};

const SEARCH_NAME: &str = "officestra_client_tool_search";

#[derive(Debug, Clone, Copy, Default)]
pub struct ToolOptions {
    pub image_tool_results_as_messages: bool,
    pub reasoning_content_as_summary: bool,
}

/// 응답·사건 복원에 필요한 별칭 표. 흐름 변환이 따로 들고 가도록 공유한다.
#[derive(Debug, Default)]
pub struct ToolRestorer {
    /// 별칭 → (namespace, name).
    aliases: HashMap<String, (Value, Value)>,
    /// 별칭 → 원래 custom 도구의 name·namespace 필드.
    custom_aliases: HashMap<String, Map<String, Value>>,
    has_search: bool,
    reasoning_content_as_summary: bool,
}

pub struct AdaptedTools {
    pub request: Value,
    pub restorer: Arc<ToolRestorer>,
}

impl AdaptedTools {
    pub fn restore_response(&self, response: &Value) -> Result<Value, LocalError> {
        self.restorer.restore_response(response)
    }

    pub fn stream(&self) -> JsonSseTransform {
        JsonSseTransform { framer: SseFramer::new("Oversized tool SSE frame"), restorer: Arc::clone(&self.restorer) }
    }
}

fn js_json(value: &Value) -> String {
    String::from_utf8(to_js_json(value).unwrap_or_default()).unwrap_or_default()
}

fn hash32(value: &Value) -> String {
    Sha256::digest(js_json(value).as_bytes()).iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

/// 템플릿 문자열 `${value}`. 없는 값은 "undefined"다.
fn template(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), js_string)
}

/// `${value ?? ''}`.
fn template_or_empty(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => js_string(value),
    }
}

/// JSON.stringify의 배열 요소처럼 없는 값은 null이다.
fn or_null(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

fn kind(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

fn is_client(value: &Value) -> bool {
    value.get("execution").and_then(Value::as_str) == Some("client")
}

fn null_property(property: &str) -> LocalError {
    LocalError::plain(format!("Cannot read properties of null (reading '{property}')"))
}

/// `[...(value ?? [])]`. 배열이 아니면 Node처럼 펼치지 못한다.
fn iterable<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a [Value], LocalError> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(LocalError::plain(format!("{name} is not iterable"))),
    }
}

/// JSON.parse 실패는 Node SyntaxError처럼 상태 없는 오류다.
fn parse_arguments(text: &str) -> Result<Value, LocalError> {
    parse_js_json(text).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))
}

struct Builder {
    restorer: ToolRestorer,
}

impl Builder {
    fn register(&mut self, namespace: &Value, name: &Value) -> String {
        let alias = format!("office_ns_{}", hash32(&json!([namespace, name])));
        self.restorer.aliases.insert(alias.clone(), (namespace.clone(), name.clone()));
        alias
    }

    fn custom_function(&mut self, tool: &Value, namespace: Option<&Value>) -> Value {
        let tool_name = tool.get("name");
        let name = format!("office_custom_{}", hash32(&json!([or_null(namespace), or_null(tool_name)])));
        let mut original = Map::new();
        if let Some(tool_name) = tool_name {
            original.insert("name".into(), tool_name.clone());
        }
        let namespaced = namespace.filter(|namespace| js_truthy(Some(namespace)));
        if let Some(namespace) = namespaced {
            original.insert("namespace".into(), namespace.clone());
        }
        self.restorer.custom_aliases.insert(name.clone(), original);
        let prefix = namespaced.map(|namespace| format!("{}.", js_string(namespace))).unwrap_or_default();
        json!({
            "type": "function",
            "name": name,
            "description": format!("{prefix}{}\n{}\nPass the exact free-form tool input in input.", template(tool_name), template_or_empty(tool.get("description"))),
            "parameters": { "type": "object", "properties": { "input": { "type": "string" } }, "required": ["input"], "additionalProperties": false },
        })
    }

    fn expand(&mut self, tool: &Value) -> Result<Vec<Value>, LocalError> {
        if kind(tool) == Some("tool_search") && is_client(tool) {
            let mut function = Map::new();
            function.insert("type".into(), json!("function"));
            function.insert("name".into(), json!(SEARCH_NAME));
            // undefined 값은 JSON.stringify에서 빠진다.
            if let Some(description) = tool.get("description") {
                function.insert("description".into(), description.clone());
            }
            if let Some(parameters) = tool.get("parameters") {
                function.insert("parameters".into(), parameters.clone());
            }
            return Ok(vec![Value::Object(function)]);
        }
        if kind(tool) == Some("custom") {
            return Ok(vec![self.custom_function(tool, None)]);
        }
        if kind(tool) != Some("namespace") {
            return Ok(vec![tool.clone()]);
        }
        let (Some(namespace), Some(children)) = (tool.get("name").filter(|name| name.is_string()), tool.get("tools").and_then(Value::as_array)) else {
            return Err(LocalError::plain("Malformed tool namespace"));
        };
        let namespace_text = js_string(namespace);
        children
            .iter()
            .map(|child| {
                if child.is_null() {
                    return Err(null_property("type"));
                }
                if kind(child) == Some("custom") {
                    return Ok(self.custom_function(child, Some(namespace)));
                }
                let child_name = child.get("name").filter(|name| name.is_string());
                let (Some("function"), Some(child_name)) = (kind(child), child_name) else {
                    return Err(LocalError::plain("Unsupported namespaced local tool"));
                };
                let alias = self.register(namespace, child_name);
                let mut function = child.as_object().cloned().unwrap_or_default();
                function.insert("name".into(), json!(alias));
                function.insert(
                    "description".into(),
                    json!(format!(
                        "{namespace_text}.{}\n{}\n{}",
                        js_string(child_name),
                        template_or_empty(tool.get("description")),
                        template_or_empty(child.get("description"))
                    )),
                );
                Ok(Value::Object(function))
            })
            .collect()
    }

    fn map_input(&mut self, item: &Value) -> Result<Value, LocalError> {
        match kind(item) {
            Some("custom_tool_call") => {
                let name = self.custom_function(item, item.get("namespace")).get("name").cloned().unwrap_or_default();
                let mut call = Map::new();
                call.insert("type".into(), json!("function_call"));
                if let Some(call_id) = item.get("call_id") {
                    call.insert("call_id".into(), call_id.clone());
                }
                call.insert("name".into(), name);
                let mut arguments = Map::new();
                if let Some(input) = item.get("input") {
                    arguments.insert("input".into(), input.clone());
                }
                call.insert("arguments".into(), json!(js_json(&Value::Object(arguments))));
                Ok(Value::Object(call))
            }
            Some("custom_tool_call_output") => {
                let mut output = item.as_object().cloned().unwrap_or_default();
                output.insert("type".into(), json!("function_call_output"));
                Ok(Value::Object(output))
            }
            Some("tool_search_call") if is_client(item) => {
                let mut call = Map::new();
                call.insert("type".into(), json!("function_call"));
                if let Some(call_id) = item.get("call_id") {
                    call.insert("call_id".into(), call_id.clone());
                }
                call.insert("name".into(), json!(SEARCH_NAME));
                let arguments = item.get("arguments").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!({}));
                call.insert("arguments".into(), json!(js_json(&arguments)));
                Ok(Value::Object(call))
            }
            Some("tool_search_output") if is_client(item) => {
                let mut loaded = Vec::new();
                for tool in iterable(item.get("tools"), "item.tools")? {
                    if tool.is_null() {
                        return Err(null_property("type"));
                    }
                    if kind(tool) == Some("namespace") {
                        for child in iterable(tool.get("tools"), "t.tools")? {
                            if child.is_null() {
                                return Err(null_property("name"));
                            }
                            loaded.push(json!(format!("{}.{}", template(tool.get("name")), template(child.get("name")))));
                        }
                    } else {
                        loaded.push(or_null(tool.get("name")));
                    }
                }
                let mut output = Map::new();
                output.insert("type".into(), json!("function_call_output"));
                if let Some(call_id) = item.get("call_id") {
                    output.insert("call_id".into(), call_id.clone());
                }
                let body = json!({ "loaded_tools": loaded, "note": "The current callable schemas are included in the tools list." });
                output.insert("output".into(), json!(js_json(&body)));
                Ok(Value::Object(output))
            }
            Some("function_call") if js_truthy(item.get("namespace")) => {
                let alias = self.register(&item["namespace"], &or_null(item.get("name")));
                let mut call = item.as_object().cloned().unwrap_or_default();
                call.shift_remove("namespace");
                call.insert("name".into(), json!(alias));
                Ok(Value::Object(call))
            }
            _ => Ok(item.clone()),
        }
    }
}

/// adaptLocalResponsesTools.
pub fn adapt_local_responses_tools(value: &Value, options: ToolOptions) -> Result<AdaptedTools, LocalError> {
    let mut builder = Builder {
        restorer: ToolRestorer { reasoning_content_as_summary: options.reasoning_content_as_summary, ..ToolRestorer::default() },
    };
    let history: &[Value] = value.get("input").and_then(Value::as_array).map_or(&[], Vec::as_slice);
    // Codex는 찾은 연결 도구 스키마를 tools가 아니라 기록에 돌려준다. 새 결과가 옛 결과를, 현재 요청이 모두를 덮는다.
    let mut declared = Vec::new();
    for item in history {
        if kind(item) == Some("tool_search_output") && is_client(item) {
            declared.extend(iterable(item.get("tools"), "item.tools")?.iter().cloned());
        }
    }
    declared.extend(iterable(value.get("tools"), "value.tools")?.iter().cloned());
    builder.restorer.has_search = declared.iter().any(|tool| kind(tool) == Some("tool_search") && is_client(tool));
    let mut tools = Vec::new();
    for tool in &declared {
        tools.extend(builder.expand(tool)?);
    }
    let input = match value.get("input") {
        Some(Value::Array(items)) => {
            let mapped = items.iter().map(|item| builder.map_input(item)).collect::<Result<Vec<_>, _>>()?;
            Some(Value::Array(if options.image_tool_results_as_messages { lift_tool_images(mapped)? } else { mapped }))
        }
        other => other.cloned(),
    };
    // 기록 재생도 별칭을 등록한다. 재생 뒤에 검사해야 현재 일반 함수를 옛 custom·MCP 호출로 오인하지 않는다.
    let restorer = &builder.restorer;
    let search_used = restorer.has_search || history.iter().any(|item| kind(item) == Some("tool_search_call") && is_client(item));
    let reserved = |name: Option<&str>| {
        name.is_some_and(|name| restorer.aliases.contains_key(name) || restorer.custom_aliases.contains_key(name) || (search_used && name == SEARCH_NAME))
    };
    if declared.iter().any(|tool| kind(tool) == Some("function") && reserved(tool.get("name").and_then(Value::as_str))) {
        return Err(LocalError::plain("Local tool alias collision"));
    }
    if history
        .iter()
        .any(|item| kind(item) == Some("function_call") && !js_truthy(item.get("namespace")) && reserved(item.get("name").and_then(Value::as_str)))
    {
        return Err(LocalError::plain("Historical local tool alias collision"));
    }
    let mut request = value.as_object().cloned().unwrap_or_default();
    if !declared.is_empty() {
        request.insert("tools".into(), Value::Array(unique_tools(tools)?));
    }
    if let Some(input) = input {
        request.insert("input".into(), input);
    }
    Ok(AdaptedTools { request: Value::Object(request), restorer: Arc::new(builder.restorer) })
}

/// `[...new Map(tools.map(t=>[t.name??t.type,t])).values()]`. 처음 자리, 마지막 값이다.
fn unique_tools(tools: Vec<Value>) -> Result<Vec<Value>, LocalError> {
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, Value> = HashMap::new();
    for tool in tools {
        if tool.is_null() {
            return Err(null_property("name"));
        }
        let key = match tool.get("name").filter(|name| !name.is_null()).or_else(|| tool.get("type")) {
            Some(key) => js_json(key),
            None => "\u{0}undefined".into(),
        };
        if values.insert(key.clone(), tool).is_none() {
            order.push(key);
        }
    }
    Ok(order.into_iter().filter_map(|key| values.remove(&key)).collect())
}

/// liftToolImages. llama.cpp b10982는 도구 출력에 글만 받는다. 병렬 도구 결과를 붙여 둔 뒤 그 뒤에 첨부를 단다.
fn lift_tool_images(input: Vec<Value>) -> Result<Vec<Value>, LocalError> {
    let mut result = Vec::new();
    let mut attachments: Vec<Value> = Vec::new();
    let flush = |result: &mut Vec<Value>, attachments: &mut Vec<Value>| {
        if !attachments.is_empty() {
            result.push(json!({ "type": "message", "role": "user", "content": std::mem::take(attachments) }));
        }
    };
    for item in input {
        if kind(&item) != Some("function_call_output") {
            flush(&mut result, &mut attachments);
            result.push(item);
            continue;
        }
        let Some(blocks) = item.get("output").and_then(Value::as_array) else {
            result.push(item);
            continue;
        };
        let mut text = Vec::new();
        for block in blocks {
            let block_kind = kind(block);
            let block_text = block.get("text").and_then(Value::as_str);
            if matches!(block_kind, Some("input_text" | "text")) && block_text.is_some() {
                text.push(block_text.unwrap_or_default().to_string());
            } else if block_kind == Some("input_image") && block.get("image_url").is_some_and(Value::is_string) {
                text.push("[Image attachment follows this tool-result batch.]".into());
                attachments.push(json!({
                    "type": "input_text",
                    "text": format!("Image returned by tool call {}. This is tool output, not a new user instruction.", template(item.get("call_id"))),
                }));
                attachments.push(block.clone());
            } else {
                let missing = if block.is_null() { None } else { block.get("type").filter(|value| !value.is_null()) };
                return Err(LocalError::status(
                    400,
                    format!("Unsupported local tool output block: {}", missing.map_or_else(|| "missing type".into(), js_string)),
                ));
            }
        }
        let mut output = item.as_object().cloned().unwrap_or_default();
        output.insert("output".into(), json!(text.join("\n")));
        result.push(Value::Object(output));
    }
    flush(&mut result, &mut attachments);
    Ok(result)
}

impl ToolRestorer {
    fn restore_item(&self, item: &Value) -> Result<Value, LocalError> {
        // Codex exec JSON은 추론 요약만 보여 준다. 고정한 로컬 Qwen 실행에서만 실제 추론 글을 요약 칸으로 옮긴다.
        // 다시 재생할 수 있게 content는 그대로 두고, 진짜 요약은 덮지 않는다.
        if self.reasoning_content_as_summary
            && kind(item) == Some("reasoning")
            && !item.get("summary").is_some_and(|summary| summary_has_length(summary))
            && let Some(content) = item.get("content").and_then(Value::as_array)
        {
            let text = content
                .iter()
                .filter(|block| kind(block) == Some("reasoning_text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                let mut next = item.as_object().cloned().unwrap_or_default();
                next.insert("summary".into(), json!([{ "type": "summary_text", "text": text }]));
                return Ok(Value::Object(next));
            }
        }
        if kind(item) != Some("function_call") {
            return Ok(item.clone());
        }
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let has_name = item.get("name").is_some_and(Value::is_string);
        if has_name && let Some(original) = self.custom_aliases.get(name) {
            let arguments = match item.get("arguments").filter(|value| js_truthy(Some(value))) {
                Some(arguments) => parse_arguments(&js_string(arguments))?,
                None => json!({ "input": "" }),
            };
            if arguments.is_null() {
                return Err(null_property("input"));
            }
            let Some(input) = arguments.get("input").and_then(Value::as_str) else {
                return Err(LocalError::plain("Invalid custom tool input"));
            };
            let mut call = Map::new();
            call.insert("type".into(), json!("custom_tool_call"));
            if let Some(id) = item.get("id").filter(|id| js_truthy(Some(id))) {
                call.insert("id".into(), id.clone());
            }
            if let Some(call_id) = item.get("call_id") {
                call.insert("call_id".into(), call_id.clone());
            }
            call.extend(original.clone());
            call.insert("input".into(), json!(input));
            if let Some(status) = item.get("status").filter(|status| js_truthy(Some(status))) {
                call.insert("status".into(), status.clone());
            }
            return Ok(Value::Object(call));
        }
        if self.has_search && has_name && name == SEARCH_NAME {
            let mut call = Map::new();
            call.insert("type".into(), json!("tool_search_call"));
            if let Some(id) = item.get("id").filter(|id| js_truthy(Some(id))) {
                call.insert("id".into(), id.clone());
            }
            call.insert("execution".into(), json!("client"));
            if let Some(call_id) = item.get("call_id") {
                call.insert("call_id".into(), call_id.clone());
            }
            call.insert("status".into(), item.get("status").filter(|status| !status.is_null()).cloned().unwrap_or_else(|| json!("completed")));
            let arguments = match item.get("arguments").filter(|value| js_truthy(Some(value))) {
                Some(arguments) => parse_arguments(&js_string(arguments))?,
                None => json!({}),
            };
            call.insert("arguments".into(), arguments);
            return Ok(Value::Object(call));
        }
        let Some((namespace, original)) = has_name.then(|| self.aliases.get(name)).flatten() else {
            return Ok(item.clone());
        };
        let mut call = item.as_object().cloned().unwrap_or_default();
        call.insert("namespace".into(), namespace.clone());
        call.insert("name".into(), original.clone());
        Ok(Value::Object(call))
    }

    /// restoreResponse. output이 배열일 때만 바꾼다.
    pub fn restore_response(&self, response: &Value) -> Result<Value, LocalError> {
        let Some(output) = response.get("output").and_then(Value::as_array) else {
            return Ok(response.clone());
        };
        let restored = output.iter().map(|item| self.restore_item(item)).collect::<Result<Vec<_>, _>>()?;
        let mut next = response.as_object().cloned().unwrap_or_default();
        next.insert("output".into(), Value::Array(restored));
        Ok(Value::Object(next))
    }

    /// restoreEvent. item이나 response가 있으면 언제나 새 사건이라 묶음을 다시 쓴다.
    fn restore_event(&self, event: &Value) -> Result<Option<Value>, LocalError> {
        if let Some(item) = event.get("item").filter(|item| js_truthy(Some(item))) {
            let mut next = event.as_object().cloned().unwrap_or_default();
            next.insert("item".into(), self.restore_item(item)?);
            return Ok(Some(Value::Object(next)));
        }
        if let Some(response) = event.get("response").filter(|response| js_truthy(Some(response))) {
            let mut next = event.as_object().cloned().unwrap_or_default();
            next.insert("response".into(), self.restore_response(response)?);
            return Ok(Some(Value::Object(next)));
        }
        Ok(None)
    }
}

/// `item.summary?.length`가 참인지. 문자열·배열 길이가 0이 아니면 참이다.
fn summary_has_length(summary: &Value) -> bool {
    match summary {
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        Value::Object(fields) => js_truthy(fields.get("length")),
        _ => false,
    }
}

/// jsonSSETransform. data 줄 앞 공백은 모두 지우고 읽는다.
pub struct JsonSseTransform {
    framer: SseFramer,
    restorer: Arc<ToolRestorer>,
}

impl JsonSseTransform {
    fn frame(&self, raw: &str) -> Result<String, String> {
        let data: Vec<&str> = split_lines(raw).filter(|line| line.starts_with("data:")).collect();
        if data.is_empty() {
            return Ok(raw.to_string());
        }
        let joined = data.iter().map(|line| line[5..].trim_start_matches(is_js_whitespace)).collect::<Vec<_>>().join("\n");
        let Ok(event) = parse_js_json(&joined) else {
            return Ok(raw.to_string());
        };
        match self.restorer.restore_event(&event).map_err(|error| error.message)? {
            Some(mapped) => Ok(rewrite_frame(raw, &mapped)),
            None => Ok(raw.to_string()),
        }
    }
}

impl StreamTransform for JsonSseTransform {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::replace(&mut self.framer, SseFramer::new("Oversized tool SSE frame"));
        let result = framer.push(chunk, &mut |raw| self.frame(raw));
        self.framer = framer;
        result
    }

    fn finish(&mut self) -> Result<Vec<u8>, String> {
        let mut framer = std::mem::replace(&mut self.framer, SseFramer::new("Oversized tool SSE frame"));
        let result = framer.finish(&mut |raw| self.frame(raw));
        self.framer = framer;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace_tool() -> Value {
        json!({ "type": "namespace", "name": "mcp__cua_repl", "description": "Computer tool documentation",
            "tools": [{ "type": "function", "name": "js", "description": "Read state first", "parameters": { "type": "object", "properties": { "code": { "type": "string" } } } }] })
    }

    fn adapt(value: Value) -> AdaptedTools {
        adapt_local_responses_tools(&value, ToolOptions::default()).unwrap()
    }

    fn through_stream(adapter: &AdaptedTools, wire: &str) -> String {
        let mut transform = adapter.stream();
        let mut output = Vec::new();
        for byte in wire.as_bytes() {
            output.extend(transform.push(&[*byte]).unwrap());
        }
        output.extend(transform.finish().unwrap());
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn reasoning_summary_is_opt_in_and_never_replaces_supplied_summary() {
        let item = json!({ "id": "r1", "type": "reasoning", "summary": [], "content": [{ "type": "reasoning_text", "text": "First" }, { "type": "reasoning_text", "text": "Second" }] });
        let response = json!({ "output": [item], "usage": { "input_tokens": 11, "output_tokens": 5 } });
        assert_eq!(adapt(json!({})).restore_response(&response).unwrap()["output"][0], item);
        let adapter = adapt_local_responses_tools(&json!({}), ToolOptions { reasoning_content_as_summary: true, ..ToolOptions::default() }).unwrap();
        let mapped = adapter.restore_response(&response).unwrap();
        assert_eq!(mapped["output"][0]["summary"], json!([{ "type": "summary_text", "text": "First\nSecond" }]));
        assert_eq!(mapped["output"][0]["content"], item["content"]);
        assert_eq!(mapped["usage"], response["usage"]);
        assert_eq!(adapter.restore_response(&mapped).unwrap(), mapped);
        let mut existing = item.clone();
        existing["summary"] = json!([{ "type": "summary_text", "text": "Existing" }]);
        let mut empty = item.clone();
        empty["content"] = json!([]);
        let mut other = item.clone();
        other["content"] = json!([{ "type": "other", "text": "not reasoning" }]);
        for original in [existing, empty, other] {
            assert_eq!(adapter.restore_response(&json!({ "output": [original] })).unwrap()["output"][0], original);
        }
    }

    #[test]
    fn reasoning_stream_done_and_completion_share_one_item() {
        let adapter = adapt_local_responses_tools(&json!({}), ToolOptions { reasoning_content_as_summary: true, ..ToolOptions::default() }).unwrap();
        let item = json!({ "id": "r1", "type": "reasoning", "summary": [], "content": [{ "type": "reasoning_text", "text": "Visible local reasoning" }] });
        let usage = json!({ "input_tokens": 11, "output_tokens": 5 });
        let events = [json!({ "type": "response.output_item.done", "item": item }), json!({ "type": "response.completed", "response": { "output": [item], "usage": usage } })];
        let wire: String = events.iter().map(|event| format!("data: {}\n\n", js_json(event))).collect();
        let output = through_stream(&adapter, &wire);
        let mapped: Vec<Value> = output.trim().split("\n\n").map(|frame| serde_json::from_str(&frame[6..]).unwrap()).collect();
        assert_eq!(mapped[0]["item"]["summary"][0]["text"], "Visible local reasoning");
        assert_eq!(mapped[0]["item"], mapped[1]["response"]["output"][0]);
        assert_eq!(mapped[1]["response"]["usage"], usage);
    }

    #[test]
    fn llama_image_tool_output_replay_is_idempotent() {
        let image = json!({ "type": "input_image", "image_url": "data:image/png;base64,AA==", "detail": "original" });
        let value = json!({ "input": [
            { "type": "function_call", "name": "view_image", "call_id": "view1", "arguments": "{}" },
            { "type": "function_call_output", "call_id": "view1", "output": [{ "type": "input_text", "text": "screenshot" }, image] },
            { "type": "message", "role": "user", "content": "continue" },
        ] });
        let lifted = ToolOptions { image_tool_results_as_messages: true, ..ToolOptions::default() };
        let input = adapt_local_responses_tools(&value, lifted).unwrap().request["input"].clone();
        assert_eq!(input[1]["call_id"], "view1");
        assert!(input[1]["output"].as_str().unwrap().contains("screenshot"));
        assert_eq!(input[2]["role"], "user");
        assert!(input[2]["content"][0]["text"].as_str().unwrap().contains("tool call view1"));
        assert!(input[2]["content"][0]["text"].as_str().unwrap().contains("not a new user instruction"));
        assert_eq!(input[2]["content"][1], image);
        assert_eq!(input[3], value["input"][2]);
        assert_eq!(adapt(value.clone()).request["input"], value["input"]);
        assert_eq!(adapt_local_responses_tools(&json!({ "input": input }), lifted).unwrap().request["input"], input);
    }

    #[test]
    fn parallel_image_and_custom_results_stay_together() {
        let image = |n: u32| json!({ "type": "input_image", "image_url": format!("https://example.invalid/{n}.png") });
        let value = json!({ "input": [
            { "type": "function_call_output", "call_id": "a", "output": [image(1), image(2)] },
            { "type": "custom_tool_call_output", "call_id": "b", "output": [{ "type": "text", "text": "second" }, image(3)] },
            { "type": "function_call_output", "call_id": "c", "output": "plain" },
        ] });
        let input = adapt_local_responses_tools(&value, ToolOptions { image_tool_results_as_messages: true, ..ToolOptions::default() }).unwrap().request["input"].clone();
        let input = input.as_array().unwrap();
        let heads: Vec<(Value, Value)> = input[..3].iter().map(|item| (item["type"].clone(), item["call_id"].clone())).collect();
        assert_eq!(heads, [(json!("function_call_output"), json!("a")), (json!("function_call_output"), json!("b")), (json!("function_call_output"), json!("c"))]);
        assert_eq!(input.len(), 4);
        let content = input[3]["content"].as_array().unwrap();
        assert_eq!(content.len(), 6);
        assert_eq!(content.iter().filter(|block| block["type"] == "input_image").cloned().collect::<Vec<_>>(), [image(1), image(2), image(3)]);
        assert!(content[4]["text"].as_str().unwrap().contains("tool call b"));
    }

    #[test]
    fn text_only_empty_and_unsupported_tool_outputs() {
        let lifted = ToolOptions { image_tool_results_as_messages: true, ..ToolOptions::default() };
        let input = json!([{ "type": "function_call_output", "call_id": "a", "output": [{ "type": "input_text", "text": "one" }, { "type": "text", "text": "two" }] },
            { "type": "function_call_output", "call_id": "b", "output": [] }]);
        let adapted = adapt_local_responses_tools(&json!({ "input": input }), lifted).unwrap();
        let outputs: Vec<Value> = adapted.request["input"].as_array().unwrap().iter().map(|item| item["output"].clone()).collect();
        assert_eq!(outputs, [json!("one\ntwo"), json!("")]);
        for block in [json!({ "type": "input_audio" }), json!({ "type": "input_image" }), json!({})] {
            let value = json!({ "input": [{ "type": "function_call_output", "call_id": "a", "output": [block] }] });
            let error = adapt_local_responses_tools(&value, lifted).err().unwrap();
            assert_eq!(error.status, Some(400));
            assert!(error.message.contains("Unsupported local tool output block"));
        }
    }

    #[test]
    fn namespace_alias_is_stable_and_identity_restored() {
        let tool = namespace_tool();
        let source = json!({ "tools": [tool], "input": [{ "type": "message", "role": "user", "content": "hi" }] });
        let adapter = adapt(source.clone());
        let function = &adapter.request["tools"][0];
        assert_eq!(function["type"], "function");
        assert!(function["name"].as_str().unwrap().starts_with("office_ns_"));
        assert_eq!(function["parameters"], tool["tools"][0]["parameters"]);
        assert!(function["description"].as_str().unwrap().contains("Read state first"));
        let alias = function["name"].clone();
        let response = json!({ "output": [{ "type": "function_call", "id": "fc1", "call_id": "c1", "name": alias, "arguments": "{\"code\":\"await cua.getState()\"}" }] });
        let restored = adapter.restore_response(&response).unwrap();
        assert_eq!(restored["output"][0]["name"], "js");
        assert_eq!(restored["output"][0]["namespace"], tool["name"]);
        assert_eq!(restored["output"][0]["call_id"], "c1");
        let resume = adapt(json!({ "tools": [tool], "input": [restored["output"][0], { "type": "function_call_output", "call_id": "c1", "output": "result" }] }));
        assert_eq!(resume.request["input"][0]["name"], alias);
        assert_eq!(resume.request["input"][0].get("namespace"), None);
        assert_eq!(resume.request["input"][1]["output"], "result");
    }

    #[test]
    fn namespace_stream_restores_items_with_crlf() {
        let tool = namespace_tool();
        let adapter = adapt(json!({ "tools": [tool] }));
        let alias = adapter.request["tools"][0]["name"].clone();
        let item = json!({ "type": "function_call", "name": alias, "call_id": "c1", "arguments": "{\"code\":\"한글\"}" });
        let events = [
            json!({ "type": "response.output_item.added", "item": item }),
            json!({ "type": "response.output_item.done", "item": item }),
            json!({ "type": "response.completed", "response": { "output": [item], "usage": { "input_tokens": 10, "output_tokens": 2 } } }),
        ];
        let wire: String = events.iter().map(|event| format!("data: {}\r\n\r\n", js_json(event))).collect();
        let output = through_stream(&adapter, &wire);
        let mapped: Vec<Value> = output.trim().split("\r\n\r\n").map(|frame| serde_json::from_str(&frame[6..]).unwrap()).collect();
        assert_eq!(mapped[0]["item"]["namespace"], tool["name"]);
        assert_eq!(mapped[1]["item"]["arguments"], item["arguments"]);
        assert_eq!(mapped[2]["response"]["usage"], json!({ "input_tokens": 10, "output_tokens": 2 }));
    }

    #[test]
    fn ordinary_names_are_not_reinterpreted_and_collisions_rejected() {
        let tool = namespace_tool();
        let adapter = adapt(json!({ "tools": [tool] }));
        let alias = adapter.request["tools"][0]["name"].clone();
        let error = adapt_local_responses_tools(&json!({ "tools": [tool, { "type": "function", "name": alias }] }), ToolOptions::default()).err().unwrap();
        assert!(error.message.contains("collision"));
        let item = json!({ "type": "function_call", "name": "not-advertised", "arguments": "{}" });
        assert_eq!(adapter.restore_response(&json!({ "output": [item] })).unwrap()["output"][0], item);
    }

    #[test]
    fn client_tool_search_round_trip() {
        let search = json!({ "type": "tool_search", "execution": "client", "description": "Search available connectors",
            "parameters": { "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] } });
        let tool = namespace_tool();
        let adapter = adapt(json!({ "tools": [search] }));
        let function = adapter.request["tools"][0].clone();
        assert_eq!(function["type"], "function");
        assert_eq!(function["parameters"], search["parameters"]);
        let call = adapter
            .restore_response(&json!({ "output": [{ "type": "function_call", "name": function["name"], "call_id": "search1", "arguments": "{\"query\":\"Google Sheets\"}", "status": "completed" }] }))
            .unwrap()["output"][0]
            .clone();
        assert_eq!(call["type"], "tool_search_call");
        assert_eq!(call["execution"], "client");
        assert_eq!(call["arguments"], json!({ "query": "Google Sheets" }));
        let next = adapt(json!({ "tools": [search, tool], "input": [call, { "type": "tool_search_output", "execution": "client", "call_id": "search1", "tools": [tool] }] }));
        assert_eq!(next.request["input"][0]["type"], "function_call");
        assert_eq!(next.request["input"][1]["type"], "function_call_output");
        assert_eq!(next.request["input"][1]["call_id"], "search1");
        assert_eq!(next.request["tools"].as_array().unwrap().len(), 2);
        let alias = next.request["tools"].as_array().unwrap().iter().find(|tool| tool["name"].as_str().unwrap().starts_with("office_ns_")).unwrap()["name"].clone();
        let restored = next.restore_response(&json!({ "output": [{ "type": "function_call", "name": alias, "call_id": "c2", "arguments": "{}" }] })).unwrap();
        assert_eq!(restored["output"][0]["namespace"], tool["name"]);
        let broken = adapter.restore_response(&json!({ "output": [{ "type": "function_call", "name": function["name"], "arguments": "broken" }] }));
        assert!(broken.err().unwrap().message.starts_with("SyntaxError"));
    }

    #[test]
    fn freeform_apply_patch_round_trip() {
        let custom = json!({ "type": "custom", "name": "apply_patch", "description": "Patch files" });
        let adapter = adapt(json!({ "tools": [custom] }));
        let alias = adapter.request["tools"][0]["name"].clone();
        let patch = "*** Begin Patch\n*** Add File: test.txt\n+한글\n*** End Patch";
        let call = adapter
            .restore_response(&json!({ "output": [{ "type": "function_call", "name": alias, "call_id": "patch1", "arguments": js_json(&json!({ "input": patch })) }] }))
            .unwrap()["output"][0]
            .clone();
        assert_eq!(call["type"], "custom_tool_call");
        assert_eq!(call["input"], patch);
        assert_eq!(call["name"], "apply_patch");
        let next = adapt(json!({ "tools": [custom], "input": [call, { "type": "custom_tool_call_output", "call_id": "patch1", "output": "ok" }] }));
        assert_eq!(next.request["input"][0]["name"], alias);
        assert_eq!(serde_json::from_str::<Value>(next.request["input"][0]["arguments"].as_str().unwrap()).unwrap(), json!({ "input": patch }));
        assert_eq!(next.request["input"][1]["type"], "function_call_output");
    }

    #[test]
    fn historical_aliases_cannot_shadow_current_functions() {
        let custom = json!({ "type": "custom", "name": "apply_patch" });
        for source in [namespace_tool(), custom.clone()] {
            let adapter = adapt(json!({ "tools": [source] }));
            let name = adapter.request["tools"][0]["name"].clone();
            let arguments = if source == custom { "{\"input\":\"patch\"}" } else { "{}" };
            let previous = adapter.restore_response(&json!({ "output": [{ "type": "function_call", "name": name, "call_id": "old", "arguments": arguments }] })).unwrap()["output"][0].clone();
            let shadow = json!({ "tools": [{ "type": "function", "name": name, "parameters": { "type": "object" } }], "input": [previous] });
            assert!(adapt_local_responses_tools(&shadow, ToolOptions::default()).err().unwrap().message.contains("collision"));
            let ordinary = json!({ "tools": [source], "input": [{ "type": "function_call", "name": name, "call_id": "ordinary", "arguments": "{}" }] });
            assert!(adapt_local_responses_tools(&ordinary, ToolOptions::default()).err().unwrap().message.contains("collision"));
        }
    }

    #[test]
    fn current_schemas_override_historical_ones() {
        let tool = namespace_tool();
        let mut old = tool.clone();
        old["tools"] = json!([
            { "type": "function", "name": "js", "description": "old", "parameters": { "type": "object", "properties": { "old": { "type": "string" } } } },
            { "type": "function", "name": "second", "parameters": { "type": "object" } },
        ]);
        let adapter = adapt(json!({ "tools": [tool], "input": [{ "type": "tool_search_output", "execution": "client", "call_id": "s1", "tools": [old] }] }));
        let tools = adapter.request["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let current = tools.iter().find(|tool| tool["description"].as_str().is_some_and(|text| text.contains("Read state first"))).unwrap();
        assert_eq!(current["parameters"], tool["tools"][0]["parameters"]);
        assert!(tools.iter().any(|tool| tool["description"].as_str().is_some_and(|text| text.contains(".second"))));
        let plain = json!({ "type": "function", "name": "read", "parameters": { "type": "object" }, "description": "current" });
        let mut historical = plain.clone();
        historical["description"] = json!("old");
        let adapter = adapt(json!({ "tools": [plain], "input": [{ "type": "tool_search_output", "execution": "client", "call_id": "s1", "tools": [historical] }] }));
        assert_eq!(adapter.request["tools"][0]["description"], "current");
    }
}
