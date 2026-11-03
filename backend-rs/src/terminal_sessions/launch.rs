// 터미널 실행 명세(인자·환경), Antigravity 단계 해독, Claude 원본의 마지막 답변 읽기를 맡는다.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

use crate::agent_runtime::launch::{
    LaunchPaths, RuntimeCharacter, antigravity_permission_arguments, execution_environment, prepend_path_entries, quoted,
};
use crate::antigravity_local_state::{
    SqliteValue, StepMetadata, nested_fields, parse_step_metadata, protobuf_fields, utf8_field,
};
use crate::antigravity_model_selection::antigravity_model_arguments;
use crate::antigravity_reasoning::antigravity_step_payload_reasoning;
use crate::js::{encode_uri_component, js_string_or_empty, js_trim, now_milliseconds, number_to_string, parse_js_json};
use crate::local_embedding::Environment;
use crate::structured_turn_result::identity_prompt_with_structured_result;

/// 터미널 훅 실행 파일 이름. 상태줄과 두 훅이 같은 명령을 쓴다.
pub(crate) const TERMINAL_HOOK_COMMAND: &str = "officestra-terminal-hook";
const LAST_MESSAGE_WINDOW: u64 = 4 * 1024 * 1024;

/// nonnegativeCost. 숫자형이고 유한하며 0 이상일 때만 값이다.
pub(crate) fn nonnegative_cost(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// terminalHookSettings. 상태줄 명령은 Claude Code가 세션 누적 비용을 넘겨 주는 유일한 대화형 경로다.
pub(crate) fn terminal_hook_settings(character: &RuntimeCharacter) -> String {
    let hook = json!({ "hooks": [{ "type": "command", "command": TERMINAL_HOOK_COMMAND }] });
    let settings = json!({
        "fastMode": character.fast_mode,
        "hooks": { "UserPromptSubmit": [hook.clone()], "Stop": [hook] },
        "statusLine": { "type": "command", "command": TERMINAL_HOOK_COMMAND },
    });
    serde_json::to_string(&settings).unwrap_or_default()
}

/// terminalArguments. print 모드 플래그 없이 원래 대화형 CLI를 실행한다.
/// Codex notify는 Node처럼 `[node, 훅 스크립트]`가 아니라 Rust 훅 실행 파일 하나다.
pub(crate) fn terminal_arguments(
    character: &RuntimeCharacter,
    previous_session_id: Option<&str>,
    workdir: Option<&str>,
    hook_path: &str,
) -> Result<Vec<String>, String> {
    let previous = previous_session_id.filter(|id| !id.is_empty());
    let identity = || identity_prompt_with_structured_result(&character.identity_prompt, character.id.as_deref());
    match character.backend.as_str() {
        "claude" => {
            let mut arguments: Vec<String> = vec![
                "--effort".into(),
                character.effort.clone(),
                "--permission-mode".into(),
                character.permission.clone(),
                "--settings".into(),
                terminal_hook_settings(character),
            ];
            if let Some(model) = character.configured_model() {
                arguments.extend(["--model".into(), model.to_string()]);
            }
            arguments.extend(["--append-system-prompt".into(), identity()]);
            if let Some(previous) = previous {
                arguments.extend(["--resume".into(), previous.to_string()]);
            }
            Ok(arguments)
        }
        "codex" => {
            let mut arguments: Vec<String> = match previous {
                Some(previous) => vec!["resume".into(), previous.to_string()],
                None => Vec::new(),
            };
            if let Some(model) = character.configured_model() {
                arguments.extend(["-c".into(), format!("model={}", quoted(model))]);
            }
            let tier = if character.fast_mode { "fast" } else { "default" };
            let notify = serde_json::to_string(&[hook_path]).unwrap_or_default();
            arguments.extend([
                "-c".into(),
                format!("model_reasoning_effort={}", quoted(&character.effort)),
                "-c".into(),
                "features.fast_mode=true".into(),
                "-c".into(),
                format!("service_tier={}", quoted(tier)),
                "-c".into(),
                "model_reasoning_summary=\"detailed\"".into(),
                "-c".into(),
                "show_raw_agent_reasoning=true".into(),
                "-c".into(),
                format!("developer_instructions={}", quoted(&identity())),
                "-s".into(),
                character.permission.clone(),
                "-c".into(),
                format!("notify={notify}"),
            ]);
            Ok(arguments)
        }
        "antigravity" => {
            let mut arguments = antigravity_model_arguments(character.model.as_deref().unwrap_or(""), &character.effort);
            if let Some(previous) = previous {
                arguments.extend(["--conversation".into(), previous.to_string()]);
            }
            if let Some(workdir) = workdir.filter(|workdir| !workdir.is_empty()) {
                arguments.extend(["--add-dir".into(), workdir.to_string()]);
            }
            arguments.extend(antigravity_permission_arguments(&character.permission)?);
            Ok(arguments)
        }
        backend => Err(format!("지원하지 않는 직원 백엔드입니다: {backend}")),
    }
}

fn parent_directory(path: &str) -> String {
    Path::new(path).parent().map_or(".".into(), |parent| {
        let text = parent.to_string_lossy();
        if text.is_empty() { ".".into() } else { text.into_owned() }
    })
}

/// terminalEnvironment. 실행 환경에 터미널 표시·훅 주소를 더하고 훅 폴더와 Node 폴더를 PATH 맨 앞에 둔다.
pub(crate) fn terminal_environment(
    character: &RuntimeCharacter,
    base: &Environment,
    workdir: &str,
    character_id: &str,
    port: u16,
    paths: &LaunchPaths,
    hook_path: &str,
) -> Environment {
    let mut environment = execution_environment(character, base, Some(workdir), paths);
    let language = base.get("LANG").filter(|value| !value.is_empty()).cloned().unwrap_or_else(|| "en_US.UTF-8".into());
    environment.insert("TERM".into(), "xterm-256color".into());
    environment.insert("COLORTERM".into(), "truecolor".into());
    environment.insert("LANG".into(), language);
    environment.insert(
        "OFFICESTRA_TERMINAL_EVENTS_URL".into(),
        format!("http://127.0.0.1:{port}/api/terminal-sessions/{}/events", encode_uri_component(character_id)),
    );
    environment.insert("OFFICESTRA_TERMINAL_CHARACTER_ID".into(), character_id.to_string());
    environment.insert("OFFICESTRA_TOOL_MODE".into(), "terminal".into());
    environment.insert("OFFICESTRA_TOOL_EVENTS_URL".into(), format!("http://127.0.0.1:{port}/api/developer-tool-events"));
    let hook_directory = parent_directory(hook_path);
    let node_directory = paths.node_executable.as_deref().map(parent_directory);
    let path = prepend_path_entries(
        environment.get("PATH").map(String::as_str).unwrap_or(""),
        &[Some(hook_directory.as_str()), node_directory.as_deref()],
    );
    environment.insert("PATH".into(), path);
    environment
}

/// Antigravity steps 표 한 행. 바이트 열은 읽을 때 한 번만 꺼낸다(숫자 값이면 None).
pub(crate) struct StepRow {
    pub(crate) index: i64,
    /// 템플릿 문자열 `${row.idx}`.
    pub(crate) label: String,
    pub(crate) step_type: f64,
    pub(crate) status: f64,
    pub(crate) metadata: Option<Vec<u8>>,
    pub(crate) payload: Option<Vec<u8>>,
}

impl StepRow {
    pub(crate) fn new(
        idx: SqliteValue,
        step_type: SqliteValue,
        status: SqliteValue,
        metadata: SqliteValue,
        payload: SqliteValue,
    ) -> Self {
        let label = match &idx {
            SqliteValue::Null => "null".into(),
            SqliteValue::Number(value) => number_to_string(*value),
            SqliteValue::Text(text) => text.clone(),
            SqliteValue::Blob(bytes) => bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(","),
        };
        Self {
            index: idx.number() as i64,
            label,
            step_type: step_type.number(),
            status: status.number(),
            metadata: metadata.buffer(),
            payload: payload.buffer(),
        }
    }
}

/// parseAntigravityTerminalStep 결과.
#[derive(Debug, PartialEq)]
pub(crate) enum TerminalStep {
    User(String),
    Assistant { text: String, metadata: Option<StepMetadata>, activities: Vec<Value> },
    Activity { activities: Vec<Value>, metadata: Option<StepMetadata> },
}

/// parseAntigravityTerminalStep. 완료(3) 단계만 읽고, 깨진 값은 Node의 catch처럼 None이다.
pub(crate) fn parse_antigravity_terminal_step(row: &StepRow) -> Option<TerminalStep> {
    if row.status != 3.0 {
        return None;
    }
    let payload = row.payload.as_deref()?;
    let fields = protobuf_fields(payload).ok()?;
    let metadata = || row.metadata.as_deref().and_then(parse_step_metadata);
    if row.step_type == 14.0 {
        let text = utf8_field(&nested_fields(&fields, 19).ok()?, 2);
        return (!text.is_empty()).then_some(TerminalStep::User(text));
    }
    if row.step_type == 15.0 {
        let text = utf8_field(&nested_fields(&fields, 20).ok()?, 1);
        let activities = antigravity_step_payload_reasoning(payload)
            .map(|reasoning| {
                vec![json!({ "kind": "thinking", "text": reasoning, "eventKey": format!("step:{}:thinking", row.label) })]
            })
            .unwrap_or_default();
        return Some(if text.is_empty() {
            TerminalStep::Activity { activities, metadata: metadata() }
        } else {
            TerminalStep::Assistant { text, metadata: metadata(), activities }
        });
    }
    if row.step_type == 132.0 {
        return Some(TerminalStep::Activity {
            activities: vec![json!({ "kind": "tool", "text": "도구 결과 수신", "eventKey": format!("step:{}:tool", row.label) })],
            metadata: None,
        });
    }
    None
}

/// dateFromEpoch. 초 단위 값이 양수면 그 시각(밀리초는 Date처럼 버림), 아니면 지금이다.
pub(crate) fn date_from_epoch(seconds: f64) -> f64 {
    if seconds.is_finite() && seconds > 0.0 { (seconds * 1_000.0).trunc() } else { now_milliseconds() }
}

/// lastClaudeAssistantMessage. 원본 끝 4MiB에서 가장 마지막 assistant 글 조각을 찾는다. 읽기 오류는 빈 문자열이다.
pub(crate) fn last_claude_assistant_message(path: Option<&str>) -> String {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return String::new();
    };
    read_last_assistant_message(path).unwrap_or_default()
}

fn read_last_assistant_message(path: &str) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let length = size.min(LAST_MESSAGE_WINDOW);
    file.seek(SeekFrom::Start(size - length))?;
    let mut buffer = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut buffer)?;
    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if size > length {
        lines.remove(0);
    }
    for line in lines.iter().rev() {
        let Ok(value) = parse_js_json(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let content = value
            .get("message")
            .and_then(|message| message.get("content"))
            .filter(|content| !content.is_null())
            .or_else(|| value.get("content"));
        let text = content
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .map(|item| js_trim(&js_string_or_empty(item.get("text"))).to_string())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return Ok(text);
        }
    }
    Ok(String::new())
}
