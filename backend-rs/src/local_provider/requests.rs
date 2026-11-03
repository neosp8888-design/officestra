// 로컬 다리로 들어온 Messages·Responses 요청을 고정한 런타임이 받는 모양으로 바꾼다. Node local-inference-bridge.mjs 앞부분과 같다.
// 바뀐 것이 없으면 None을 돌려 다리가 원래 바이트를 그대로 보낸다.

use serde_json::{Map, Value, json};

use super::LocalError;
use super::usage::{LLAMA_RESPONSES_PROFILE, safe_nonnegative_integer};
use crate::js::{js_string, js_truthy};
use crate::local_model_capabilities::{MEROMERO_MODEL_ID, is_mero_mero_model};

pub const TEXT_ONLY_MESSAGE: &str = "31B는 텍스트 전용입니다. 이미지 분석은 G4 MeroMero 26B A4B · 비전 모델을 선택하세요.";
const IMAGE_PLACEHOLDER: &str = "[이전 이미지 생략: 현재 31B 모델은 텍스트 전용입니다.]";

fn kind(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 모델 값. Node `model=value.model`처럼 따로 주지 않으면 요청의 model을 쓴다.
fn model_of<'a>(value: &'a Value, model: Option<&'a str>) -> Option<&'a str> {
    model.or_else(|| value.get("model").and_then(Value::as_str))
}

/// textOnlyMessages. 마지막 user 항목부터의 새 이미지는 거절하고, 그 앞 기록의 이미지는 자리표시 글로 바꾼다.
fn text_only_messages(items: &Value, responses: bool) -> Result<Value, LocalError> {
    let Some(items) = items.as_array() else {
        return Ok(items.clone());
    };
    let last_user = items.iter().rposition(|item| item.get("role").and_then(Value::as_str) == Some("user"));
    fn map(value: &Value, current: bool, responses: bool) -> Result<Value, LocalError> {
        match value {
            Value::Array(values) => Ok(Value::Array(values.iter().map(|value| map(value, current, responses)).collect::<Result<_, _>>()?)),
            Value::Object(fields) => {
                if matches!(fields.get("type").and_then(Value::as_str), Some("image" | "input_image" | "image_url")) {
                    if current {
                        return Err(LocalError::status(400, TEXT_ONLY_MESSAGE));
                    }
                    return Ok(json!({ "type": if responses { "input_text" } else { "text" }, "text": IMAGE_PLACEHOLDER }));
                }
                let mut next = Map::new();
                for (key, value) in fields {
                    let mapped = if key == "content" || key == "output" { map(value, current, responses)? } else { value.clone() };
                    next.insert(key.clone(), mapped);
                }
                Ok(Value::Object(next))
            }
            other => Ok(other.clone()),
        }
    }
    let mapped = items
        .iter()
        .enumerate()
        .map(|(index, item)| map(item, last_user.is_none_or(|last| index >= last), responses))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(mapped))
}

/// `{...value, [key]: textOnlyMessages(value[key])}`. 없는 값은 그대로 없다.
fn with_text_only(value: &Value, key: &str, responses: bool) -> Result<Value, LocalError> {
    let mut next = object(value);
    if let Some(items) = value.get(key) {
        next.insert(key.into(), text_only_messages(items, responses)?);
    }
    Ok(Value::Object(next))
}

/// `value.reasoning?.effort`. 없거나 null이면 None이다.
fn requested_effort(value: &Value) -> Option<Value> {
    value.get("reasoning").and_then(Value::as_object).and_then(|reasoning| reasoning.get("effort")).filter(|effort| !effort.is_null()).cloned()
}

/// normalizeLocalResponsesRequest. 언제나 새 요청이다.
pub fn normalize_local_responses_request(
    value: &Value,
    reasoning: &str,
    usage_profile: &str,
    context_window: Option<u64>,
    model: Option<&str>,
) -> Result<Value, LocalError> {
    let model = model_of(value, model).map(str::to_string);
    let value = if model.as_deref() != Some(MEROMERO_MODEL_ID) { value.clone() } else { with_text_only(value, "input", true)? };
    let direct = usage_profile == LLAMA_RESPONSES_PROFILE;
    let allowed: &[&str] = if direct { &["default", "on", "off", "low", "medium", "xhigh"] } else { &["default", "on", "off"] };
    if !allowed.contains(&reasoning) {
        return Err(LocalError::status(400, "Local Responses reasoning level is not verified"));
    }
    let max_output = value.get("max_output_tokens");
    if let Some(budget) = max_output {
        let valid = safe_nonnegative_integer(budget).is_some_and(|budget| budget >= 1 && context_window.is_none_or(|window| budget <= window));
        if !valid {
            return Err(LocalError::status(400, "Unverified output budget"));
        }
    }
    // 예전 4096 상한을 넣지 않는다. Codex가 예산을 안 주면 EOS나 측정한 문맥 창까지 생성한다.
    // 공유 Codex 설정의 클라우드 effort가 새지 않게 reasoning·reasoning_effort·service_tier를 뺀다.
    let mut request = object(&value);
    request.shift_remove("reasoning");
    request.shift_remove("reasoning_effort");
    request.shift_remove("service_tier");
    // Codex 재개는 앞 턴 사이에 developer·환경 갱신을 붙인다. Qwen 템플릿은 맨 앞 system 글만 받으므로
    // 실제 system/developer 메시지만 instructions로 옮기고 역할 표시와 갱신 순서를 남긴다.
    if let Some(input) = request.get("input").and_then(Value::as_array).cloned() {
        let is_lifted =
            |item: &Value| kind(item) == Some("message") && matches!(item.get("role").and_then(Value::as_str), Some("system" | "developer"));
        let lifted: Vec<&Value> = input.iter().filter(|item| is_lifted(item)).collect();
        if !lifted.is_empty() {
            if let Some(instructions) = request.get("instructions")
                && !instructions.is_null()
                && !instructions.is_string()
            {
                return Err(LocalError::status(400, "Unsupported local instructions"));
            }
            let mut parts = vec![request.get("instructions").and_then(Value::as_str).unwrap_or_default().to_string()];
            for (index, item) in lifted.iter().enumerate() {
                let role = item.get("role").and_then(Value::as_str).unwrap_or_default().to_uppercase();
                parts.push(format!(
                    "{role} INSTRUCTIONS (update {}; later updates take precedence within the same role):\n{}",
                    index + 1,
                    instruction_text(item)?
                ));
            }
            let joined = parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
            request.insert("instructions".into(), json!(joined));
            request.insert("input".into(), Value::Array(input.iter().filter(|item| !is_lifted(item)).cloned().collect()));
        }
    }
    let mut effort: Option<Value> = (reasoning == "off").then(|| json!("none"));
    if direct && model.as_deref().is_some_and(is_mero_mero_model) {
        if !["default", "on", "off"].contains(&reasoning) {
            return Err(LocalError::status(400, "MeroMero supports thinking on/off only"));
        }
        let effort = requested_effort(&value).unwrap_or_else(|| json!(if reasoning == "on" { "medium" } else { "none" }));
        let Some(effort) = effort.as_str().filter(|effort| matches!(*effort, "none" | "medium")) else {
            return Err(LocalError::status(400, "MeroMero supports none (off) or medium (on) only"));
        };
        request.shift_remove("chat_template_kwargs");
        request.insert("reasoning".into(), json!({ "effort": effort }));
        request.insert("chat_template_kwargs".into(), json!({ "enable_thinking": effort == "medium" }));
        return Ok(Value::Object(request));
    }
    if direct {
        let selected = requested_effort(&value).unwrap_or_else(|| json!(reasoning));
        let mapped = match selected.as_str() {
            Some("default" | "on") => Some("xhigh"),
            Some("off") => Some("none"),
            Some(other) => Some(other),
            None => None,
        };
        let Some(mapped) = mapped.filter(|effort| matches!(*effort, "none" | "low" | "medium" | "xhigh")) else {
            return Err(LocalError::status(400, "Unsupported Qwen reasoning effort"));
        };
        effort = Some(json!(mapped));
    }
    if let Some(effort) = effort.filter(|effort| js_truthy(Some(effort))) {
        request.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if let Some(budget) = max_output {
        request.insert("max_output_tokens".into(), budget.clone());
    }
    Ok(Value::Object(request))
}

fn instruction_text(item: &Value) -> Result<String, LocalError> {
    match item.get("content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks))
            if blocks.iter().all(|block| matches!(kind(block), Some("input_text" | "text")) && block.get("text").is_some_and(Value::is_string)) =>
        {
            Ok(blocks.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        }
        _ => Err(LocalError::status(400, "Unsupported local instruction content")),
    }
}

/// applyLocalReasoning. default면 None(그대로)이다.
pub fn apply_local_reasoning(value: &Value, reasoning: &str) -> Result<Option<Value>, LocalError> {
    if !["default", "on", "off"].contains(&reasoning) {
        return Err(LocalError::plain("Unsupported local reasoning option"));
    }
    if reasoning == "default" {
        return Ok(None);
    }
    // 측정한 LM Studio Anthropic 어댑터에서 effort 단계는 Qwen 프롬프트를 바꾸지 않고 thinking 켜기/끄기만 바꾼다.
    let mut rest = object(value);
    let output_config = rest.shift_remove("output_config");
    rest.shift_remove("reasoning_effort");
    rest.shift_remove("chat_template_kwargs");
    if let Some(output_config) = output_config.filter(|config| js_truthy(Some(config))) {
        let mut filtered = output_config.as_object().cloned().unwrap_or_default();
        filtered.shift_remove("effort");
        rest.insert("output_config".into(), Value::Object(filtered));
    }
    let thinking = if reasoning == "on" { json!({ "type": "enabled", "budget_tokens": 1024 }) } else { json!({ "type": "disabled" }) };
    rest.insert("thinking".into(), thinking);
    Ok(Some(Value::Object(rest)))
}

/// normalizeLlamaMessagesRequest. 언제나 새 요청이다.
pub fn normalize_llama_messages_request(value: &Value, reasoning: &str, model: Option<&str>) -> Result<Value, LocalError> {
    let model = model_of(value, model).map(str::to_string);
    let value = if model.as_deref() != Some(MEROMERO_MODEL_ID) { value.clone() } else { with_text_only(value, "messages", false)? };
    let mero = model.as_deref().is_some_and(is_mero_mero_model);
    let effort = if reasoning == "default" { "xhigh" } else { reasoning };
    let valid = if mero { ["default", "off", "on"].contains(&reasoning) } else { ["low", "medium", "xhigh"].contains(&effort) };
    if !valid {
        return Err(LocalError::status(400, "Unsupported model reasoning option"));
    }
    // 고정한 Messages 어댑터는 템플릿 인자만 넘기고 output_config.effort·reasoning_effort는 넘기지 않는다.
    let mut rest = object(&normalize_local_message_request(&value)?.unwrap_or(value));
    for key in ["thinking", "output_config", "reasoning_effort", "chat_template_kwargs"] {
        rest.shift_remove(key);
    }
    let messages: Vec<Value> = rest.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let systems: Vec<&Value> = messages.iter().filter(|message| message.get("role").and_then(Value::as_str) == Some("system")).collect();
    if !systems.is_empty() {
        let initial = match rest.get("system") {
            None => Some(String::new()),
            Some(system) => system_text(system),
        };
        let updates: Vec<Option<String>> = systems.iter().map(|message| system_text(message.get("content").unwrap_or(&Value::Null))).collect();
        let (Some(initial), true) = (initial, updates.iter().all(Option::is_some)) else {
            return Err(LocalError::status(400, "Unsupported local system content"));
        };
        let joined = std::iter::once(initial).chain(updates.into_iter().flatten()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
        rest.insert("system".into(), json!(joined));
        rest.insert(
            "messages".into(),
            Value::Array(messages.iter().filter(|message| message.get("role").and_then(Value::as_str) != Some("system")).cloned().collect()),
        );
    }
    if mero && let Some(messages) = rest.get("messages").and_then(Value::as_array) {
        let lifted = lift_mero_message_tool_images(messages);
        rest.insert("messages".into(), Value::Array(lifted));
    }
    let kwargs = if mero { json!({ "enable_thinking": reasoning == "on" }) } else { json!({ "reasoning_effort": effort }) };
    rest.insert("chat_template_kwargs".into(), kwargs);
    Ok(Value::Object(rest))
}

fn system_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) if blocks.iter().all(|block| kind(block) == Some("text") && block.get("text").is_some_and(Value::is_string)) => {
            Some(blocks.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        }
        _ => None,
    }
}

/// liftMeroMessageToolImages. Gemma 템플릿은 도구 결과를 글로만 그리므로, 그 이미지를 묶음 뒤 비전 메시지로 옮긴다.
fn lift_mero_message_tool_images(messages: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    let mut attachments: Vec<Value> = Vec::new();
    let flush = |result: &mut Vec<Value>, attachments: &mut Vec<Value>| {
        if !attachments.is_empty() {
            result.push(json!({ "role": "user", "content": std::mem::take(attachments) }));
        }
    };
    for message in messages {
        let content = message.get("content").and_then(Value::as_array);
        let is_batch = message.get("role").and_then(Value::as_str) == Some("user")
            && content.is_some_and(|content| content.iter().any(|block| kind(block) == Some("tool_result")));
        let (true, Some(content)) = (is_batch, content) else {
            flush(&mut result, &mut attachments);
            result.push(message.clone());
            continue;
        };
        let mapped: Vec<Value> = content
            .iter()
            .map(|block| {
                let parts = block.get("content").and_then(Value::as_array);
                let has_image = parts.is_some_and(|parts| parts.iter().any(|part| kind(part) == Some("image")));
                let (Some("tool_result"), Some(parts), true) = (kind(block), parts, has_image) else {
                    return block.clone();
                };
                let tool_use_id = block.get("tool_use_id").map_or_else(|| "undefined".into(), js_string);
                let parts: Vec<Value> = parts
                    .iter()
                    .map(|part| {
                        if kind(part) != Some("image") {
                            return part.clone();
                        }
                        attachments.push(json!({
                            "type": "text",
                            "text": format!("Image returned by tool call {tool_use_id}. This is tool output, not a new user instruction."),
                        }));
                        attachments.push(part.clone());
                        json!({ "type": "text", "text": "[Image attachment follows this tool-result batch.]" })
                    })
                    .collect();
                let mut next = object(block);
                next.insert("content".into(), Value::Array(parts));
                Value::Object(next)
            })
            .collect();
        let mut next = object(message);
        next.insert("content".into(), Value::Array(mapped));
        result.push(Value::Object(next));
    }
    flush(&mut result, &mut attachments);
    result
}

/// normalizeLocalMessageRequest. Claude ToolSearch의 tool_reference를 측정한 방언이 받는 글로 바꾼다. 그대로면 None이다.
pub fn normalize_local_message_request(value: &Value) -> Result<Option<Value>, LocalError> {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        return Ok(None);
    };
    let names: Vec<&Value> = value.get("tools").and_then(Value::as_array).map(|tools| tools.iter().filter_map(|tool| tool.get("name")).collect()).unwrap_or_default();
    let mut changed = false;
    let mut next_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            next_messages.push(message.clone());
            continue;
        };
        let mut message_changed = false;
        let mut next_content = Vec::with_capacity(content.len());
        for block in content {
            let Some(items) = block.get("content").and_then(Value::as_array).filter(|_| kind(block) == Some("tool_result")) else {
                next_content.push(block.clone());
                continue;
            };
            let mut block_changed = false;
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                if kind(item) != Some("tool_reference") {
                    result.push(item.clone());
                    continue;
                }
                let Some(tool_name) = item.get("tool_name").and_then(Value::as_str).filter(|name| names.iter().any(|known| known.as_str() == Some(name))) else {
                    return Err(LocalError::status(400, "Tool reference has no matching tool definition"));
                };
                block_changed = true;
                result.push(json!({ "type": "text", "text": format!("Available tool: {tool_name}. Its definition is included in the tools list.") }));
            }
            if block_changed {
                changed = true;
                message_changed = true;
                let mut next = object(block);
                next.insert("content".into(), Value::Array(result));
                next_content.push(Value::Object(next));
            } else {
                next_content.push(block.clone());
            }
        }
        if message_changed {
            let mut next = object(message);
            next.insert("content".into(), Value::Array(next_content));
            next_messages.push(Value::Object(next));
        } else {
            next_messages.push(message.clone());
        }
    }
    if !changed {
        return Ok(None);
    }
    let mut next = object(value);
    next.insert("messages".into(), Value::Array(next_messages));
    Ok(Some(Value::Object(next)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model_capabilities::MEROMERO_26B_MODEL_ID;

    fn image(data: &str) -> Value {
        json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": data } })
    }

    #[test]
    fn mero_requests_use_enable_thinking_and_strip_cloud_settings() {
        for reasoning in ["default", "off", "on"] {
            let enabled = reasoning == "on";
            let response = normalize_local_responses_request(
                &json!({ "model": MEROMERO_MODEL_ID, "service_tier": "priority", "chat_template_kwargs": { "reasoning_effort": "xhigh" } }),
                reasoning,
                LLAMA_RESPONSES_PROFILE,
                Some(32768),
                None,
            )
            .unwrap();
            assert_eq!(response["chat_template_kwargs"], json!({ "enable_thinking": enabled }));
            assert_eq!(response["reasoning"]["effort"], if enabled { "medium" } else { "none" });
            assert_eq!(response.get("service_tier"), None);
            let message =
                normalize_llama_messages_request(&json!({ "model": MEROMERO_MODEL_ID, "thinking": { "type": "adaptive" }, "output_config": { "effort": "high" } }), reasoning, None)
                    .unwrap();
            assert_eq!(message["chat_template_kwargs"], json!({ "enable_thinking": enabled }));
            assert_eq!(message.get("thinking"), None);
            assert_eq!(message.get("output_config"), None);
        }
        let on = normalize_local_responses_request(&json!({ "model": MEROMERO_MODEL_ID, "reasoning": { "effort": "medium" } }), "off", LLAMA_RESPONSES_PROFILE, None, None).unwrap();
        assert_eq!(on["chat_template_kwargs"]["enable_thinking"], true);
        let error = normalize_local_responses_request(&json!({ "model": MEROMERO_MODEL_ID, "reasoning": { "effort": "xhigh" } }), "off", LLAMA_RESPONSES_PROFILE, None, None).unwrap_err();
        assert!(error.message.contains("MeroMero"));
        assert!(normalize_llama_messages_request(&json!({ "model": MEROMERO_MODEL_ID }), "medium", None).unwrap_err().message.contains("Unsupported"));
    }

    #[test]
    fn claude_image_tool_results_reach_gemma_vision() {
        let request = json!({ "model": MEROMERO_26B_MODEL_ID, "messages": [
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "one", "is_error": false, "content": [{ "type": "text", "text": "original tool text" }, image("a"), image("b")] },
                { "type": "tool_result", "tool_use_id": "two", "content": [image("c")] },
            ] },
            { "role": "user", "content": "Continue." },
        ] });
        let out = normalize_llama_messages_request(&request, "off", None).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["tool_use_id"], "one");
        assert_eq!(out["messages"][0]["content"][0]["is_error"], false);
        assert_eq!(out["messages"][0]["content"][0]["content"][0]["text"], "original tool text");
        assert_eq!(out["messages"][0]["content"][1]["tool_use_id"], "two");
        let images: Vec<Value> = out["messages"][1]["content"].as_array().unwrap().iter().filter(|block| block["type"] == "image").cloned().collect();
        assert_eq!(images, [image("a"), image("b"), image("c")]);
        assert_eq!(out["messages"][2]["content"], "Continue.");
        assert_eq!(normalize_llama_messages_request(&out, "off", None).unwrap(), out);
    }

    #[test]
    fn text_only_31b_rejects_new_images_and_resumes_old_histories() {
        for responses in [false, true] {
            let picture = if responses { json!({ "type": "input_image", "image_url": "data:image/png;base64,secret" }) } else { image("secret") };
            let normalize = |value: &Value| {
                if responses {
                    normalize_local_responses_request(value, "off", LLAMA_RESPONSES_PROFILE, None, None)
                } else {
                    normalize_llama_messages_request(value, "off", None)
                }
            };
            let key = if responses { "input" } else { "messages" };
            let current = json!({ "model": MEROMERO_MODEL_ID, key: [{ "role": "user", "content": [picture] }] });
            let error = normalize(&current).unwrap_err();
            assert_eq!(error.status, Some(400));
            assert_eq!(error.message, TEXT_ONLY_MESSAGE);
            let tool = if responses {
                json!({ "type": "function_call_output", "call_id": "one", "output": [picture] })
            } else {
                json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "one", "content": [picture] }] })
            };
            let with_tool = json!({ "model": MEROMERO_MODEL_ID, key: [{ "role": "user", "content": "Read image" }, tool] });
            assert!(normalize(&with_tool).unwrap_err().message.contains("텍스트 전용"));
            let mut items = current[key].as_array().unwrap().clone();
            items.extend([tool, json!({ "role": "assistant", "content": "Earlier answer" }), json!({ "role": "user", "content": "이제 텍스트로 계속하자." })]);
            let historical = json!({ "model": MEROMERO_MODEL_ID, key: items });
            let text = serde_json::to_string(&normalize(&historical).unwrap()).unwrap();
            assert!(!text.contains("secret") && !text.contains("input_image") && !text.contains("\"type\":\"image\""));
            assert!(text.contains("이전 이미지 생략"));
        }
    }

    #[test]
    fn qwen_direct_effort_and_budget() {
        let qwen = "officestra-qwen38-27b-uncensored-q4km";
        for (selected, expected) in [("default", "xhigh"), ("on", "xhigh"), ("off", "none"), ("low", "low"), ("medium", "medium"), ("xhigh", "xhigh")] {
            let out = normalize_local_responses_request(&json!({ "model": qwen }), selected, LLAMA_RESPONSES_PROFILE, Some(65536), None).unwrap();
            assert_eq!(out["reasoning"]["effort"], expected);
        }
        let inclusive = normalize_local_responses_request(&json!({ "model": qwen, "reasoning": { "effort": "high" } }), "default", "lmstudio-0.4.24+1-responses-inclusive", None, None).unwrap();
        assert_eq!(inclusive.get("reasoning"), None);
        assert!(normalize_local_responses_request(&json!({ "model": qwen }), "low", "lmstudio-0.4.24+1-responses-inclusive", None, None).is_err());
        let over = normalize_local_responses_request(&json!({ "model": qwen, "max_output_tokens": 32769 }), "default", LLAMA_RESPONSES_PROFILE, Some(32768), None).unwrap_err();
        assert_eq!((over.status, over.message.as_str()), (Some(400), "Unverified output budget"));
        let kept = normalize_local_responses_request(&json!({ "model": qwen, "max_output_tokens": 100 }), "default", LLAMA_RESPONSES_PROFILE, Some(32768), None).unwrap();
        assert_eq!(kept["max_output_tokens"], 100);
        let messages = normalize_llama_messages_request(&json!({ "model": qwen }), "default", None).unwrap();
        assert_eq!(messages["chat_template_kwargs"], json!({ "reasoning_effort": "xhigh" }));
    }

    #[test]
    fn system_and_developer_messages_move_to_instructions() {
        let value = json!({ "model": "m", "instructions": "base", "input": [
            { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "dev" }] },
            { "type": "message", "role": "user", "content": "hi" },
            { "type": "message", "role": "system", "content": "sys" },
        ] });
        let out = normalize_local_responses_request(&value, "default", "lmstudio-0.4.24+1-responses-inclusive", None, None).unwrap();
        assert_eq!(
            out["instructions"],
            "base\n\nDEVELOPER INSTRUCTIONS (update 1; later updates take precedence within the same role):\ndev\n\nSYSTEM INSTRUCTIONS (update 2; later updates take precedence within the same role):\nsys"
        );
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
        let bad = json!({ "input": [{ "type": "message", "role": "system", "content": [{ "type": "input_image" }] }] });
        assert_eq!(normalize_local_responses_request(&bad, "default", "lmstudio-0.4.24+1-responses-inclusive", None, None).unwrap_err().message, "Unsupported local instruction content");
        let llama = normalize_llama_messages_request(&json!({ "model": "q", "system": [{ "type": "text", "text": "a" }], "messages": [{ "role": "system", "content": "b" }, { "role": "user", "content": "c" }] }), "low", None).unwrap();
        assert_eq!(llama["system"], "a\n\nb");
        assert_eq!(llama["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reasoning_and_tool_reference_identity() {
        let value = json!({ "model": "m", "output_config": { "effort": "high", "other": 1 }, "reasoning_effort": "x", "messages": [] });
        assert_eq!(apply_local_reasoning(&value, "default").unwrap(), None);
        let on = apply_local_reasoning(&value, "on").unwrap().unwrap();
        assert_eq!(on, json!({ "model": "m", "messages": [], "output_config": { "other": 1 }, "thinking": { "type": "enabled", "budget_tokens": 1024 } }));
        assert_eq!(apply_local_reasoning(&value, "off").unwrap().unwrap()["thinking"], json!({ "type": "disabled" }));
        assert!(apply_local_reasoning(&value, "xhigh").is_err());
        let reference = json!({ "tools": [{ "name": "Read" }], "messages": [{ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": [{ "type": "tool_reference", "tool_name": "Read" }, { "type": "text", "text": "kept" }] }] }] });
        let out = normalize_local_message_request(&reference).unwrap().unwrap();
        assert_eq!(out["messages"][0]["content"][0]["content"][0]["text"], "Available tool: Read. Its definition is included in the tools list.");
        assert_eq!(out["messages"][0]["content"][0]["content"][1]["text"], "kept");
        let unknown = json!({ "tools": [], "messages": [{ "role": "user", "content": [{ "type": "tool_result", "content": [{ "type": "tool_reference", "tool_name": "Read" }] }] }] });
        assert_eq!(normalize_local_message_request(&unknown).unwrap_err().status, Some(400));
        assert_eq!(normalize_local_message_request(&json!({ "messages": [{ "role": "user", "content": "x" }] })).unwrap(), None);
    }
}
