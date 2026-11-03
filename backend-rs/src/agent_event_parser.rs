// Codex·Claude·Antigravity CLI JSONL 한 줄을 공개 가능한 실시간 업무 이벤트로 바꾸고, 응답 끝의 기계용 블록을 읽는다.
// Node agent-event-parser.mjs를 옮겼다. 결과 객체의 키 순서와 Node가 TypeError를 내는 자리까지 맞춘다.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::js::{
    is_js_whitespace, js_keys, js_length, js_property, js_string, js_string_or_empty, js_trim, js_truthy, parse_js_json,
    relative_path, string_to_number, utf16_prefix, utf16_suffix,
};
use crate::work_record_provenance::normalize_response_sources;

const MAX_REASONING_LENGTH: usize = 6_000;
const MAX_COLLABORATION_PROMPT_LENGTH: usize = 4_000;
const MAX_COLLABORATION_RESULT_LENGTH: usize = 12_000;
const NEED_INPUT_MARKER: &str = "[NEED_INPUT]";
const RESPONSE_SOURCES_MARKER: &str = "[OFFICE_SOURCES]";
const WIKI_PROPOSALS_MARKER: &str = "[OFFICE_WIKI_PROPOSALS]";
const MAX_WIKI_PROPOSALS: usize = 3;
const MAX_WIKI_PROPOSAL_TITLE_LENGTH: usize = 120;
const MAX_WIKI_PROPOSAL_BODY_LENGTH: usize = 12_000;
const WIKI_PROPOSAL_FIELDS: [&str; 5] = ["pageKey", "kind", "title", "body", "approvalTier"];

/// 정규식의 `\s`(JS 공백과 줄 끝)를 문자 묶음 안에 그대로 넣기 위한 목록. is_js_whitespace와 같다.
const JS_SPACE_CLASS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
// 유니코드 플래그 없는 /i라 영문자는 ASCII끼리만 대소문자를 맞추고 \b도 ASCII 낱말 경계다.
static SENSITIVE_COMMAND_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i-u:api[_-]?key|token|password|passwd|secret|authorization|cookie|bearer)").unwrap()
});
static SENSITIVE_COMMAND_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i-u:\b(?:sk|rk|pk)-[a-z0-9_-]{8,})").unwrap());
static CREDENTIAL_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("://[^/{JS_SPACE_CLASS}:@]+:[^@{JS_SPACE_CLASS}/]+@")).unwrap()
});
// 플래그 m이 없어 ^와 $는 글 전체의 처음과 끝이다. 찾기는 정규식 g의 lastIndex처럼 앞 일치의 끝에서 이어 간다.
static MACHINE_BLOCK_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\n)(\[OFFICE_(?:SOURCES|WIKI_PROPOSALS)\])[ \t]*(?:\r?\n|$)").unwrap());
static STRANDED_SOURCES_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\n)\[OFFICE_SOURCES\][ \t]*(?:\r?\n|$)").unwrap());
static STRANDED_WIKI_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\n)\[OFFICE_WIKI_PROPOSALS\][ \t]*(?:\r?\n|$)").unwrap());

/// parseAgentEvent. JSON 객체가 아닌 줄과 모르는 이벤트는 None이다.
/// Node가 TypeError로 멈추는 줄(Claude content 배열의 null 항목)은 같은 문구의 Err다.
pub fn parse_agent_event(line: &str, backend: &str, workdir: Option<&str>) -> Result<Option<Value>, String> {
    let Ok(object @ Value::Object(_)) = parse_js_json(line) else {
        return Ok(None);
    };
    if backend == "claude" && type_is(&object, "prompt_suggestion") {
        let Some(suggestion) = clean_text(object.get("suggestion")) else {
            return Ok(None);
        };
        let event_key = clean_text(object.get("uuid")).map(|uuid| format!("suggestion:{uuid}"));
        return Ok(Some(json!({ "activity": activity("suggestion", suggestion, event_key, "completed") })));
    }
    match backend {
        "codex" => Ok(parse_codex_event(&object, workdir)),
        "claude" => parse_claude_event(&object, workdir),
        "antigravity" => Ok(parse_antigravity_event(&object, workdir)),
        _ => Ok(None),
    }
}

/// decodeAgentResponse. 응답 끝의 출처·위키 수정안 블록과 [NEED_INPUT] 표시를 떼어 낸다.
pub fn decode_agent_response(value: &str) -> Value {
    let blocks = response_machine_blocks(js_trim(value));
    let (text, needs_input) = need_input_response(&blocks.text);
    let mut response = json!({
        "text": text,
        "needsInput": needs_input,
        "sources": blocks.sources,
        "proposals": blocks.proposals,
        "wikiProposalError": blocks.wiki_proposal_error,
    });
    if let Some(source_error) = blocks.source_error {
        response["sourceError"] = source_error.into();
    }
    response
}

fn need_input_response(text: &str) -> (String, bool) {
    if let Some(rest) = text.strip_prefix(NEED_INPUT_MARKER) {
        return (rest.trim_start_matches(is_js_whitespace).to_string(), true);
    }
    // /^[\t ]*\[NEED_INPUT\][\t ]*(?:\r?\n|$)/gm. 플래그 m의 줄 끝은 \n·\r·U+2028·U+2029다.
    let line_starts = std::iter::once(0).chain(
        text.char_indices()
            .filter(|(_, character)| matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .map(|(index, character)| index + character.len_utf8()),
    );
    let Some((start, length)) =
        line_starts.filter_map(|start| need_input_line(&text[start..]).map(|length| (start, length))).last()
    else {
        return (text.to_string(), false);
    };
    (js_trim(&format!("{}{}", &text[..start], &text[start + length..])).to_string(), true)
}

fn need_input_line(rest: &str) -> Option<usize> {
    let after_marker = rest.trim_start_matches([' ', '\t']).strip_prefix(NEED_INPUT_MARKER)?;
    let after_space = after_marker.trim_start_matches([' ', '\t']);
    let consumed = rest.len() - after_space.len();
    if after_space.starts_with("\r\n") {
        Some(consumed + 2)
    } else if after_space.starts_with('\n') {
        Some(consumed + 1)
    } else if after_space.is_empty() || after_space.starts_with(['\r', '\u{2028}', '\u{2029}']) {
        Some(consumed)
    } else {
        None
    }
}

struct MachineBlocks {
    text: String,
    sources: Vec<Value>,
    proposals: Vec<Value>,
    wiki_proposal_error: Option<String>,
    source_error: Option<String>,
}

struct MachineBlock {
    marker: String,
    encoded: String,
    text: String,
}

fn response_machine_blocks(text: &str) -> MachineBlocks {
    let mut visible_text = text.to_string();
    let mut blocks = Vec::new();
    while let Some(block) = trailing_response_machine_block(&visible_text) {
        visible_text = block.text.clone();
        blocks.insert(0, block);
    }
    let sources_blocks: Vec<&MachineBlock> = blocks.iter().filter(|block| block.marker == RESPONSE_SOURCES_MARKER).collect();
    let mut sources = Vec::new();
    let mut source_error = None;
    match sources_blocks.as_slice() {
        [block] => match parse_js_json(unfenced_machine_block(&block.encoded)) {
            // 검증 규칙 위반과 JSON 문법 오류를 같은 문구로 뭉치면 직원이 무엇을 고칠지 몰라 같은 실수를 되풀이한다.
            Ok(value) => match normalize_response_sources(&value, 20) {
                Ok(normalized) => sources = normalized,
                Err(error) => source_error = Some(format!("응답 근거를 저장하지 못했습니다. {}", error.0)),
            },
            Err(_) => source_error = Some("응답 근거 형식을 읽지 못했습니다.".to_string()),
        },
        [] if stranded_machine_block_marker(&visible_text, &STRANDED_SOURCES_MARKER) => {
            source_error = Some(misplaced_block_message("응답 근거"));
        }
        [] => {}
        _ => source_error = Some("응답 근거 블록은 하나만 사용할 수 있습니다.".to_string()),
    }
    let proposal_blocks: Vec<&MachineBlock> = blocks.iter().filter(|block| block.marker == WIKI_PROPOSALS_MARKER).collect();
    let mut proposals = Vec::new();
    let mut wiki_proposal_error = None;
    match proposal_blocks.as_slice() {
        [block] => match parse_js_json(unfenced_machine_block(&block.encoded))
            .map_err(|error| error.to_string())
            .and_then(|value| normalized_wiki_proposals(&value))
        {
            Ok(normalized) => proposals = normalized,
            Err(_) => wiki_proposal_error = Some("위키 수정안 형식을 읽지 못했습니다.".to_string()),
        },
        [] if stranded_machine_block_marker(&visible_text, &STRANDED_WIKI_MARKER) => {
            wiki_proposal_error = Some(misplaced_block_message("위키 수정안"));
        }
        [] => {}
        _ => wiki_proposal_error = Some("위키 수정안 블록은 하나만 사용할 수 있습니다.".to_string()),
    }
    MachineBlocks { text: js_trim(&visible_text).to_string(), sources, proposals, wiki_proposal_error, source_error }
}

fn misplaced_block_message(label: &str) -> String {
    format!("{label} 블록은 응답 맨 끝에 두고 뒤에 아무것도 쓰지 마십시오.")
}

/// 정규식 g 플래그 exec 반복처럼 앞 일치가 끝난 자리부터 다시 찾아 마지막 일치를 돌려준다.
fn last_marker_match<'a>(pattern: &Regex, text: &'a str) -> Option<regex::Captures<'a>> {
    let mut position = 0;
    let mut last = None;
    while let Some(captures) = pattern.captures_at(text, position) {
        position = captures.get(0).map_or(text.len(), |whole| whole.end());
        last = Some(captures);
    }
    last
}

/// 마커와 JSON 뒤에 인사말을 더 붙이면 블록으로 인식되지 않아 근거가 조용히 사라진다. 남은 본문에 항목이 든 마커가 있는지 본다.
/// 빈 배열은 형식을 설명하려고 적은 예시라 알리지 않는다.
fn stranded_machine_block_marker(text: &str, pattern: &Regex) -> bool {
    let Some(whole) = last_marker_match(pattern, text).and_then(|captures| captures.get(0)) else {
        return false;
    };
    let encoded = js_trim(&text[whole.end()..]);
    let payload = unfenced_machine_block(encoded).trim_start_matches(is_js_whitespace);
    let opens = |rest: Option<&str>, next: char| rest.is_some_and(|rest| rest.trim_start_matches(is_js_whitespace).starts_with(next));
    opens(payload.strip_prefix('['), '{') || opens(payload.strip_prefix('{'), '"')
}

fn trailing_response_machine_block(text: &str) -> Option<MachineBlock> {
    let captures = last_marker_match(&MACHINE_BLOCK_MARKER, text)?;
    let whole = captures.get(0)?;
    let marker_index = whole.start() + usize::from(whole.as_str().starts_with('\n'));
    let encoded = js_trim(&text[whole.end()..]);
    if has_content_after_machine_payload(encoded) {
        return None;
    }
    Some(MachineBlock {
        marker: captures.get(1)?.as_str().to_string(),
        encoded: encoded.to_string(),
        text: js_trim(&text[..marker_index]).to_string(),
    })
}

fn has_content_after_machine_payload(encoded: &str) -> bool {
    if encoded.starts_with("```") {
        // 첫 줄바꿈이 없으면 indexOf("\n")이 -1이라 0부터 찾아 여는 울타리 자신을 닫는 울타리로 본다.
        let search_from = encoded.find('\n').map_or(0, |end| end + 1);
        return encoded[search_from..]
            .find("```")
            .is_some_and(|offset| !js_trim(&encoded[search_from + offset + 3..]).is_empty());
    }
    if !encoded.starts_with(['[', '{']) {
        return false;
    }
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in encoded.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return !js_trim(&encoded[index + 1..]).is_empty();
                }
            }
            _ => {}
        }
    }
    false
}

/// ```` ```json ```` 울타리를 벗긴다. `/^```(?:json)?\s*/i` 뒤에 `/\s*```$/` 순서다.
fn unfenced_machine_block(encoded: &str) -> &str {
    let Some(rest) = encoded.strip_prefix("```") else {
        return encoded;
    };
    let rest = match rest.get(..4) {
        Some(word) if word.eq_ignore_ascii_case("json") => &rest[4..],
        _ => rest,
    };
    let rest = rest.trim_start_matches(is_js_whitespace);
    rest.strip_suffix("```").map_or(rest, |body| body.trim_end_matches(is_js_whitespace))
}

/// normalizedWikiProposals. 공개 필드 형식만 본다. pageKey의 프로젝트 소속과 근거 유효성은 저장 단계가 맡는다.
pub fn normalized_wiki_proposals(value: &Value) -> Result<Vec<Value>, String> {
    let Value::Array(proposals) = value else {
        return Err("Invalid wiki proposals array.".to_string());
    };
    if proposals.len() > MAX_WIKI_PROPOSALS {
        return Err("Invalid wiki proposals array.".to_string());
    }
    proposals.iter().map(normalized_wiki_proposal).collect()
}

fn normalized_wiki_proposal(value: &Value) -> Result<Value, String> {
    let Value::Object(fields) = value else {
        return Err("Invalid wiki proposal.".to_string());
    };
    if fields.len() != WIKI_PROPOSAL_FIELDS.len() || fields.keys().any(|field| !WIKI_PROPOSAL_FIELDS.contains(&field.as_str())) {
        return Err("Invalid wiki proposal fields.".to_string());
    }
    let text = |key: &str| fields.get(key).and_then(Value::as_str);
    let valid = text("pageKey").is_some_and(wiki_page_key)
        && text("kind").is_some_and(|kind| matches!(kind, "decision" | "constraint" | "incident"))
        && text("title").is_some_and(|title| title.chars().count() <= MAX_WIKI_PROPOSAL_TITLE_LENGTH)
        && text("body").is_some_and(|body| body.chars().count() <= MAX_WIKI_PROPOSAL_BODY_LENGTH)
        && text("approvalTier").is_some_and(|tier| matches!(tier, "peer" | "user"));
    if !valid {
        return Err("Invalid wiki proposal values.".to_string());
    }
    Ok(json!({
        "pageKey": fields["pageKey"],
        "kind": fields["kind"],
        "title": fields["title"],
        "body": fields["body"],
        "approvalTier": fields["approvalTier"],
    }))
}

/// `/^[a-z0-9][a-z0-9-]{0,79}$/`
fn wiki_page_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    let allowed = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=80).contains(&bytes.len()) && allowed(&bytes[0]) && bytes[1..].iter().all(|byte| allowed(byte) || *byte == b'-')
}

fn parse_codex_event(object: &Value, workdir: Option<&str>) -> Option<Value> {
    let event_type = object.get("type").and_then(Value::as_str);
    match event_type {
        Some("thread.started") => return Some(json!({ "sessionID": clean_text(object.get("thread_id")) })),
        Some("turn.started") => return None,
        Some("turn.completed") => return normalized_usage(object.get("usage"), "codex", None).map(|usage| json!({ "usage": usage })),
        Some("turn.failed") => {
            let failure = failure_text(object.get("error")).unwrap_or_else(|| "Codex 작업이 완료되지 못했습니다.".to_string());
            return Some(json!({ "failure": failure }));
        }
        Some("error") => {
            return Some(json!({ "warning": failure_text(object.get("message")).or_else(|| failure_text(object.get("error"))) }));
        }
        _ => {}
    }
    let event_type = event_type.filter(|kind| matches!(*kind, "item.started" | "item.updated" | "item.completed"))?;
    // 객체가 아닌 참값 item은 item.type이 undefined라 어느 갈래에도 들지 않는다.
    let item = object.get("item").filter(|item| js_truthy(Some(item)))?;
    let event_key = codex_event_key(item);
    let status = || codex_activity_status(item, event_type);
    let single = |activity: Map<String, Value>| Some(json!({ "activity": activity }));
    match item.get("type").and_then(Value::as_str)? {
        "reasoning" => {
            let mut activity = activity("thinking", codex_reasoning_text(item)?, event_key, status());
            activity.insert("isCodexReasoning".into(), true.into());
            single(activity)
        }
        "agent_message" => {
            clean_text(item.get("text")).map(|text| json!({ "agentMessage": text, "agentMessageKey": event_key }))
        }
        "command_execution" => {
            let command = safe_command(item.get("command")).unwrap_or_else(|| "명령".to_string());
            single(activity("command", command, event_key, status()))
        }
        "file_change" => {
            let changes = item.get("changes");
            let text = if event_type == "item.completed" {
                file_change_activity_text(changes, None, workdir)
            } else {
                file_change_running_text(changes)
            };
            Some(json!({
                "activity": activity("tool", text, event_key.clone(), status()),
                "fileChange": { "eventKey": event_key, "phase": event_type, "changes": file_change_metadata(changes) },
            }))
        }
        "mcp_tool_call" => single(activity("tool", mcp_activity_text(item), event_key, status())),
        "collab_tool_call" => {
            let activities = collab_activities(item, event_type, event_key);
            (!activities.is_empty()).then(|| json!({ "activities": activities }))
        }
        "web_search" => single(activity("tool", web_search_activity_text(item), event_key, status())),
        "error" => {
            let text = clean_text(first(item, &["message", "error"]))?;
            single(activity("tool", format!("오류 · {text}"), event_key, "failed"))
        }
        "todo_list" if event_type != "item.updated" => single(activity("thinking", todo_activity_text(item), event_key, "completed")),
        _ => None,
    }
}

fn codex_activity_status(item: &Value, event_type: &str) -> &'static str {
    if event_type != "item.completed" {
        return "running";
    }
    if let Some(exit_code) = numeric_value(first(item, &["exit_code", "exitCode"])) {
        return if exit_code == 0.0 { "completed" } else { "failed" };
    }
    let status = clean_text(item.get("status")).map(|status| status.to_lowercase());
    let failed_status = status.as_deref().is_some_and(|status| matches!(status, "failed" | "error" | "cancelled" | "canceled" | "declined"));
    if failed_status || js_truthy(item.get("error")) { "failed" } else { "completed" }
}

/// fileChangeActivityText. statistics가 참값이면 `+추가 -삭제` 줄을 넣는다(없는 칸은 템플릿 문자열처럼 "undefined").
pub fn file_change_activity_text(changes: Option<&Value>, statistics: Option<&Value>, workdir: Option<&str>) -> String {
    let summaries: Vec<String> = change_entries(changes)
        .filter_map(|change| {
            let path = clean_text(change_path(change))?;
            Some(format!("{} {}", file_change_label(change.get("kind")), compact_path(&path, workdir)))
        })
        .collect();
    if summaries.is_empty() {
        return "파일 변경 완료".to_string();
    }
    let mut lines = vec![format!("파일 {}개를 편집했습니다", summaries.len())];
    if let Some(statistics) = statistics.filter(|statistics| js_truthy(Some(statistics))) {
        lines.push(format!("+{} -{}", template_text(statistics.get("additions")), template_text(statistics.get("deletions"))));
    }
    lines.extend(summaries.iter().take(40).cloned());
    if summaries.len() > 40 {
        lines.push(format!("외 {}개", summaries.len() - 40));
    }
    lines.join("\n")
}

fn file_change_running_text(changes: Option<&Value>) -> String {
    match file_change_metadata(changes).len() {
        0 => "파일 변경을 적용하는 중".to_string(),
        count => format!("파일 변경을 적용하는 중 · {count}개"),
    }
}

fn file_change_metadata(changes: Option<&Value>) -> Vec<Value> {
    change_entries(changes)
        .filter_map(|change| {
            let path = clean_text(change_path(change))?;
            Some(json!({ "path": path, "kind": clean_text(change.get("kind")) }))
        })
        .collect()
}

fn change_entries(changes: Option<&Value>) -> impl Iterator<Item = &Value> {
    changes.and_then(Value::as_array).into_iter().flatten()
}

fn change_path(change: &Value) -> Option<&Value> {
    first(change, &["path", "file_path", "file", "move_path"])
}

fn mcp_activity_text(item: &Value) -> String {
    let server = clean_text(first(item, &["server", "server_name", "appName", "app_name"]));
    let tool = clean_text(first(item, &["tool", "name", "actionName", "action_name"]));
    let target = [server, tool].into_iter().flatten().collect::<Vec<_>>().join("/");
    if target.is_empty() { "연결 도구".to_string() } else { target }
}

fn collab_activities(item: &Value, event_type: &str, event_key: Option<String>) -> Vec<Value> {
    if event_type != "item.completed" {
        return Vec::new();
    }
    let tool = normalized_collab_tool(first(item, &["tool", "name", "action"]));
    if tool == "close_agent" {
        return Vec::new();
    }
    let call_status = codex_activity_status(item, event_type);
    let prompt = limited_collab_text(first(item, &["prompt", "message", "input"]), MAX_COLLABORATION_PROMPT_LENGTH);
    let states = first(item, &["agents_states", "agent_states", "agentsStates"]).and_then(Value::as_object);
    let thread_ids = collab_thread_ids(item, states);
    let agent_label = if thread_ids.len() == 1 {
        clean_text(first(item, &["receiver_agent_nickname", "receiver_agent", "agent_name", "agent"]))
    } else {
        None
    };
    // agentStates[threadID] ?? {}. 객체가 아닌 상태 값은 칸을 읽으면 모두 undefined다.
    let state_field = |thread_id: &str, keys: &[&str]| {
        states.and_then(|states| states.get(thread_id)).and_then(|state| first(state, keys))
    };
    let summary_or_status = |text: &Option<String>, status: &str| match text {
        Some(text) => collaboration_summary_text(text),
        None => collaboration_status_text(status).to_string(),
    };
    let collaboration = |text: String, key: Option<String>, status: &str, collaboration: Value| {
        let mut activity = activity("collaboration", text, key, status);
        activity.insert("collaboration".into(), collaboration);
        Value::Object(activity)
    };
    match tool.as_str() {
        "wait" => thread_ids
            .iter()
            .filter_map(|thread_id| {
                let agent_status = normalized_collab_agent_status(state_field(thread_id, &["status"]));
                let message =
                    limited_collab_text(state_field(thread_id, &["message", "result", "output"]), MAX_COLLABORATION_RESULT_LENGTH);
                if message.is_none() && matches!(agent_status.as_deref(), Some("pending_init" | "running")) {
                    return None;
                }
                let status = collab_agent_activity_status(agent_status.as_deref(), call_status, "result");
                Some(collaboration(
                    summary_or_status(&message, status),
                    Some(collaboration_event_key(event_key.as_deref(), thread_id)),
                    status,
                    json!({ "action": "result", "agentThreadId": thread_id, "agentLabel": agent_label, "message": message, "agentStatus": agent_status }),
                ))
            })
            .collect(),
        "spawn_agent" | "send_input" if thread_ids.is_empty() => {
            if tool == "send_input" || call_status != "failed" {
                return Vec::new();
            }
            vec![collaboration(
                "협업 검토를 시작하지 못했습니다.".to_string(),
                event_key,
                "failed",
                json!({ "action": "spawn", "agentLabel": agent_label, "prompt": prompt, "agentStatus": "errored" }),
            )]
        }
        "spawn_agent" | "send_input" => {
            let action = if tool == "spawn_agent" { "spawn" } else { "follow_up" };
            thread_ids
                .iter()
                .map(|thread_id| {
                    let agent_status = normalized_collab_agent_status(state_field(thread_id, &["status"]))
                        .unwrap_or_else(|| if call_status == "failed" { "errored" } else { "running" }.to_string());
                    let status = collab_agent_activity_status(Some(agent_status.as_str()), call_status, action);
                    collaboration(
                        summary_or_status(&prompt, status),
                        Some(collaboration_event_key(event_key.as_deref(), thread_id)),
                        status,
                        json!({ "action": action, "agentThreadId": thread_id, "agentLabel": agent_label, "prompt": prompt, "agentStatus": agent_status }),
                    )
                })
                .collect()
        }
        _ => thread_ids
            .iter()
            .map(|thread_id| {
                let text = prompt.as_deref().map_or_else(|| "협업 작업".to_string(), collaboration_summary_text);
                collaboration(
                    text,
                    Some(collaboration_event_key(event_key.as_deref(), thread_id)),
                    call_status,
                    json!({ "action": "other", "agentThreadId": thread_id, "agentLabel": agent_label, "prompt": prompt }),
                )
            })
            .collect(),
    }
}

fn normalized_collab_tool(value: Option<&Value>) -> String {
    let tool = clean_text(value).map(|tool| tool.replace('-', "_").to_lowercase());
    match tool.as_deref() {
        Some("spawnagent") => "spawn_agent".to_string(),
        Some("sendinput") => "send_input".to_string(),
        Some("closeagent") => "close_agent".to_string(),
        _ => tool.unwrap_or_else(|| "collaboration".to_string()),
    }
}

/// 받는 스레드 ID 목록. 배열 두 칸, 단일 칸, 상태 객체 키(Object.keys 순서) 순으로 모아 다듬고 중복을 지운다.
fn collab_thread_ids(item: &Value, states: Option<&Map<String, Value>>) -> Vec<String> {
    let mut values: Vec<Option<String>> = Vec::new();
    for key in ["receiver_thread_ids", "receiverThreadIds"] {
        if let Some(Value::Array(ids)) = item.get(key) {
            values.extend(ids.iter().map(|id| clean_text(Some(id))));
        }
    }
    values.push(clean_text(first(item, &["receiver_thread_id", "receiverThreadId"])));
    values.extend(states.into_iter().flat_map(js_keys).map(clean_str));
    let mut thread_ids: Vec<String> = Vec::new();
    for id in values.into_iter().flatten() {
        if !thread_ids.contains(&id) {
            thread_ids.push(id);
        }
    }
    thread_ids
}

fn normalized_collab_agent_status(value: Option<&Value>) -> Option<String> {
    clean_text(value).map(|status| status.replace('-', "_").to_lowercase()).filter(|status| !status.is_empty())
}

fn collab_agent_activity_status(agent_status: Option<&str>, call_status: &'static str, action: &str) -> &'static str {
    match agent_status {
        _ if call_status == "failed" => "failed",
        Some("interrupted" | "errored" | "not_found") => "failed",
        Some("completed" | "shutdown") => "completed",
        Some("pending_init" | "running") => "running",
        _ if action == "spawn" || action == "follow_up" => "running",
        _ => call_status,
    }
}

fn collaboration_event_key(event_key: Option<&str>, thread_id: &str) -> String {
    ["collaboration", event_key.unwrap_or(""), thread_id].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(":")
}

fn collaboration_summary_text(value: &str) -> String {
    safe_public_text(value, 320)
}

fn collaboration_status_text(status: &str) -> &'static str {
    match status {
        "running" => "협업 검토 중",
        "failed" => "협업 검토에 실패했습니다.",
        _ => "협업 검토를 마쳤습니다.",
    }
}

fn limited_collab_text(value: Option<&Value>, limit: usize) -> Option<String> {
    clean_text(value).map(|text| truncated(text.replace("\r\n", "\n"), limit))
}

fn web_search_activity_text(item: &Value) -> String {
    let action = item.get("action");
    let action_field = |key: &str| action.and_then(|action| action.get(key));
    let detail = clean_text(coalesce([item.get("query"), action_field("query"), action_field("pattern"), action_field("url")]));
    detail.map_or_else(|| "웹 검색".to_string(), |detail| format!("검색 · {}", safe_public_text(&detail, 220)))
}

fn todo_activity_text(item: &Value) -> String {
    let entries = item
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| item.get("todos").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default();
    let pending = entries.iter().find(|entry| {
        matches!(entry, Value::Object(_) | Value::Array(_))
            && entry.get("completed") != Some(&Value::Bool(true))
            && entry.get("status") != Some(&json!("completed"))
    });
    match pending.and_then(|pending| clean_text(first(pending, &["text", "step", "title"]))) {
        Some(text) => format!("계획 · {}", safe_public_text(&text, 220)),
        None if !entries.is_empty() => format!("계획 · {}단계", entries.len()),
        None => "작업 계획 정리".to_string(),
    }
}

fn codex_event_key(item: &Value) -> Option<String> {
    clean_text(first(item, &["id", "item_id", "itemId"]))
}

fn codex_reasoning_text(item: &Value) -> Option<String> {
    let raw = nested_reasoning_text(first(item, &["raw_reasoning", "rawReasoning", "raw_content", "rawContent", "content"]));
    raw.and_then(|raw| reasoning_text(Some(&Value::String(raw))))
        .or_else(|| reasoning_text(item.get("text")))
        .or_else(|| reasoning_text(item.get("summary")))
}

fn nested_reasoning_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(entries) => Some(
            entries
                .iter()
                .filter_map(|entry| nested_reasoning_text(Some(entry)))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        value @ Value::Object(_) => nested_reasoning_text(first(value, &["text", "content", "reasoning", "summary"])),
        _ => None,
    }
}

fn parse_claude_event(object: &Value, workdir: Option<&str>) -> Result<Option<Value>, String> {
    if type_is(object, "system") && object.get("subtype").and_then(Value::as_str) == Some("init") {
        return Ok(Some(json!({ "sessionID": clean_text(object.get("session_id")), "resolvedModel": clean_text(object.get("model")) })));
    }
    match object.get("type").and_then(Value::as_str) {
        Some("stream_event") => Ok(object.get("event").and_then(|event| parse_claude_stream_event(event, workdir))),
        Some("assistant") => parse_claude_assistant(object, workdir),
        Some("user") => {
            let Some(Value::Array(content)) = object.get("message").and_then(|message| message.get("content")) else {
                return Ok(None);
            };
            let mut activities = Vec::new();
            for item in content {
                if js_property(item, "type")?.and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let status = if item.get("is_error") == Some(&Value::Bool(true)) { "failed" } else { "completed" };
                let mut activity = activity("tool", "도구 완료", clean_text(item.get("tool_use_id")), status);
                activity.insert("preserveText".into(), true.into());
                activities.push(Value::Object(activity));
            }
            Ok((!activities.is_empty()).then(|| json!({ "activities": activities })))
        }
        Some("result") => {
            let session_id = clean_text(object.get("session_id"));
            let usage = normalized_claude_result_usage(object);
            Ok(Some(if object.get("is_error") == Some(&Value::Bool(true)) {
                let failure = clean_text(object.get("result")).unwrap_or_else(|| "Claude Code 작업이 완료되지 못했습니다.".to_string());
                json!({ "sessionID": session_id, "usage": usage, "failure": failure })
            } else {
                json!({ "sessionID": session_id, "usage": usage, "responseText": clean_text(object.get("result")) })
            }))
        }
        _ => Ok(None),
    }
}

/// `object.event ?? {}`. 객체가 아닌 event는 event.type이 undefined라 None이다.
fn parse_claude_stream_event(event: &Value, workdir: Option<&str>) -> Option<Value> {
    let nested_type = |key: &str| event.get(key).and_then(|nested| nested.get("type")).and_then(Value::as_str);
    match event.get("type").and_then(Value::as_str)? {
        "message_start" => {
            Some(json!({ "streamMessageID": clean_text(event.get("message").and_then(|message| message.get("id"))) }))
        }
        "content_block_delta" if nested_type("delta") == Some("text_delta") => {
            let delta = js_string_or_empty(event.get("delta").and_then(|delta| delta.get("text")));
            (!delta.is_empty()).then(|| json!({ "responseDelta": delta }))
        }
        "content_block_start" if nested_type("content_block") == Some("thinking") => {
            let index = event.get("index").filter(|index| !index.is_null()).map_or_else(|| "0".to_string(), js_string);
            let mut activity = activity("thinking", "추론 중", Some(format!("block:{index}")), "running");
            activity.insert("messageScoped".into(), true.into());
            Some(json!({ "activity": activity }))
        }
        "content_block_start" if nested_type("content_block") == Some("tool_use") => {
            let block = &event["content_block"];
            let text = claude_tool_activity_text(block, workdir);
            Some(json!({ "activity": activity("tool", text, claude_tool_event_key(block), "running") }))
        }
        _ => None,
    }
}

fn parse_claude_assistant(object: &Value, workdir: Option<&str>) -> Result<Option<Value>, String> {
    let message = object.get("message");
    let Some(Value::Array(content)) = message.and_then(|message| message.get("content")) else {
        return Ok(None);
    };
    let message_id = clean_text(message.and_then(|message| message.get("id")));
    let mut activities = Vec::new();
    // content.forEach의 item.type. null 항목이면 Node는 여기서 TypeError로 멈춘다.
    for (index, item) in content.iter().enumerate() {
        match js_property(item, "type")?.and_then(Value::as_str) {
            Some("thinking") => {
                if let Some(text) = reasoning_text(item.get("thinking")) {
                    let event_key = message_id.as_ref().map(|id| format!("{id}:block:{index}"));
                    activities.push(Value::Object(activity("thinking", text, event_key, "completed")));
                }
            }
            Some("tool_use") => {
                let text = claude_tool_activity_text(item, workdir);
                activities.push(Value::Object(activity("tool", text, claude_tool_event_key(item), "running")));
            }
            _ => {}
        }
    }
    let public_text: String = content
        .iter()
        .filter(|item| type_is(item, "text"))
        .map(|item| js_string_or_empty(item.get("text")))
        .collect();
    let mut result = Map::new();
    if let Some(model) = claude_message_model(object) {
        result.insert("resolvedModel".into(), model.into());
    }
    if !activities.is_empty() {
        result.insert("activities".into(), activities.into());
    }
    let public_text = js_trim(&public_text);
    if !public_text.is_empty() {
        result.insert("agentMessage".into(), public_text.into());
        result.insert("agentMessageKey".into(), message_id.map_or(Value::Null, Value::from));
    }
    Ok((!result.is_empty()).then_some(Value::Object(result)))
}

fn parse_antigravity_event(object: &Value, workdir: Option<&str>) -> Option<Value> {
    match object.get("event").and_then(Value::as_str)? {
        "init" => {
            let session = coalesce([object.get("conversation_id"), object.get("init").and_then(|init| init.get("conversation_id"))]);
            Some(json!({ "sessionID": clean_text(session) }))
        }
        "step_update" => parse_antigravity_step(object.get("step_update").filter(|update| update.is_object())?, workdir),
        "result" => {
            let result = object.get("result").filter(|result| result.is_object())?;
            let session_id = clean_text(coalesce([result.get("conversation_id"), object.get("conversation_id")]));
            let usage_fallback = normalized_antigravity_usage(result.get("usage"));
            let status = clean_text(result.get("status")).map(|status| status.to_uppercase());
            if status.as_deref().is_some_and(|status| status != "SUCCESS") {
                let failure = clean_text(first(result, &["error", "response"]))
                    .unwrap_or_else(|| "Antigravity 작업이 완료되지 못했습니다.".to_string());
                // 한도 오류가 난 대화를 이어 가면 agy가 정상 답에도 옛 오류를 붙인다. 런타임이 종료 코드로 가리도록 답도 넘긴다.
                let response_fallback = if js_truthy(result.get("error")) { clean_text(result.get("response")) } else { None };
                return Some(json!({
                    "sessionID": session_id, "usageFallback": usage_fallback, "failure": failure, "responseFallback": response_fallback,
                }));
            }
            // 최종 response는 단계 메시지를 모두 이은 전문이라, 단계 메시지를 하나도 못 받았을 때만 쓰는 fallback이다.
            Some(json!({ "sessionID": session_id, "usageFallback": usage_fallback, "responseFallback": clean_text(result.get("response")) }))
        }
        "error" => {
            let failure = failure_text(first(object, &["error", "message"]))
                .unwrap_or_else(|| "Antigravity 작업이 완료되지 못했습니다.".to_string());
            Some(json!({ "failure": failure }))
        }
        "warning" => Some(json!({ "warning": failure_text(first(object, &["warning", "message"])) })),
        _ => None,
    }
}

fn parse_antigravity_step(update: &Value, workdir: Option<&str>) -> Option<Value> {
    let step_type = clean_text(update.get("step_type")).map(|step| step.to_lowercase());
    let state = clean_text(update.get("state")).map(|state| state.to_uppercase());
    let state = state.as_deref();
    match step_type.as_deref()? {
        "thinking" | "thought" | "reasoning" => {
            let text = reasoning_text(first(update, &["thinking", "thought", "text", "text_delta", "reasoning"]))?;
            let event_key = antigravity_step_key(update, Some("thinking"));
            Some(json!({ "activity": activity("thinking", text, Some(event_key), antigravity_activity_status(state, None)) }))
        }
        "agent_response" => {
            let delta = js_string_or_empty(update.get("text_delta"));
            let usage = if state == Some("DONE") { normalized_antigravity_usage(update.get("usage")) } else { None };
            let activities: Vec<Value> = clean_text(first(update, &["thinking", "thought"]))
                .map(|thinking| {
                    let text = truncated(thinking, MAX_REASONING_LENGTH);
                    let event_key = antigravity_step_key(update, Some("thinking"));
                    Value::Object(activity("thinking", text, Some(event_key), antigravity_activity_status(state, None)))
                })
                .into_iter()
                .collect();
            if delta.is_empty() && usage.is_none() && activities.is_empty() {
                return None;
            }
            // 한 응답 단계는 ACTIVE 조각 여럿과 DONE 하나로 온다. 도구 사이 중간 메시지도 단계 단위로 끊도록 열쇠와 종료 여부를 넘긴다.
            let step_key = antigravity_step_key(update, None);
            let mut result = Map::new();
            if !delta.is_empty() {
                result.insert("responseDelta".into(), delta.into());
                result.insert("responseStepKey".into(), step_key.clone().into());
            }
            if state == Some("DONE") {
                result.insert("responseStepKey".into(), step_key.into());
                result.insert("responseStepDone".into(), true.into());
            }
            if let Some(usage) = usage {
                result.insert("usage".into(), usage);
                result.insert("usageIsDelta".into(), true.into());
            }
            if !activities.is_empty() {
                result.insert("activities".into(), activities.into());
            }
            Some(Value::Object(result))
        }
        "tool" => {
            let tool_info = update.get("tool_info");
            let name = clean_text(coalesce([update.get("tool_name"), tool_info.and_then(|info| info.get("name"))]))
                .unwrap_or_else(|| "도구".to_string());
            let kind = if name == "run_command" { "command" } else { "tool" };
            let text = antigravity_tool_activity_text(&name, tool_info, workdir);
            let status = antigravity_activity_status(state, tool_info);
            Some(json!({ "activity": activity(kind, text, Some(antigravity_step_key(update, None)), status) }))
        }
        _ => None,
    }
}

/// ["antigravity", 대화 ID, step_index, 접미사]에서 null·undefined를 빼고 ":"로 잇는다.
fn antigravity_step_key(update: &Value, suffix: Option<&str>) -> String {
    let mut parts = vec!["antigravity".to_string()];
    parts.extend(clean_text(update.get("conversation_id")));
    parts.extend(update.get("step_index").filter(|index| !index.is_null()).map(js_string));
    parts.extend(suffix.map(str::to_string));
    parts.join(":")
}

fn normalized_antigravity_usage(value: Option<&Value>) -> Option<Value> {
    let value = value.filter(|value| value.is_object())?;
    let counts = [
        token_count(value.get("input_tokens")),
        token_count(value.get("output_tokens")),
        token_count(value.get("cache_read_tokens")),
        token_count(value.get("thinking_tokens")),
    ];
    if counts.iter().all(Option::is_none) {
        return None;
    }
    let number = |count: Option<f64>| count.map_or(Value::Null, Value::from);
    Some(json!({
        "inputTokens": number(counts[0]),
        "outputTokens": number(counts[1]),
        "cachedInputTokens": number(counts[2]),
        "cacheWriteInputTokens": null,
        "cacheWrite5mInputTokens": null,
        "cacheWrite1hInputTokens": null,
        "reasoningOutputTokens": number(counts[3]),
        "serviceTier": null,
        "speed": null,
        "inferenceGeo": null,
        "reportedCostUsd": null,
    }))
}

fn antigravity_activity_status(state: Option<&str>, tool_info: Option<&Value>) -> &'static str {
    if state != Some("DONE") {
        return "running";
    }
    let info_field = |key: &str| tool_info.and_then(|info| info.get(key));
    let status = clean_text(info_field("status")).map(|status| status.to_uppercase());
    if js_truthy(info_field("error")) || status.as_deref().is_some_and(|status| matches!(status, "FAILED" | "ERROR" | "CANCELLED" | "CANCELED")) {
        "failed"
    } else {
        "completed"
    }
}

fn antigravity_tool_activity_text(name: &str, tool_info: Option<&Value>, workdir: Option<&str>) -> String {
    let parameters = tool_info.filter(|info| info.is_object()).and_then(|info| info.get("parameters")).filter(|parameters| parameters.is_object());
    let parameter = |keys: &[&str]| parameters.and_then(|parameters| first(parameters, keys));
    if name == "run_command" {
        return safe_command(parameter(&["CommandLine", "command_line", "command"])).unwrap_or_else(|| "명령".to_string());
    }
    if let Some(path) = clean_text(parameter(&["TargetFile", "target_file", "Path", "path", "FilePath", "file_path"])) {
        return format!("{name} · {}", compact_path(&path, workdir));
    }
    match clean_text(parameter(&["Query", "query"])) {
        Some(query) => format!("{name} · {}", safe_public_text(&query, 220)),
        None => name.to_string(),
    }
}

/// Claude 결과 이벤트의 사용량. 모델별 사용량(modelUsage)이 있으면 하위 에이전트까지 합친 총량을 쓴다.
fn normalized_claude_result_usage(result: &Value) -> Option<Value> {
    let main_loop_usage = normalized_usage(result.get("usage"), "claude", result.get("total_cost_usd"));
    let Some(per_model) = first(result, &["modelUsage", "model_usage"]).and_then(Value::as_object) else {
        return main_loop_usage;
    };
    let mut totals = [0.0f64; 4];
    let mut has_usage = false;
    let mut reported_cost_usd = nonnegative_number(result.get("total_cost_usd"));
    let mut summed_cost_usd = 0.0;
    let mut has_summed_cost = false;
    let mut reported_sonnet5_cost_usd = 0.0;
    let mut has_reported_sonnet5_cost = false;
    for model in js_keys(per_model) {
        let usage = &per_model[model];
        if !usage.is_object() {
            continue;
        }
        let fields = [
            ["inputTokens", "input_tokens"],
            ["outputTokens", "output_tokens"],
            ["cacheReadInputTokens", "cache_read_input_tokens"],
            ["cacheCreationInputTokens", "cache_creation_input_tokens"],
        ];
        for (total, keys) in totals.iter_mut().zip(fields) {
            if let Some(count) = token_count(first(usage, &keys)) {
                *total += count;
                has_usage = true;
            }
        }
        if let Some(model_cost) = nonnegative_number(first(usage, &["costUSD", "cost_usd"])) {
            summed_cost_usd += model_cost;
            has_summed_cost = true;
            if model.starts_with("claude-sonnet-5") {
                reported_sonnet5_cost_usd += model_cost;
                has_reported_sonnet5_cost = true;
            }
        }
    }
    if reported_cost_usd.is_none() && has_summed_cost {
        reported_cost_usd = Some(summed_cost_usd);
    }
    if !has_usage && reported_cost_usd.is_none() {
        return main_loop_usage;
    }
    let total = |index: usize| if has_usage { Value::from(totals[index]) } else { Value::Null };
    let main = |key: &str| main_loop_usage.as_ref().and_then(|usage| usage.get(key)).filter(|value| !value.is_null()).cloned();
    // modelUsage에는 캐시 생성 총량만 있어 5분·1시간 세부값을 본체 루프 값으로 채우지 않는다.
    Some(json!({
        "inputTokens": total(0),
        "outputTokens": total(1),
        "cachedInputTokens": total(2),
        "cacheWriteInputTokens": total(3),
        "cacheWrite5mInputTokens": null,
        "cacheWrite1hInputTokens": null,
        "reasoningOutputTokens": main("reasoningOutputTokens"),
        "serviceTier": main("serviceTier"),
        "speed": main("speed"),
        "inferenceGeo": main("inferenceGeo"),
        "reportedCostUsd": reported_cost_usd,
        "reportedSonnet5CostUsd": has_reported_sonnet5_cost.then_some(reported_sonnet5_cost_usd),
    }))
}

fn claude_tool_activity_text(tool: &Value, workdir: Option<&str>) -> String {
    if !matches!(tool, Value::Object(_) | Value::Array(_)) {
        return "도구 호출".to_string();
    }
    let name = clean_text(tool.get("name")).unwrap_or_else(|| "도구".to_string());
    // tool.input이 객체가 아니면 {}다. 글자·숫자의 칸을 읽어도 undefined라 get이 None인 것과 같다.
    let input = tool.get("input");
    let input_field = |keys: &[&str]| input.and_then(|input| first(input, keys));
    let lowered_name = name.to_lowercase();
    match lowered_name.as_str() {
        "bash" | "shell" | "terminal" => {
            return match safe_command(input_field(&["command"])) {
                Some(command) => format!("도구 · {name} · {command}"),
                None => format!("도구 · {name}"),
            };
        }
        "todowrite" => return claude_plan_activity_text(&name, input),
        "task" => {
            let detail = [clean_text(input_field(&["subagent_type", "subagentType"])), clean_text(input_field(&["description"]))]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            return if detail.is_empty() {
                format!("도구 · {name}")
            } else {
                format!("도구 · {name} · {}", safe_public_text(&detail, 180))
            };
        }
        _ => {}
    }
    if let Some(path) = clean_text(input_field(&["file_path", "notebook_path", "path", "cwd", "directory"])) {
        let header = format!("도구 · {name} · {}", compact_path(&path, workdir));
        return match claude_edit_stats(&lowered_name, input) {
            Some((additions, deletions)) => format!("{header}\n+{additions} -{deletions}"),
            None => header,
        };
    }
    if matches!(lowered_name.as_str(), "grep" | "glob" | "websearch")
        && let Some(query) = clean_text(input_field(&["pattern", "query"]))
    {
        return format!("도구 · {name} · {}", safe_public_text(&query, 180));
    }
    format!("도구 · {name}")
}

/// Claude CLI는 편집 통계를 주지 않아 도구 입력의 편집 전후 글자로 줄 수를 센다. Write는 이전 내용을 몰라 추가 줄만 센다.
fn claude_edit_stats(lowered_name: &str, input: Option<&Value>) -> Option<(usize, usize)> {
    fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
        value.and_then(|value| value.get(key)).and_then(Value::as_str)
    }
    let count_lines = |text: &str| if text.is_empty() { 0 } else { text.split('\n').count() };
    match lowered_name {
        "edit" => Some((count_lines(field(input, "new_string")?), count_lines(field(input, "old_string")?))),
        "multiedit" => {
            let edits = input.and_then(|input| input.get("edits")).and_then(Value::as_array).filter(|edits| !edits.is_empty())?;
            let mut totals = (0, 0);
            for edit in edits {
                let (old, new) = (field(Some(edit), "old_string")?, field(Some(edit), "new_string")?);
                totals = (totals.0 + count_lines(new), totals.1 + count_lines(old));
            }
            Some(totals)
        }
        "write" => Some((count_lines(field(input, "content")?), 0)),
        "notebookedit" => {
            let lines = count_lines(field(input, "new_source")?);
            let delete = clean_text(input.and_then(|input| input.get("edit_mode"))).as_deref() == Some("delete");
            Some(if delete { (0, lines) } else { (lines, 0) })
        }
        _ => None,
    }
}

fn claude_plan_activity_text(name: &str, input: Option<&Value>) -> String {
    let list = |key: &str| input.and_then(|input| input.get(key)).and_then(Value::as_array);
    let entries = list("todos").or_else(|| list("items")).map(Vec::as_slice).unwrap_or_default();
    let steps: Vec<String> = entries
        .iter()
        .filter(|entry| matches!(entry, Value::Object(_) | Value::Array(_)))
        .filter_map(|entry| {
            let text = clean_text(first(entry, &["content", "text", "title"]))?;
            let status = clean_text(entry.get("status")).map(|status| status.to_lowercase());
            let done = entry.get("completed") == Some(&Value::Bool(true)) || status.as_deref() == Some("completed");
            let marker = if done { "x" } else if status.as_deref() == Some("in_progress") { "~" } else { " " };
            Some(format!("[{marker}] {}", safe_public_text(&text, 160)))
        })
        .collect();
    if steps.is_empty() {
        return format!("도구 · {name}");
    }
    let done = steps.iter().filter(|step| step.starts_with("[x]")).count();
    let mut lines = vec![format!("도구 · {name} · {done}/{}단계", steps.len())];
    lines.extend(steps.iter().take(12).cloned());
    if steps.len() > 12 {
        lines.push(format!("외 {}개", steps.len() - 12));
    }
    lines.join("\n")
}

fn claude_tool_event_key(tool: &Value) -> Option<String> {
    clean_text(first(tool, &["id", "tool_use_id"]))
}

/// activity(kind, text, options). 키 순서는 kind·text·eventKey·status·preserveText·messageScoped다.
fn activity(kind: &str, text: impl Into<String>, event_key: Option<String>, status: &str) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("kind".into(), kind.into());
    value.insert("text".into(), text.into().into());
    value.insert("eventKey".into(), event_key.map_or(Value::Null, Value::from));
    value.insert("status".into(), status.into());
    value.insert("preserveText".into(), false.into());
    value.insert("messageScoped".into(), false.into());
    value
}

fn type_is(value: &Value, expected: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some(expected)
}

/// `value.a ?? value.b ?? …`. null이 아닌 첫 칸이고, 객체가 아닌 value의 칸은 없는 값이다.
fn first<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key).filter(|field| !field.is_null()))
}

/// 여러 식을 잇는 `??`. 없거나 null인 값을 건너뛴다.
fn coalesce<'a, const N: usize>(values: [Option<&'a Value>; N]) -> Option<&'a Value> {
    values.into_iter().flatten().find(|value| !value.is_null())
}

fn clean_str(text: &str) -> Option<String> {
    let text = js_trim(text);
    (!text.is_empty()).then(|| text.to_string())
}

/// UTF-16 길이가 limit을 넘으면 limit-1 단위까지 자르고 "…"를 붙인다.
fn truncated(text: String, limit: usize) -> String {
    if js_length(&text) <= limit { text } else { format!("{}…", utf16_prefix(&text, limit - 1)) }
}

fn reasoning_text(value: Option<&Value>) -> Option<String> {
    clean_text(value).map(|text| truncated(text, MAX_REASONING_LENGTH))
}

/// 템플릿 문자열 `${value}`. 없는 값은 "undefined"다.
pub(crate) fn template_text(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), js_string)
}

/// numericValue. 유한한 수, 또는 `/^-?\d+$/` 글자.
fn numeric_value(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|number| number.is_finite()),
        Value::String(text) => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then(|| string_to_number(text))
        }
        _ => None,
    }
}

fn safe_command(value: Option<&Value>) -> Option<String> {
    let command = replace_newline_runs(&clean_text(value)?);
    if contains_sensitive_command_data(&command) {
        return Some(format!("{} [민감 인자 숨김]", command_program(&command)));
    }
    Some(safe_public_text(&command, 280))
}

fn contains_sensitive_command_data(command: &str) -> bool {
    SENSITIVE_COMMAND_WORD.is_match(command) || SENSITIVE_COMMAND_KEY.is_match(command) || CREDENTIAL_URL.is_match(command)
}

/// 환경 변수 대입(`NAME=값`)을 건너뛴 첫 낱말.
fn command_program(command: &str) -> String {
    let assignment = |token: &str| {
        let bytes = token.as_bytes();
        let name_end = bytes.iter().position(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_')).unwrap_or(bytes.len());
        bytes.first().is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_') && bytes.get(name_end) == Some(&b'=')
    };
    js_trim(command)
        .split(is_js_whitespace)
        .filter(|token| !token.is_empty())
        .find(|token| !assignment(token))
        .map_or_else(|| "명령".to_string(), str::to_string)
}

/// `replace(/\s*\n\s*/g, " ↳ ")`. 줄바꿈을 하나라도 품은 공백 덩어리 전체가 " ↳ " 하나가 된다.
fn replace_newline_runs(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_js_whitespace) {
        output.push_str(&rest[..start]);
        let run_length = rest[start..].find(|character| !is_js_whitespace(character)).unwrap_or(rest.len() - start);
        let run = &rest[start..start + run_length];
        output.push_str(if run.contains('\n') { " ↳ " } else { run });
        rest = &rest[start + run_length..];
    }
    output.push_str(rest);
    output
}

fn safe_public_text(value: &str, limit: usize) -> String {
    let source = replace_newline_runs(&value.replace("\r\n", "\n"));
    truncated(js_trim(&source).to_string(), limit)
}

/// 작업 폴더 안 경로는 상대 경로로, 너무 긴 경로는 끝 세 마디만 남긴다.
fn compact_path(value: &str, workdir: Option<&str>) -> String {
    let base = workdir.unwrap_or("");
    if !base.is_empty() {
        let relative = if value.starts_with('/') { relative_path(base, value) } else { value.to_string() };
        let normalized = relative.replace('\\', "/");
        if !normalized.is_empty() && normalized != ".." && !normalized.starts_with("../") && !relative.starts_with('/') {
            return normalized;
        }
    }
    let path = value.replace('\\', "/");
    if js_length(&path) <= 96 {
        return path;
    }
    let components: Vec<&str> = path.split('/').filter(|component| !component.is_empty()).collect();
    if components.len() > 3 {
        format!("…/{}", components[components.len() - 3..].join("/"))
    } else {
        format!("…{}", utf16_suffix(&path, 92))
    }
}

fn file_change_label(value: Option<&Value>) -> &'static str {
    match clean_text(value).map(|kind| kind.to_lowercase()).as_deref() {
        Some("add" | "create") => "추가",
        Some("delete" | "remove") => "삭제",
        Some("move" | "rename") => "이동",
        _ => "수정",
    }
}

/// failureText. 글자는 다듬고, 객체는 message·detail·code·type·error 칸을 차례로 읽어 중복 없이 줄로 잇는다.
fn failure_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => clean_str(text),
        value @ (Value::Object(_) | Value::Array(_)) => {
            let mut parts: Vec<String> = Vec::new();
            for key in ["message", "detail", "code", "type", "error"] {
                if let Some(part) = failure_text(value.get(key))
                    && !parts.contains(&part)
                {
                    parts.push(part);
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

/// 정규화한 사용량의 숫자 칸. 순서가 응답 JSON의 키 순서다.
pub const USAGE_COUNT_FIELDS: [&str; 7] = [
    "inputTokens",
    "outputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "cacheWrite5mInputTokens",
    "cacheWrite1hInputTokens",
    "reasoningOutputTokens",
];

/// 정규화한 사용량의 글자 칸.
pub const USAGE_TEXT_FIELDS: [&str; 3] = ["serviceTier", "speed", "inferenceGeo"];

/// normalizedUsage. 토큰이나 비용이 하나라도 있어야 사용량으로 친다.
pub fn normalized_usage(value: Option<&Value>, backend: &str, reported_cost_usd: Option<&Value>) -> Option<Value> {
    let value = value.filter(|value| value.is_object())?;
    let cache_creation = value.get("cache_creation");
    let codex = backend == "codex";
    let counts = [
        token_count(value.get("input_tokens")),
        token_count(value.get("output_tokens")),
        token_count(value.get(if codex { "cached_input_tokens" } else { "cache_read_input_tokens" })),
        token_count(value.get(if codex { "cache_write_input_tokens" } else { "cache_creation_input_tokens" })),
        token_count(cache_creation.and_then(|cache| cache.get("ephemeral_5m_input_tokens"))),
        token_count(cache_creation.and_then(|cache| cache.get("ephemeral_1h_input_tokens"))),
        token_count(value.get("reasoning_output_tokens")),
    ];
    let reported = nonnegative_number(reported_cost_usd);
    if counts.iter().all(Option::is_none) && reported.is_none() {
        return None;
    }
    let mut usage = Map::new();
    for (field, count) in USAGE_COUNT_FIELDS.iter().zip(counts) {
        usage.insert((*field).into(), count.map_or(Value::Null, Value::from));
    }
    for (field, source) in USAGE_TEXT_FIELDS.iter().zip(["service_tier", "speed", "inference_geo"]) {
        usage.insert((*field).into(), clean_text(value.get(source)).map_or(Value::Null, Value::from));
    }
    usage.insert("reportedCostUsd".into(), reported.map_or(Value::Null, Value::from));
    Some(Value::Object(usage))
}

/// claudeSessionUsage. 중단으로 결과 이벤트를 받지 못한 턴의 사용량을 세션 기록 한 줄에서 되살린다.
pub fn claude_session_usage(line: &str) -> Option<Value> {
    claude_message_usage(&parse_js_json(line).ok()?)
}

pub fn claude_message_usage(object: &Value) -> Option<Value> {
    object.as_object()?;
    normalized_usage(object.get("message").and_then(|message| message.get("usage")), "claude", None)
}

/// 응답 메시지에 남은 실제 모델 ID. 하위 에이전트 응답과 CLI가 만든 `<synthetic>` 메시지는 이 턴의 모델이 아니다.
pub fn claude_message_model(object: &Value) -> Option<String> {
    object.as_object()?;
    if object.get("isSidechain") == Some(&json!(true)) || clean_text(object.get("parent_tool_use_id")).is_some() {
        return None;
    }
    clean_text(object.get("message").and_then(|message| message.get("model"))).filter(|model| model != "<synthetic>")
}

/// 같은 응답이 여러 줄로 남으므로 합산 중복을 거를 열쇠. 응답 식별자가 없으면 줄 식별자를 쓴다.
pub fn claude_session_usage_key(object: &Value) -> Option<String> {
    object.as_object()?;
    clean_text(object.get("message").and_then(|message| message.get("id"))).or_else(|| clean_text(object.get("uuid")))
}

/// Codex rollout token_count 한 줄의 요청 한 번 사용량. 누적 총량은 압축으로 줄어들어 쓰지 않는다.
pub fn codex_request_usage(object: &Value) -> Option<Value> {
    object.as_object()?;
    let payload = object.get("payload");
    if object.get("type") != Some(&json!("event_msg"))
        || payload.and_then(|payload| payload.get("type")) != Some(&json!("token_count"))
    {
        return None;
    }
    normalized_usage(payload.and_then(|payload| payload.get("info")).and_then(|info| info.get("last_token_usage")), "codex", None)
}

/// tokenCount. 0 이상의 수나 숫자 글자를 소수점 아래를 버린 정수로 읽는다.
fn token_count(value: Option<&Value>) -> Option<f64> {
    nonnegative_number(value).map(f64::trunc)
}

/// 0 이상의 유한한 수, 또는 `/^\d+(?:\.\d+)?$/`에 맞는 글자.
fn nonnegative_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|number| number.is_finite() && *number >= 0.0),
        Value::String(text) if decimal_text(text) => Some(string_to_number(text)),
        _ => None,
    }
}

fn decimal_text(text: &str) -> bool {
    let (whole, fraction) = text.split_once('.').map_or((text, None), |(whole, fraction)| (whole, Some(fraction)));
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(whole) && fraction.is_none_or(digits)
}

/// cleanText. 글자를 앞뒤 공백 없이 돌려주고, 비었거나 글자가 아니면 None이다.
pub fn clean_text(value: Option<&Value>) -> Option<String> {
    let text = js_trim(value?.as_str()?);
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_usage_reads_numbers_and_decimal_text() {
        let usage = normalized_usage(
            Some(&json!({
                "input_tokens": "12.9", "output_tokens": 3.7, "cache_read_input_tokens": "007",
                "cache_creation_input_tokens": -1, "cache_creation": { "ephemeral_5m_input_tokens": "1e3", "ephemeral_1h_input_tokens": " 4" },
                "reasoning_output_tokens": "٣", "service_tier": "  fast ", "speed": "", "inference_geo": 5,
            })),
            "claude",
            None,
        )
        .unwrap();
        assert_eq!(
            usage,
            json!({
                "inputTokens": 12.0, "outputTokens": 3.0, "cachedInputTokens": 7.0, "cacheWriteInputTokens": null,
                "cacheWrite5mInputTokens": null, "cacheWrite1hInputTokens": null, "reasoningOutputTokens": null,
                "serviceTier": "fast", "speed": null, "inferenceGeo": null, "reportedCostUsd": null,
            })
        );
        assert_eq!(normalized_usage(Some(&json!({ "service_tier": "fast" })), "codex", None), None);
        assert_eq!(normalized_usage(Some(&json!([1])), "codex", None), None);
        assert!(normalized_usage(Some(&json!({})), "claude", Some(&json!(0))).is_some());
    }

    #[test]
    fn claude_model_skips_sidechains_and_synthetic() {
        let line = |extra: Value| {
            let mut object = json!({ "type": "assistant", "message": { "model": " claude-opus-5-5 ", "id": " " }, "uuid": "u-1" });
            object.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            object
        };
        assert_eq!(claude_message_model(&line(json!({}))).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_message_model(&line(json!({ "isSidechain": true }))), None);
        assert_eq!(claude_message_model(&line(json!({ "isSidechain": "true" }))).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_message_model(&line(json!({ "parent_tool_use_id": "toolu_1" }))), None);
        assert_eq!(claude_message_model(&json!({ "message": { "model": "<synthetic>" } })), None);
        assert_eq!(claude_session_usage_key(&line(json!({}))).as_deref(), Some("u-1"));
        assert_eq!(claude_session_usage(r#"{"message":{"usage":{"input_tokens":1}}}"#).unwrap()["inputTokens"], json!(1.0));
        assert_eq!(claude_session_usage("깨진 줄"), None);
    }

    #[test]
    fn codex_request_usage_needs_token_count_event() {
        let object = json!({ "type": "event_msg", "payload": { "type": "token_count", "info": {
            "last_token_usage": { "input_tokens": 10, "cached_input_tokens": 4, "cache_write_input_tokens": 1, "cache_read_input_tokens": 99 },
        } } });
        let usage = codex_request_usage(&object).unwrap();
        assert_eq!((usage["inputTokens"].clone(), usage["cachedInputTokens"].clone(), usage["cacheWriteInputTokens"].clone()), (json!(10.0), json!(4.0), json!(1.0)));
        assert_eq!(codex_request_usage(&json!({ "type": "event_msg", "payload": { "type": "task_started" } })), None);
        assert_eq!(codex_request_usage(&json!("event_msg")), None);
    }

    // 아래 기대값은 같은 입력을 Node agent-event-parser.mjs에 넣어 얻었다.

    fn parse(event: Value, backend: &str, workdir: Option<&str>) -> Result<Option<Value>, String> {
        parse_agent_event(&event.to_string(), backend, workdir)
    }

    #[test]
    fn null_content_item_is_the_node_type_error() {
        let line = json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "a" }, null] } });
        assert_eq!(parse(line, "claude", None), Err("Cannot read properties of null (reading 'type')".to_string()));
        let line = json!({ "type": "user", "message": { "content": [null] } });
        assert_eq!(parse(line, "claude", None), Err("Cannot read properties of null (reading 'type')".to_string()));
    }

    #[test]
    fn commands_follow_non_unicode_regex_rules() {
        let command = |command: &str| {
            let event = parse(json!({ "type": "item.completed", "item": { "id": "c", "type": "command_execution", "command": command, "exit_code": "-0" } }), "codex", None);
            let activity = event.unwrap().unwrap()["activity"].clone();
            (activity["text"].as_str().unwrap().to_string(), activity["status"].as_str().unwrap().to_string())
        };
        assert_eq!(command("ſecret run"), ("ſecret run".to_string(), "completed".to_string()));
        assert_eq!(command("echo 한sk-12345678").0, "echo [민감 인자 숨김]");
        assert_eq!(command("echo _sk-12345678").0, "echo _sk-12345678");
        assert_eq!(command("A=1 B=2 git push https://u:p@h/x").0, "git [민감 인자 숨김]");
        assert_eq!(command("  ls\u{2028}-la \n\u{a0} x").0, "ls\u{2028}-la ↳ x");
        // Node는 잘린 높은 대리 문자를 그대로 두고, Rust는 node-pg 저장 결과처럼 U+FFFD로 둔다.
        assert_eq!(command(&format!("{}😀y", "x".repeat(278))).0, format!("{}\u{fffd}…", "x".repeat(278)));
    }

    #[test]
    fn collaboration_wait_reads_states_in_object_keys_order() {
        let line = json!({ "type": "item.completed", "item": { "id": "k", "type": "collab_tool_call", "tool": "wait", "agents_states": {
            "b": { "status": "completed", "message": "끝" }, "10": { "status": "Running" }, "2": { "status": "not-found" },
        } } });
        let activities = parse(line, "codex", None).unwrap().unwrap()["activities"].clone();
        let summary: Vec<(String, String, String)> = activities
            .as_array()
            .unwrap()
            .iter()
            .map(|activity| (js_string(&activity["eventKey"]), js_string(&activity["status"]), js_string(&activity["text"])))
            .collect();
        assert_eq!(
            summary,
            [
                ("collaboration:k:2".into(), "failed".into(), "협업 검토에 실패했습니다.".into()),
                ("collaboration:k:b".into(), "completed".into(), "끝".into()),
            ]
        );
    }

    #[test]
    fn response_blocks_follow_regex_last_index_and_multiline_rules() {
        let decoded = |text: &str| decode_agent_response(text);
        // 앞 마커가 줄바꿈까지 먹어 다음 마커의 (?:^|\n)이 맞지 않으므로 블록이 아니다.
        assert_eq!(decoded("본문\n[OFFICE_SOURCES]\n[OFFICE_SOURCES]\n[]")["text"], "본문\n[OFFICE_SOURCES]\n[OFFICE_SOURCES]\n[]");
        let need_input = decoded("a\r[NEED_INPUT]\r\nb\n\t[NEED_INPUT] \u{2028}끝");
        assert_eq!((need_input["text"].clone(), need_input["needsInput"].clone()), (json!("a\r[NEED_INPUT]\r\nb\n\u{2028}끝"), json!(true)));
        let fenced = decoded("답\n[OFFICE_WIKI_PROPOSALS]\n```JSON  \n[{\"pageKey\":\"a-1\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"b\",\"approvalTier\":\"peer\"}]  \n```");
        assert_eq!(fenced["proposals"][0]["pageKey"], "a-1");
        let stranded = decoded("답\n[OFFICE_SOURCES]\n[ {\"kind\":\"file\"}]\n감사합니다");
        assert_eq!(stranded["sourceError"], "응답 근거 블록은 응답 맨 끝에 두고 뒤에 아무것도 쓰지 마십시오.");
    }

    #[test]
    fn lone_surrogate_escapes_do_not_drop_the_event() {
        let event = parse_agent_event(r#"{"type":"result","result":"a\ud83d","session_id":"s"}"#, "claude", None).unwrap();
        assert_eq!(event, Some(json!({ "sessionID": "s", "usage": null, "responseText": "a\u{fffd}" })));
    }

    #[test]
    fn file_change_paths_are_compacted_like_node() {
        let line = json!({ "type": "item.completed", "item": { "id": "f", "type": "file_change", "changes": [
            { "path": "/Users/example/office/src/a.rs", "kind": "ADD" }, { "path": "/Users/example/office" },
            { "path": format!("/a/{}", "가".repeat(100)) }, { "file": format!("/x/{}z", "y/".repeat(50)) },
        ] } });
        let event = parse(line, "codex", Some("/Users/example/office")).unwrap().unwrap();
        let expected = format!(
            "파일 4개를 편집했습니다\n추가 src/a.rs\n수정 /Users/example/office\n수정 …{}\n수정 …/y/y/z",
            "가".repeat(92)
        );
        assert_eq!(event["activity"]["text"], expected);
    }

    #[test]
    fn model_usage_sums_in_object_keys_order() {
        let line = json!({ "type": "result", "modelUsage": {
            "claude-sonnet-5-x": { "inputTokens": "10", "costUSD": 0.1 }, "10": { "input_tokens": 1, "cost_usd": "0.2" },
            "claude-sonnet-5": { "outputTokens": 5, "cacheReadInputTokens": 2.9, "cost_usd": 0.3 },
        }, "usage": { "reasoning_output_tokens": 3, "service_tier": "p" } });
        let usage = parse(line, "claude", None).unwrap().unwrap()["usage"].clone();
        assert_eq!(
            String::from_utf8(crate::js::to_js_json(&usage).unwrap()).unwrap(),
            r#"{"inputTokens":11,"outputTokens":5,"cachedInputTokens":2,"cacheWriteInputTokens":0,"cacheWrite5mInputTokens":null,"cacheWrite1hInputTokens":null,"reasoningOutputTokens":3,"serviceTier":"p","speed":null,"inferenceGeo":null,"reportedCostUsd":0.6000000000000001,"reportedSonnet5CostUsd":0.4}"#
        );
    }
}
