// 직원 CLI 실행 인자와 자식 환경 변수를 만든다. Node agent-runtime.mjs의 buildArguments·executionEnvironment 묶음과 같다.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::attachments::StagedAttachment;
use crate::antigravity_model_selection::antigravity_model_arguments;
use crate::antigravity_playwright::antigravity_playwright_environment;
use crate::developer_tool_events::developer_tool_environment;
use crate::js::{js_trim, to_js_json};
use crate::structured_turn_result::{STRUCTURED_RESULT_ENV, identity_prompt_with_structured_result, structured_turn_result_path};

pub type Environment = BTreeMap<String, String>;

/// 실행에 쓰는 직원 설정. Node prepareTurn이 characters 행에서 읽는 값과 같다.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCharacter {
    /// Node 시험처럼 id 없는 직원도 인자를 만들 수 있어 Option이다. 실제 업무에서는 언제나 있다.
    pub id: Option<String>,
    pub name: String,
    pub seat: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    /// characters.auto_compact_percent. 없으면 Node Number(undefined)처럼 기본값 90을 쓴다.
    pub auto_compact_percent: Option<f64>,
    pub permission: String,
    pub identity_prompt: String,
    pub config: Value,
    /// Node character.localProfile. resolveLocalCharacter가 고른 주소·추론까지 덮어쓴 4090 정의의 JSON이다(턴 스냅숏과 같다).
    /// 완료 처리는 있는지만 보고, 실행할 때 다시 정규화한다. 클라우드 직원은 None이다.
    pub local_profile: Option<Value>,
}

impl RuntimeCharacter {
    /// Node `if (character.model)`. 빈 문자열은 없는 값이다.
    pub(crate) fn configured_model(&self) -> Option<&str> {
        self.model.as_deref().filter(|model| !model.is_empty())
    }
}

/// Node 프로세스 자체에서 얻던 경로들. Rust 백엔드는 설정으로 받는다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchPaths {
    /// officestra-result가 있는 폴더(Node structuredResultToolDirectory).
    pub result_tool_directory: String,
    /// Node 실행 파일(Node process.execPath). officestra-result와 Playwright 드라이버가 쓴다. 모르면 PATH에 넣지 않는다.
    pub node_executable: Option<String>,
    /// Antigravity용 Playwright 1.57 드라이버 폴더.
    pub playwright_driver_directory: String,
    /// 구조화 결과 경로를 실행기마다 나누는 값(Node process.pid).
    pub runtime_id: String,
}

/// normalizeAutoCompactPercent. 값이 없거나 유한수가 아니면 90, 아니면 반올림해 20~maximum으로 자른다.
pub fn normalize_auto_compact_percent(value: Option<f64>, maximum: f64) -> i64 {
    let Some(percent) = value.filter(|percent| percent.is_finite()) else {
        return 90;
    };
    // Math.round와 f64::round는 음수 .5에서만 다르고, 그 값은 어차피 20으로 올라간다.
    percent.round().clamp(20.0, maximum) as i64
}

/// configuredIdentityPrompt.
pub fn configured_identity_prompt(character: &RuntimeCharacter) -> String {
    identity_prompt_with_structured_result(&character.identity_prompt, character.id.as_deref())
}

/// JSON.stringify(String(text)). 터미널 실행 인자(quotedConfig)도 같은 규칙이다.
pub(crate) fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn fast_mode_settings(character: &RuntimeCharacter) -> String {
    String::from_utf8(to_js_json(&json!({ "fastMode": character.fast_mode })).unwrap_or_default()).unwrap_or_default()
}

/// buildArguments. previous_session_id는 빈 문자열이면 새 세션이다.
pub fn build_arguments(
    character: &RuntimeCharacter,
    prompt: &str,
    previous_session_id: Option<&str>,
    attachments: &[StagedAttachment],
    workdir: Option<&str>,
) -> Result<Vec<String>, String> {
    let previous = previous_session_id.filter(|id| !id.is_empty());
    match character.backend.as_str() {
        "codex" => Ok(codex_arguments(character, prompt, previous, attachments)),
        "claude" => Ok(claude_arguments(character, prompt, previous)),
        "antigravity" => antigravity_arguments(character, prompt, previous, workdir),
        backend => Err(format!("지원하지 않는 직원 백엔드입니다: {backend}")),
    }
}

fn codex_arguments(character: &RuntimeCharacter, prompt: &str, previous: Option<&str>, attachments: &[StagedAttachment]) -> Vec<String> {
    let mut arguments: Vec<String> = vec!["exec".into()];
    match previous {
        Some(session) => arguments.extend(["resume".into(), session.to_string(), "--json".into()]),
        None => arguments.extend(["--json".into(), "--skip-git-repo-check".into()]),
    }
    arguments.extend(codex_config_overrides(character));
    if previous.is_some() {
        arguments.extend(["-c".into(), format!("sandbox_mode=\"{}\"", character.permission)]);
    } else {
        arguments.extend(["-s".into(), character.permission.clone()]);
    }
    for attachment in attachments.iter().filter(|attachment| attachment.is_codex_image) {
        arguments.extend(["-i".into(), attachment.path.clone()]);
    }
    arguments.push(prompt.to_string());
    arguments
}

/// exec와 app-server가 함께 쓰는 Codex 설정 덮어쓰기(-c).
fn codex_config_overrides(character: &RuntimeCharacter) -> Vec<String> {
    let mut arguments: Vec<String> = Vec::new();
    if let Some(model) = character.configured_model() {
        arguments.extend(["-c".into(), format!("model=\"{model}\"")]);
    }
    arguments.extend([
        "-c".into(),
        format!("model_reasoning_effort=\"{}\"", character.effort),
        "-c".into(),
        "features.fast_mode=true".into(),
        "-c".into(),
        format!("service_tier=\"{}\"", if character.fast_mode { "fast" } else { "default" }),
        "-c".into(),
        "model_reasoning_summary=\"detailed\"".into(),
        "-c".into(),
        "show_raw_agent_reasoning=true".into(),
        "-c".into(),
        format!("developer_instructions={}", quoted(&configured_identity_prompt(character))),
    ]);
    arguments
}

/// GUI Codex 턴의 app-server 실행 인자. 실행 중 끼어들기(turn/steer)를 받으려고 exec 대신 쓴다.
/// 작업 권한·질문·첨부 이미지는 JSON-RPC 요청에 담는다.
pub fn codex_app_server_arguments(character: &RuntimeCharacter) -> Vec<String> {
    let mut arguments: Vec<String> = vec!["app-server".into()];
    arguments.extend(codex_config_overrides(character));
    arguments.push("--stdio".into());
    arguments
}

fn claude_common_arguments(character: &RuntimeCharacter, head: Vec<String>, previous: Option<&str>) -> Vec<String> {
    let mut arguments = head;
    arguments.extend([
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--prompt-suggestions".into(),
        "true".into(),
        "--settings".into(),
        fast_mode_settings(character),
        "--effort".into(),
        character.effort.clone(),
        "--permission-mode".into(),
        character.permission.clone(),
    ]);
    if let Some(model) = character.configured_model() {
        arguments.extend(["--model".into(), model.to_string()]);
    }
    arguments.extend(["--append-system-prompt".into(), configured_identity_prompt(character)]);
    // 실행 전에 같은 transcript inode를 현재 작업 폴더에 연결하므로 JSONL을 복제하지 않고 이어 쓴다.
    if let Some(session) = previous {
        arguments.extend(["--resume".into(), session.to_string()]);
    }
    arguments
}

fn claude_arguments(character: &RuntimeCharacter, prompt: &str, previous: Option<&str>) -> Vec<String> {
    let head = vec!["-p".into(), prompt.to_string(), "--output-format".into(), "stream-json".into()];
    claude_common_arguments(character, head, previous)
}

/// claudePersistentArguments. 프롬프트는 인자가 아니라 stream-json 표준 입력으로 보낸다.
pub fn claude_persistent_arguments(character: &RuntimeCharacter, previous_session_id: Option<&str>) -> Vec<String> {
    let head = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        // 끼어든 메시지를 받아 간 순간을 알아야 같은 턴에 합쳐졌는지, 다음 턴으로 처리됐는지 가린다.
        "--replay-user-messages".into(),
    ];
    claude_common_arguments(character, head, previous_session_id.filter(|id| !id.is_empty()))
}

fn antigravity_arguments(
    character: &RuntimeCharacter,
    prompt: &str,
    previous: Option<&str>,
    workdir: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut arguments: Vec<String> = vec![
        "-p".into(),
        antigravity_execution_prompt(character, prompt, workdir),
        "--output-format".into(),
        "stream-json".into(),
        "--print-timeout".into(),
        "24h".into(),
    ];
    arguments.extend(antigravity_model_arguments(character.model.as_deref().unwrap_or(""), &character.effort));
    if let Some(conversation) = previous {
        arguments.extend(["--conversation".into(), conversation.to_string()]);
    }
    // Headless 모드는 --add-dir로 등록한 폴더만 읽기 권한을 자동 승인한다.
    if let Some(workdir) = workdir.filter(|workdir| !workdir.is_empty()) {
        arguments.extend(["--add-dir".into(), workdir.to_string()]);
    }
    arguments.extend(antigravity_permission_arguments(&character.permission)?);
    Ok(arguments)
}

/// antigravityPermissionArguments. GUI 실행과 터미널 실행이 같은 권한 인자를 쓴다.
pub(crate) fn antigravity_permission_arguments(permission: &str) -> Result<Vec<String>, String> {
    let arguments: &[&str] = match permission {
        "plan" => &["--mode", "plan"],
        "accept-edits" => &["--mode", "accept-edits", "--sandbox", "--dangerously-skip-permissions"],
        "dangerously-skip-permissions" => &["--mode", "accept-edits", "--dangerously-skip-permissions"],
        permission => return Err(format!("지원하지 않는 Antigravity 권한입니다: {permission}")),
    };
    Ok(arguments.iter().map(|argument| (*argument).to_string()).collect())
}

/// antigravityExecutionPrompt. 업무 폴더가 있으면 그 폴더 안에서만 명령을 실행하라고 덧붙인다.
pub fn antigravity_execution_prompt(character: &RuntimeCharacter, prompt: &str, workdir: Option<&str>) -> String {
    let resolved = js_trim(workdir.unwrap_or(""));
    let mut lines = vec![
        "<officestra_identity>".to_string(),
        configured_identity_prompt(character),
        "</officestra_identity>".into(),
        "<officestra_execution>".into(),
    ];
    if resolved.is_empty() {
        lines.push("현재 업무 폴더 안에서만 파일 작업을 수행합니다.".into());
    } else {
        lines.push(format!("업무 폴더는 {}입니다.", quoted(resolved)));
        lines.push(format!("명령을 실행할 때는 반드시 {}로 이동한 뒤 실행합니다.", quoted(resolved)));
    }
    lines.extend([
        "사용자 요청 범위를 벗어난 파일이나 서비스를 변경하지 않습니다.".into(),
        "</officestra_execution>".into(),
        "<user_request>".into(),
        prompt.to_string(),
        "</user_request>".into(),
    ]);
    lines.join("\n")
}

/// claudePersistentWorkerSignature. 캐시를 바꾸는 설정과 실행 폴더가 같을 때만 지속 세션을 다시 쓴다.
pub fn claude_persistent_worker_signature(character: &RuntimeCharacter, executable: &str, workdir: &str) -> String {
    let signature = json!({
        "executable": executable,
        "workdir": workdir,
        "model": character.model.as_deref().unwrap_or(""),
        "effort": character.effort,
        "fastMode": character.fast_mode,
        "permission": character.permission,
        "autoCompactPercent": normalize_auto_compact_percent(character.auto_compact_percent, 95.0),
        "identityPrompt": configured_identity_prompt(character),
    });
    String::from_utf8(to_js_json(&signature).unwrap_or_default()).unwrap_or_default()
}

/// executionEnvironment. Antigravity는 Playwright 경로, Claude는 자동 압축 기준과 자동 갱신 끄기를 넣는다.
/// 업무 폴더가 있으면 개발 도구 표시 주소·구조화 결과 경로를 넣고 결과 도구와 Node 폴더를 PATH 앞에 둔다.
pub fn execution_environment(
    character: &RuntimeCharacter,
    base: &Environment,
    workdir: Option<&str>,
    paths: &LaunchPaths,
) -> Environment {
    let mut environment = match character.backend.as_str() {
        "antigravity" => antigravity_playwright_environment(
            base,
            &paths.playwright_driver_directory,
            paths.node_executable.as_deref(),
        ),
        "claude" => {
            let mut environment = base.clone();
            environment.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
            environment.insert(
                "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".into(),
                normalize_auto_compact_percent(character.auto_compact_percent, 95.0).to_string(),
            );
            // CLI 갱신은 OFFICESTRA의 명시적 업데이트 화면에서만 한다. 업무 사이에 바뀌면 프롬프트 캐시가 깨진다.
            environment.insert("DISABLE_AUTOUPDATER".into(), "1".into());
            environment
        }
        _ => base.clone(),
    };
    let Some(workdir) = workdir.filter(|workdir| !workdir.is_empty()) else {
        return environment;
    };
    // Node String(character.id): id가 없으면 "undefined"다.
    let character_id = character.id.as_deref().unwrap_or("undefined");
    environment.extend(developer_tool_environment(character_id, base, "gui"));
    environment.insert(
        STRUCTURED_RESULT_ENV.into(),
        structured_turn_result_path(workdir, character.id.as_deref(), &paths.runtime_id),
    );
    let node_directory = paths
        .node_executable
        .as_deref()
        .map(|node| std::path::Path::new(node).parent().map_or(".".into(), |parent| parent.to_string_lossy().into_owned()));
    let path = prepend_path_entries(
        environment.get("PATH").map(String::as_str).unwrap_or(""),
        &[Some(paths.result_tool_directory.as_str()), node_directory.as_deref()],
    );
    environment.insert("PATH".into(), path);
    environment
}

pub(crate) fn prepend_path_entries(value: &str, entries: &[Option<&str>]) -> String {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .flatten()
        .copied()
        .chain(value.split(':'))
        .filter(|entry| !entry.is_empty() && seen.insert(*entry))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests;
