// 로컬(4090) 실행 프로필의 검증과 CLI 실행 명세. Node의 local-agent-profile.mjs를 옮겼다.
// 서버·터널·워커·DB는 건드리지 않는다. 앞문과 턴별 프로필 기록은 호출하는 쪽이 맡는다.

use std::ffi::CString;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::agent_runtime::attachments::StagedAttachment;
use crate::agent_runtime::launch::{
    Environment, LaunchPaths, RuntimeCharacter, build_arguments, claude_persistent_arguments, claude_persistent_worker_signature,
    execution_environment, normalize_auto_compact_percent, quoted,
};
use crate::js::{join_path, to_js_json};
use crate::local_model_capabilities::{DIRECT_RUNTIME, LocalProfileLike, direct_codex_reasoning, direct_reasoning_options, is_mero_mero};
use crate::terminal_sessions::launch::terminal_arguments;

const KEYS: [&str; 12] = [
    "id", "providerKind", "backend", "model", "endpoint", "credentialEnv", "credentialVersion",
    "contextWindow", "maxOutputTokens", "usageProtocol", "reasoning", "kvCacheQuantization",
];
const RUNTIME_KEY: &str = "runtime";
const QWEN_DIRECT_MODEL: &str = "officestra-qwen38-27b-uncensored-q4km";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalAgentProfile {
    pub id: String,
    pub provider_kind: &'static str,
    pub backend: String,
    pub model: String,
    pub endpoint: String,
    pub credential_env: String,
    pub credential_version: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub usage_protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache_quantization: Option<String>,
}

impl LocalProfileLike for LocalAgentProfile {
    fn model(&self) -> &str {
        &self.model
    }
    fn runtime(&self) -> Option<&str> {
        self.runtime.as_deref()
    }
    fn reasoning(&self) -> Option<&str> {
        self.reasoning.as_deref()
    }
}

/// 검증 중간 단계용. 아직 확정되지 않은 값으로 능력 함수를 부를 때 쓴다.
struct Draft<'a> {
    model: &'a str,
    runtime: Option<&'a str>,
    reasoning: Option<&'a str>,
}

impl LocalProfileLike for Draft<'_> {
    fn model(&self) -> &str {
        self.model
    }
    fn runtime(&self) -> Option<&str> {
        self.runtime
    }
    fn reasoning(&self) -> Option<&str> {
        self.reasoning
    }
}

fn text<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    match object.get(name).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() && !value.contains(['\r', '\n', '\0']) => Ok(value),
        _ => Err(format!("Invalid local profile {name}")),
    }
}

/// Node의 `value[name] !== undefined`에 해당한다. 키가 있으면 문자열이 아니어도(null 포함) 각 항목의 검사로 간다.
fn present<'a>(object: &'a Map<String, Value>, name: &str) -> (bool, Option<&'a str>) {
    match object.get(name) {
        None => (false, None),
        Some(value) => (true, value.as_str()),
    }
}

fn safe_integer(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    if number.fract() != 0.0 || number < 0.0 || number > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(number as u64)
}

pub fn normalize_local_agent_profile(value: &Value) -> Result<LocalAgentProfile, String> {
    let Some(object) = value.as_object() else {
        return Err("Invalid local profile fields".to_string());
    };
    if object.keys().any(|key| key != RUNTIME_KEY && !KEYS.contains(&key.as_str())) {
        return Err("Invalid local profile fields".to_string());
    }
    let backend = object.get("backend").and_then(Value::as_str).unwrap_or("");
    if object.get("providerKind").and_then(Value::as_str) != Some("local") || !["claude", "codex"].contains(&backend) {
        return Err("Only explicit local Claude or Codex runner profiles are supported".to_string());
    }
    let id = text(object, "id")?;
    let mut chars = id.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok || id.len() > 64 || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err("Invalid local profile ID".to_string());
    }
    let model = text(object, "model")?;
    let url = Url::parse(text(object, "endpoint")?).map_err(|_| "Invalid local profile endpoint".to_string())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1") | Some("[::1]"));
    if url.scheme() != "http"
        || !loopback
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Local endpoint must be a credential-free loopback bridge URL".to_string());
    }
    let credential_env = text(object, "credentialEnv")?;
    let env_body = credential_env.strip_prefix("OFFICESTRA_LOCAL_").and_then(|rest| rest.strip_suffix("_TOKEN"));
    if !env_body.is_some_and(|body| !body.is_empty() && body.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')) {
        return Err("Use a dedicated local credential environment reference".to_string());
    }
    let credential_version = text(object, "credentialVersion")?;
    let context_window = safe_integer(object.get("contextWindow"));
    let max_output_tokens = safe_integer(object.get("maxOutputTokens"));
    let (context_window, max_output_tokens) = match (context_window, max_output_tokens) {
        (Some(window @ (32768 | 65536)), Some(output)) if output >= 1 && output <= window => (window, output),
        _ => return Err("Local output budget must be a safe integer within the context window".to_string()),
    };
    let expected_usage_protocol = if backend == "codex" { "openai-responses-v1" } else { "anthropic-normalized-v1" };
    if object.get("usageProtocol").and_then(Value::as_str) != Some(expected_usage_protocol) {
        return Err(format!("Local {backend} profile requires {expected_usage_protocol}"));
    }
    let (runtime_present, runtime) = present(object, RUNTIME_KEY);
    if runtime_present && runtime != Some(DIRECT_RUNTIME) {
        return Err("Unsupported local runtime".to_string());
    }
    let (kv_present, kv) = present(object, "kvCacheQuantization");
    let draft = Draft { model, runtime, reasoning: None };
    let mero = is_mero_mero(&draft);
    if runtime.is_some()
        && !(kv == Some("q8_0")
            && (if mero { [32768, 65536].contains(&context_window) } else { context_window == 65536 && model == QWEN_DIRECT_MODEL }))
    {
        return Err("Direct runtime requires a verified model/context/KV8 configuration".to_string());
    }
    let (reasoning_present, reasoning) = present(object, "reasoning");
    if reasoning_present {
        let allowed: &[&str] = if runtime.is_some() { direct_reasoning_options(&draft) } else { &["default", "on", "off"] };
        if !reasoning.is_some_and(|reasoning| allowed.contains(&reasoning)) {
            return Err("Unsupported local reasoning option".to_string());
        }
    }
    if kv_present
        && !(kv == Some("q8_0")
            && (backend == "codex" || runtime == Some(DIRECT_RUNTIME))
            && (if mero { [32768, 65536].contains(&context_window) } else { context_window == 65536 }))
    {
        return Err("Only verified local 64K q8 KV profiles are supported".to_string());
    }
    Ok(LocalAgentProfile {
        id: id.to_string(),
        provider_kind: "local",
        backend: backend.to_string(),
        model: model.to_string(),
        endpoint: url.origin().ascii_serialization(),
        credential_env: credential_env.to_string(),
        credential_version: credential_version.to_string(),
        context_window,
        max_output_tokens,
        usage_protocol: expected_usage_protocol.to_string(),
        reasoning: reasoning.map(str::to_string),
        runtime: runtime.map(str::to_string),
        kv_cache_quantization: kv.map(str::to_string),
    })
}

fn invalid_text(value: &str) -> bool {
    value.contains(['\r', '\n', '\0'])
}

/// localClaudeOptions. 사용자 프롬프트와 정체 문구는 옵션처럼 보여도 값으로 둔다. 클라우드 effort 플래그는 뺀다.
fn local_claude_options(arguments: &[String], mode: &str) -> Vec<String> {
    const PAIRED: [&str; 7] = ["--settings", "--permission-mode", "--model", "--append-system-prompt", "--resume", "--output-format", "--input-format"];
    let mut result = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if (argument == "-p" && mode == "gui") || PAIRED.contains(&argument.as_str()) {
            result.push(argument.clone());
            result.extend(arguments.get(index + 1).cloned());
            index += 2;
            continue;
        }
        if argument == "--effort" {
            index += 2;
            continue;
        }
        if argument == "--prompt-suggestions" {
            result.extend([argument.clone(), "false".to_string()]);
            index += 2;
            continue;
        }
        result.push(argument.clone());
        index += 1;
    }
    result.extend(["--setting-sources".to_string(), String::new()]);
    result
}

const LOCAL_CODEX_PROVIDER: &str = "officestra_local";
const CODEX_CLOUD_CONFIG_KEYS: [&str; 7] =
    ["model", "model_reasoning_effort", "features.fast_mode", "service_tier", "model_reasoning_summary", "show_raw_agent_reasoning", "hide_agent_reasoning"];

/// localTerminalExecutable. SwiftTerm은 execve라 PATH를 찾지 않는다. 업무 폴더가 CLI를 가리지 못하게 상대 PATH 항목은 건너뛴다.
pub fn local_terminal_executable(executable: &str, environment: &Environment) -> Result<String, String> {
    if Path::new(executable).is_absolute() {
        return Ok(executable.to_string());
    }
    for directory in environment.get("PATH").map(String::as_str).unwrap_or("").split(':') {
        if !Path::new(directory).is_absolute() {
            continue;
        }
        let candidate = join_path(&[directory, executable]);
        let executable_bit = CString::new(candidate.as_str())
            // SAFETY: 널로 끝나는 경로 문자열을 넘기고 결과 코드만 읽는다.
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0);
        if executable_bit && std::fs::metadata(&candidate).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(candidate);
        }
    }
    Err(format!("Local terminal executable not found: {executable}"))
}

/// localCodexOptions. 클라우드 설정 키를 지우고 프로세스 안에서만 쓰는 Responses 제공자 설정을 붙인다.
fn local_codex_options(
    arguments: &[String],
    profile: &LocalAgentProfile,
    character: &RuntimeCharacter,
    mode: &str,
    catalog_path: Option<&str>,
    workdir: &str,
) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "-c" && index + 1 < arguments.len() {
            let config = &arguments[index + 1];
            let key = config.split('=').next().unwrap_or("");
            if !CODEX_CLOUD_CONFIG_KEYS.contains(&key) {
                result.extend(["-c".to_string(), config.clone()]);
            }
            index += 2;
            continue;
        }
        result.push(arguments[index].clone());
        index += 1;
    }
    let percent = normalize_auto_compact_percent(Some(character.auto_compact_percent.unwrap_or(100.0)), 100.0);
    let token_limit = (profile.context_window as f64 * percent as f64 / 100.0).floor();
    let direct = profile.runtime.as_deref() == Some(DIRECT_RUNTIME);
    let effort = if direct { direct_codex_reasoning(profile) } else { "default".to_string() };
    let provider = |key: &str, value: String| format!("model_providers.{LOCAL_CODEX_PROVIDER}.{key}={value}");
    let mut overrides = vec![
        format!("model={}", quoted(&profile.model)),
        format!("model_provider={}", quoted(LOCAL_CODEX_PROVIDER)),
        format!("model_reasoning_effort={}", quoted(&effort)),
        // 로컬 모델이 실제로 내는 추론만 보여 준다. llama.cpp 원문은 다리가 요약 칸으로 옮기므로 TUI에 두 번 찍지 않는다.
        format!("show_raw_agent_reasoning={}", if direct { "false" } else { "true" }),
        "hide_agent_reasoning=false".to_string(),
        "model_reasoning_summary=\"detailed\"".to_string(),
        "features.fast_mode=false".to_string(),
        format!("service_tier={}", quoted("default")),
        provider("name", quoted("OFFICESTRA Local")),
        provider("base_url", quoted(&format!("{}/v1", profile.endpoint))),
        provider("wire_api", quoted("responses")),
        provider("env_key", quoted(&profile.credential_env)),
        provider("requires_openai_auth", "false".to_string()),
        provider("supports_websockets", "false".to_string()),
        // 끊기거나 실패한 로컬 요청이 24GB GPU 작업을 몰래 다시 올리지 않게 한다. 다시 연결은 사용자의 새 요청 때만 한다.
        provider("request_max_retries", "0".to_string()),
        provider("stream_max_retries", "0".to_string()),
        format!("model_context_window={}", profile.context_window),
        format!("model_auto_compact_token_limit={token_limit}"),
        format!("model_auto_compact_token_limit_scope={}", quoted("total")),
    ];
    if let Some(path) = catalog_path {
        overrides.push(format!("model_catalog_json={}", quoted(path)));
    }
    let overrides = overrides.into_iter().flat_map(|config| ["-c".to_string(), config]);
    if mode != "gui" {
        result.extend(overrides);
        result.extend(["-C".to_string(), workdir.to_string()]);
        return result;
    }
    let prompt = result.pop();
    result.extend(overrides);
    result.extend(prompt);
    result
}

/// createLocalAgentLaunch 입력. Node 객체 인자의 기본값(executable=locateExecutable, baseEnvironment=process.env)은 호출하는 쪽이 채운다.
pub struct LocalLaunchInput<'a> {
    pub profile: &'a LocalAgentProfile,
    pub character: &'a RuntimeCharacter,
    pub mode: &'a str,
    pub prompt: &'a str,
    pub attachments: &'a [StagedAttachment],
    pub previous_session_id: Option<&'a str>,
    pub workdir: &'a str,
    pub base_environment: &'a Environment,
    /// 터미널 Codex notify가 부르는 훅 실행 파일.
    pub hook_path: &'a str,
    pub catalog_path: Option<&'a str>,
    pub executable: &'a str,
    pub paths: &'a LaunchPaths,
}

/// createLocalAgentLaunch 결과. 자격 증명 값은 env에만 있고 profile·args·signature에는 없다.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalAgentLaunch {
    pub executable: String,
    pub backend: String,
    pub provider_kind: &'static str,
    pub profile: LocalAgentProfile,
    pub args: Vec<String>,
    pub env: Environment,
    pub cwd: String,
    pub signature: String,
}

/// createLocalAgentLaunch. 직원 설정이 프로필과 맞을 때만 로컬 CLI 실행 명세를 만든다.
pub fn create_local_agent_launch(input: LocalLaunchInput<'_>) -> Result<LocalAgentLaunch, String> {
    let LocalLaunchInput { profile, character, mode, workdir, previous_session_id, .. } = input;
    let value = serde_json::to_value(profile).map_err(|error| error.to_string())?;
    let profile = normalize_local_agent_profile(&value)?;
    if character.backend != profile.backend || character.model.as_deref() != Some(profile.model.as_str()) {
        return Err("Character runner/model must match the local profile".to_string());
    }
    if character.fast_mode {
        return Err("Local Fast mode is not verified".to_string());
    }
    if character.effort != "default" {
        return Err("Local profile requires default effort; configurable effort is not verified".to_string());
    }
    let allowed: &[&str] =
        if profile.backend == "codex" { &["read-only", "workspace-write", "danger-full-access"] } else { &["plan", "auto", "acceptEdits", "bypassPermissions"] };
    if !allowed.contains(&character.permission.as_str()) {
        return Err(format!("Invalid {} permission", profile.backend));
    }
    if !workdir.starts_with('/') || invalid_text(workdir) {
        return Err("Absolute workdir required".to_string());
    }
    if !["gui", "terminal", "persistent"].contains(&mode) {
        return Err("Invalid local execution mode".to_string());
    }
    if profile.backend == "codex" && mode == "persistent" {
        return Err("Persistent local Codex workers are not supported".to_string());
    }
    if previous_session_id.is_some_and(|session| session.trim().is_empty() || invalid_text(session)) {
        return Err("Invalid resume session".to_string());
    }
    let credential = match input.base_environment.get(&profile.credential_env) {
        Some(credential) if !credential.is_empty() && !invalid_text(credential) => credential.clone(),
        _ => return Err("Local bridge credential is unavailable".to_string()),
    };
    let clean: Environment = input
        .base_environment
        .iter()
        .filter(|(key, _)| {
            !(["ANTHROPIC_", "CLAUDE_", "OPENAI_", "OFFICESTRA_LOCAL_"].iter().any(|prefix| key.starts_with(prefix))
                || ["DISABLE_AUTO_COMPACT", "DISABLE_COMPACT", "MAX_THINKING_TOKENS"].contains(&key.as_str()))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut env = execution_environment(character, &clean, Some(workdir), input.paths);
    env.remove("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE");
    let percent = normalize_auto_compact_percent(Some(character.auto_compact_percent.unwrap_or(100.0)), 100.0);
    if profile.backend == "claude" {
        let entries = [
            ("ANTHROPIC_BASE_URL", profile.endpoint.clone()),
            ("ANTHROPIC_AUTH_TOKEN", credential.clone()),
            ("ANTHROPIC_API_KEY", credential),
            ("CLAUDE_CODE_USE_GATEWAY", "1".into()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
            ("CLAUDE_CODE_MAX_RETRIES", "0".into()),
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", profile.context_window.to_string()),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", profile.max_output_tokens.to_string()),
            ("MAX_THINKING_TOKENS", "0".into()),
            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", percent.to_string()),
            // 로컬 모델은 긴 프롬프트를 몇 분 동안 읽은 뒤에야 응답 머리를 보낸다. SDK 제한과 첫 바이트·흐름 감시가 정상 생성을 끊지 않게 한다.
            // 취소는 OFFICESTRA가 자식 프로세스와 다리로 맡는다. 0은 즉시 시간 초과라 Node 타이머 최대값(약 24.8일)을 쓴다.
            ("API_TIMEOUT_MS", "2147483647".into()),
            ("CLAUDE_ENABLE_BYTE_WATCHDOG", "0".into()),
            ("CLAUDE_ENABLE_STREAM_WATCHDOG", "0".into()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", profile.model.clone()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", profile.model.clone()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", profile.model.clone()),
            ("DISABLE_TELEMETRY", "1".into()),
        ];
        env.extend(entries.into_iter().map(|(key, value)| (key.to_string(), value)));
    } else {
        env.insert(profile.credential_env.clone(), credential);
    }
    let arguments = match mode {
        "terminal" => terminal_arguments(character, previous_session_id, Some(workdir), input.hook_path)?,
        "persistent" => claude_persistent_arguments(character, previous_session_id),
        _ => build_arguments(character, input.prompt, previous_session_id, input.attachments, Some(workdir))?,
    };
    let args = if profile.backend == "codex" {
        local_codex_options(&arguments, &profile, character, mode, input.catalog_path, workdir)
    } else {
        local_claude_options(&arguments, mode)
    };
    // 비밀값 없는 워커 정체. 자격 증명을 바꾸면 credentialVersion도 바꿔야 한다. 값은 로그·서명에 넣지 않는다.
    let worker_identity = claude_persistent_worker_signature(character, input.executable, workdir);
    let identity = json!({ "profile": profile, "mode": mode, "workerIdentity": worker_identity, "autoCompactPercent": percent });
    let signature = Sha256::digest(to_js_json(&identity).map_err(|error| error.to_string())?).iter().map(|byte| format!("{byte:02x}")).collect();
    let executable = if mode == "terminal" { local_terminal_executable(input.executable, &env)? } else { input.executable.to_string() };
    Ok(LocalAgentLaunch { executable, backend: profile.backend.clone(), provider_kind: "local", profile, args, env, cwd: workdir.to_string(), signature })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile() -> Value {
        json!({
            "id": "local-4090", "providerKind": "local", "backend": "claude", "model": "officestra-local-smoke",
            "endpoint": "http://127.0.0.1:41235", "credentialEnv": "OFFICESTRA_LOCAL_4090_TOKEN",
            "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096,
            "usageProtocol": "anthropic-normalized-v1"
        })
    }

    fn with(base: Value, patch: Value) -> Value {
        let mut object = base.as_object().unwrap().clone();
        for (key, value) in patch.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        Value::Object(object)
    }

    fn codex_profile() -> Value {
        with(profile(), json!({ "id": "local-4090-codex", "backend": "codex", "usageProtocol": "openai-responses-v1" }))
    }

    #[test]
    fn 기본_프로필은_origin만_남기고_정규화한다() {
        let normalized = normalize_local_agent_profile(&with(profile(), json!({ "endpoint": "http://127.0.0.1:41235/" }))).unwrap();
        assert_eq!(normalized.endpoint, "http://127.0.0.1:41235");
        assert_eq!(normalized.kv_cache_quantization, None);
        assert_eq!(normalized.reasoning, None);
        let json = serde_json::to_value(&normalized).unwrap();
        assert_eq!(json["providerKind"], "local");
        assert!(json.get("runtime").is_none());
    }

    #[test]
    fn 잘못된_endpoint와_비밀_필드와_검증되지_않은_컨텍스트는_거절한다() {
        for endpoint in [
            "https://example.com",
            "http://localhost:1234",
            "http://127.0.0.1",
            "http://u:p@127.0.0.1:1234",
            "http://127.0.0.1:1234/?token=x",
        ] {
            assert!(normalize_local_agent_profile(&with(profile(), json!({ "endpoint": endpoint }))).is_err(), "{endpoint}");
        }
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "apiKey": "secret" }))).is_err());
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "contextWindow": 200000 }))).is_err());
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "usageProtocol": "raw-lmstudio" }))).is_err());
        assert!(normalize_local_agent_profile(&with(codex_profile(), json!({ "usageProtocol": "anthropic-normalized-v1" }))).is_err());
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "usageProtocol": "openai-responses-v1" }))).is_err());
    }

    #[test]
    fn 출력_한도는_컨텍스트_안에서만_허용한다() {
        assert_eq!(normalize_local_agent_profile(&with(codex_profile(), json!({ "maxOutputTokens": 16384 }))).unwrap().max_output_tokens, 16384);
        for max_output_tokens in [json!(0), json!(65537), json!(1.5), json!("8192")] {
            let error = normalize_local_agent_profile(&with(codex_profile(), json!({ "contextWindow": 65536, "maxOutputTokens": max_output_tokens }))).unwrap_err();
            assert!(error.contains("output budget"), "{error}");
        }
    }

    #[test]
    fn kv8은_codex나_직접_런타임의_64k에서만_허용하고_kv4는_거절한다() {
        let kv4 = with(codex_profile(), json!({ "contextWindow": 65536, "kvCacheQuantization": "q8_0" }));
        assert_eq!(normalize_local_agent_profile(&kv4).unwrap().kv_cache_quantization.as_deref(), Some("q8_0"));
        for invalid in [
            with(kv4.clone(), json!({ "backend": "claude", "usageProtocol": "anthropic-normalized-v1" })),
            with(kv4.clone(), json!({ "contextWindow": 32768 })),
            with(kv4.clone(), json!({ "kvCacheQuantization": "q4_0" })),
            with(kv4.clone(), json!({ "kvCacheQuantization": null })),
        ] {
            let error = normalize_local_agent_profile(&invalid).unwrap_err();
            assert!(error.contains("KV"), "{error}");
        }
    }

    #[test]
    fn 직접_런타임은_검증된_모델_컨텍스트_kv8만_받는다() {
        let direct = with(profile(), json!({
            "id": "claude-direct", "runtime": DIRECT_RUNTIME, "model": QWEN_DIRECT_MODEL,
            "contextWindow": 65536, "kvCacheQuantization": "q8_0"
        }));
        for reasoning in ["low", "medium", "xhigh"] {
            let normalized = normalize_local_agent_profile(&with(direct.clone(), json!({ "reasoning": reasoning }))).unwrap();
            assert_eq!(normalized.reasoning.as_deref(), Some(reasoning));
            assert_eq!(normalized.runtime.as_deref(), Some(DIRECT_RUNTIME));
        }
        assert!(normalize_local_agent_profile(&with(direct.clone(), json!({ "contextWindow": 32768 }))).unwrap_err().contains("Direct runtime"));
        assert!(normalize_local_agent_profile(&with(direct.clone(), json!({ "kvCacheQuantization": "q4_0" }))).unwrap_err().contains("Direct runtime"));
        assert!(normalize_local_agent_profile(&with(direct, json!({ "reasoning": "on" }))).unwrap_err().contains("reasoning"));
        // 브리지 프로필의 추론 값은 default·on·off만 허용한다.
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "reasoning": "on" }))).is_ok());
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "reasoning": "high" }))).is_err());
        assert!(normalize_local_agent_profile(&with(profile(), json!({ "runtime": "other" }))).unwrap_err().contains("runtime"));
    }

    const SECRET: &str = "test-local-secret";

    fn character() -> RuntimeCharacter {
        RuntimeCharacter {
            id: Some("isolated-validation".into()),
            name: String::new(),
            seat: String::new(),
            backend: "claude".into(),
            model: Some("officestra-local-smoke".into()),
            effort: "default".into(),
            fast_mode: false,
            auto_compact_percent: Some(30.0),
            permission: "plan".into(),
            identity_prompt: "Keep the identity instructions.".into(),
            config: json!({}),
            local_profile: None,
        }
    }

    fn codex_character() -> RuntimeCharacter {
        RuntimeCharacter { backend: "codex".into(), permission: "danger-full-access".into(), ..character() }
    }

    fn base() -> Environment {
        [
            ("PATH", "/usr/bin"),
            ("OFFICESTRA_LOCAL_4090_TOKEN", SECRET),
            ("OFFICESTRA_LOCAL_OTHER_TOKEN", "other-secret"),
            ("ANTHROPIC_API_KEY", "cloud-secret"),
            ("ANTHROPIC_BASE_URL", "https://cloud.example"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "40"),
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "200000"),
            ("DISABLE_AUTO_COMPACT", "1"),
            ("DISABLE_COMPACT", "1"),
            ("OPENAI_API_KEY", "openai-secret"),
            ("CODEX_HOME", "/tmp/shared-codex-home"),
            ("OFFICESTRA_PLUGIN_MARKER", "preserved"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    /// Node 시험의 build/buildCodex 기본 인자.
    struct Build {
        profile: Value,
        character: RuntimeCharacter,
        mode: &'static str,
        prompt: String,
        attachments: Vec<StagedAttachment>,
        previous: Option<String>,
        workdir: String,
        base: Environment,
        executable: String,
    }

    fn run(build: Build) -> Result<LocalAgentLaunch, String> {
        // Node는 createLocalAgentLaunch 안에서 정규화한다. 잘못된 프로필은 같은 오류로 실패한다.
        let profile = normalize_local_agent_profile(&build.profile)?;
        create_local_agent_launch(LocalLaunchInput {
            profile: &profile,
            character: &build.character,
            mode: build.mode,
            prompt: &build.prompt,
            attachments: &build.attachments,
            previous_session_id: build.previous.as_deref(),
            workdir: &build.workdir,
            base_environment: &build.base,
            hook_path: "/tmp/test-hook",
            catalog_path: None,
            executable: &build.executable,
            paths: &LaunchPaths::default(),
        })
    }

    fn build(change: impl FnOnce(&mut Build)) -> Result<LocalAgentLaunch, String> {
        let mut input = Build {
            profile: profile(),
            character: character(),
            mode: "gui",
            prompt: String::new(),
            attachments: Vec::new(),
            previous: None,
            workdir: "/tmp/local-validation".into(),
            base: base(),
            executable: "/usr/local/bin/claude".into(),
        };
        change(&mut input);
        run(input)
    }

    fn build_codex(change: impl FnOnce(&mut Build)) -> Result<LocalAgentLaunch, String> {
        build(|input| {
            input.profile = codex_profile();
            input.character = codex_character();
            input.executable = "/usr/local/bin/codex".into();
            change(input);
        })
    }

    fn after<'a>(arguments: &'a [String], flag: &str) -> &'a str {
        let index = arguments.iter().position(|argument| argument == flag).unwrap_or_else(|| panic!("{flag} 없음"));
        &arguments[index + 1]
    }

    fn config_arguments(arguments: &[String]) -> std::collections::BTreeMap<String, String> {
        let mut values = std::collections::BTreeMap::new();
        let mut index = 0;
        while index < arguments.len() {
            if arguments[index] == "-c" {
                index += 1;
                let config = &arguments[index];
                let split = config.find('=').unwrap();
                values.insert(config[..split].to_string(), config[split + 1..].to_string());
            }
            index += 1;
        }
        values
    }

    fn attachment(path: &str, is_codex_image: bool) -> StagedAttachment {
        StagedAttachment { name: String::new(), path: path.into(), directory: String::new(), is_codex_image }
    }

    #[test]
    fn 로컬_codex_gui는_새_턴과_이어가는_턴_모두_그림을_인자로_붙인다() {
        for previous in [None, Some("existing-session")] {
            let launch = build_codex(|input| {
                input.previous = previous.map(str::to_string);
                input.prompt = "첨부를 분석해줘".into();
                input.attachments =
                    vec![attachment("/tmp/그림 1.png", true), attachment("/tmp/document.pdf", false), attachment("/tmp/그림 2.jpeg", true)];
            })
            .unwrap();
            let args = &launch.args;
            let images: Vec<&str> = args.iter().enumerate().filter(|(_, value)| *value == "-i").map(|(index, _)| args[index + 1].as_str()).collect();
            assert_eq!(images, ["/tmp/그림 1.png", "/tmp/그림 2.jpeg"]);
            assert_eq!(args.last().unwrap(), "첨부를 분석해줘");
            // -i는 여러 값을 받는다. 뒤에 플래그가 와야 마지막 프롬프트가 그림 이름으로 먹히지 않는다.
            let last = args.iter().rposition(|value| value == "-i").unwrap();
            assert_eq!(args[last + 2], "-c");
        }
    }

    #[test]
    fn 터미널_실행_파일은_절대경로이고_상대_path는_보지_않는다() {
        let path = |value: &str| -> Environment { [("PATH".to_string(), value.to_string())].into_iter().collect() };
        assert_eq!(local_terminal_executable("/test/codex", &Environment::new()).unwrap(), "/test/codex");
        assert_eq!(local_terminal_executable("true", &path(".:/usr/bin")).unwrap(), "/usr/bin/true");
        assert!(local_terminal_executable("codex", &path(".:relative")).unwrap_err().contains("not found"));
    }

    fn direct_claude() -> Value {
        with(profile(), json!({ "id": "claude-direct", "runtime": DIRECT_RUNTIME, "model": QWEN_DIRECT_MODEL, "contextWindow": 65536, "kvCacheQuantization": "q8_0" }))
    }

    #[test]
    fn 직접_claude는_gui와_터미널에서_고정_서버를_쓴다() {
        for mode in ["gui", "terminal"] {
            for reasoning in ["low", "medium", "xhigh"] {
                let launch = build(|input| {
                    input.mode = mode;
                    input.profile = with(direct_claude(), json!({ "reasoning": reasoning }));
                    input.character.model = Some(QWEN_DIRECT_MODEL.into());
                })
                .unwrap();
                assert_eq!(launch.profile.reasoning.as_deref(), Some(reasoning));
                assert_eq!(launch.profile.backend, "claude");
                assert_eq!(launch.env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "65536");
                assert_eq!(launch.env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "4096");
                assert!(!launch.args.iter().any(|argument| argument == "--effort"));
                assert!(launch.args.iter().any(|argument| argument == "--setting-sources"));
            }
        }
    }

    #[test]
    fn codex_64k_kv8은_두_실행_방식에서_유지된다() {
        let kv8 = with(codex_profile(), json!({ "contextWindow": 65536, "kvCacheQuantization": "q8_0" }));
        for mode in ["gui", "terminal"] {
            let launch = build_codex(|input| {
                input.mode = mode;
                input.profile = kv8.clone();
            })
            .unwrap();
            assert_eq!(launch.profile.kv_cache_quantization.as_deref(), Some("q8_0"));
            assert!(launch.args.iter().any(|argument| argument == "model_context_window=65536"));
            let plain = build_codex(|input| {
                input.mode = mode;
                input.profile = with(codex_profile(), json!({ "contextWindow": 65536 }));
            })
            .unwrap();
            assert_ne!(launch.signature, plain.signature);
        }
    }

    #[test]
    fn 직접_qwen_codex는_요약을_한_번만_보이고_선택한_추론을_지킨다() {
        let direct = with(codex_profile(), json!({ "model": QWEN_DIRECT_MODEL, "runtime": DIRECT_RUNTIME, "contextWindow": 65536, "kvCacheQuantization": "q8_0" }));
        for mode in ["gui", "terminal"] {
            for reasoning in ["low", "medium", "xhigh"] {
                let launch = build_codex(|input| {
                    input.mode = mode;
                    input.profile = with(direct.clone(), json!({ "reasoning": reasoning }));
                    input.character.model = Some(QWEN_DIRECT_MODEL.into());
                })
                .unwrap();
                let configs = config_arguments(&launch.args);
                assert_eq!(configs["show_raw_agent_reasoning"], "false");
                assert_eq!(configs["hide_agent_reasoning"], "false");
                assert_eq!(configs["model_reasoning_summary"], "\"detailed\"");
                assert_eq!(configs["model_reasoning_effort"], quoted(reasoning));
            }
        }
    }

    #[test]
    fn 추론_선택은_프로필과_워커_정체에_고정된다() {
        for mode in ["gui", "terminal", "persistent"] {
            let pick = |reasoning: &str| {
                build(|input| {
                    input.mode = mode;
                    input.profile = with(profile(), json!({ "reasoning": reasoning }));
                })
            };
            let (on, off) = (pick("on").unwrap(), pick("off").unwrap());
            assert_eq!(on.profile.reasoning.as_deref(), Some("on"));
            assert_eq!(off.profile.reasoning.as_deref(), Some("off"));
            assert_ne!(on.signature, off.signature);
            assert!(pick("high").is_err());
        }
    }

    #[test]
    fn 실행_64k는_측정한_창을_쓰고_압축과_도구를_끄지_않는다() {
        for mode in ["gui", "terminal"] {
            let original = build(|input| {
                input.mode = mode;
                input.previous = Some("preserved-session".into());
            })
            .unwrap();
            let larger = build(|input| {
                input.mode = mode;
                input.previous = Some("preserved-session".into());
                input.profile = with(profile(), json!({ "contextWindow": 65536 }));
            })
            .unwrap();
            assert_eq!(larger.profile.context_window, 65536);
            assert_eq!(larger.env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "65536");
            assert_eq!(larger.env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "4096");
            assert!(!larger.env.contains_key("DISABLE_AUTO_COMPACT"));
            assert_eq!(larger.env["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "30");
            assert_eq!(larger.args, original.args);
            assert_ne!(larger.signature, original.signature);
        }
    }

    #[test]
    fn codex는_프로세스_안의_responses_제공자_설정만_쓴다() {
        for mode in ["gui", "terminal"] {
            let launch = build_codex(|input| {
                input.mode = mode;
                input.profile = with(codex_profile(), json!({ "contextWindow": 65536 }));
            })
            .unwrap();
            let configs = config_arguments(&launch.args);
            assert_eq!(launch.backend, "codex");
            assert_eq!(launch.profile.usage_protocol, "openai-responses-v1");
            assert_eq!(launch.profile.max_output_tokens, 4096);
            let provider = |key: &str| configs[&format!("model_providers.officestra_local.{key}")].clone();
            assert_eq!(configs["model"], quoted("officestra-local-smoke"));
            assert_eq!(configs["model_provider"], quoted("officestra_local"));
            assert_eq!(configs["model_reasoning_effort"], quoted("default"));
            assert_eq!(configs["features.fast_mode"], "false");
            assert_eq!(configs["service_tier"], quoted("default"));
            assert_eq!(provider("name"), quoted("OFFICESTRA Local"));
            assert_eq!(provider("base_url"), quoted("http://127.0.0.1:41235/v1"));
            assert_eq!(provider("wire_api"), quoted("responses"));
            assert_eq!(provider("env_key"), quoted("OFFICESTRA_LOCAL_4090_TOKEN"));
            assert_eq!(provider("requires_openai_auth"), "false");
            assert_eq!(provider("supports_websockets"), "false");
            assert_eq!(provider("request_max_retries"), "0");
            assert_eq!(provider("stream_max_retries"), "0");
            assert_eq!(configs["model_context_window"], "65536");
            assert_eq!(configs["model_auto_compact_token_limit"], "19660");
            assert_eq!(configs["model_auto_compact_token_limit_scope"], quoted("total"));
            assert_eq!(configs["model_reasoning_summary"], "\"detailed\"");
            assert_eq!(configs["show_raw_agent_reasoning"], "true");
            assert_eq!(configs["hide_agent_reasoning"], "false");
            assert!(!launch.args.iter().any(|argument| argument == "--ignore-user-config"));
            assert!(configs.contains_key("developer_instructions"));
            if mode == "terminal" {
                assert!(configs.contains_key("notify"));
                assert_eq!(after(&launch.args, "-C"), "/tmp/local-validation");
            }
        }
    }

    #[test]
    fn codex_로컬_자격은_격리되고_공유_홈과_플러그인은_남는다() {
        let launch = build_codex(|_| {}).unwrap();
        assert_eq!(launch.env["OFFICESTRA_LOCAL_4090_TOKEN"], SECRET);
        for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OFFICESTRA_LOCAL_OTHER_TOKEN"] {
            assert!(!launch.env.contains_key(key), "{key}");
        }
        assert_eq!(launch.env["CODEX_HOME"], "/tmp/shared-codex-home");
        assert_eq!(launch.env["OFFICESTRA_PLUGIN_MARKER"], "preserved");
        let visible = format!("{:?}{:?}{}", launch.profile, launch.args, launch.signature);
        assert!(!visible.contains(SECRET));
    }

    #[test]
    fn codex_gui와_터미널은_원래_새_턴과_이어가기_인자_모양을_쓴다() {
        let gui = build_codex(|input| input.prompt = "--config-looking-user-prompt".into()).unwrap();
        assert_eq!(gui.args[0], "exec");
        assert_eq!(gui.args.last().unwrap(), "--config-looking-user-prompt");
        assert_eq!(gui.args.iter().filter(|value| *value == "--config-looking-user-prompt").count(), 1);
        let resumed = build_codex(|input| input.previous = Some("codex-session".into())).unwrap();
        assert_eq!(resumed.args[..4], ["exec", "resume", "codex-session", "--json"]);
        let terminal = build_codex(|input| input.mode = "terminal").unwrap();
        assert_ne!(terminal.args[0], "exec");
        let resumed_terminal = build_codex(|input| {
            input.mode = "terminal";
            input.previous = Some("codex-session".into());
        })
        .unwrap();
        assert_eq!(resumed_terminal.args[..2], ["resume", "codex-session"]);
    }

    #[test]
    fn 지속_codex와_맞지_않는_계약은_닫힌_채_실패한다() {
        assert!(build_codex(|input| input.mode = "persistent").unwrap_err().contains("Persistent local Codex"));
        assert!(build_codex(|input| input.character.permission = "plan".into()).is_err());
        assert!(build_codex(|input| input.character.backend = "claude".into()).is_err());
        assert!(build_codex(|input| input.character.effort = "high".into()).is_err());
        assert!(build_codex(|input| input.character.fast_mode = true).is_err());
    }

    #[test]
    fn 세_실행_방식은_권한_정체_로컬_기본값을_지킨다() {
        for mode in ["gui", "persistent", "terminal"] {
            for previous in [None, Some("test-session")] {
                let launch = build(|input| {
                    input.mode = mode;
                    input.previous = previous.map(str::to_string);
                })
                .unwrap();
                let env = &launch.env;
                assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:41235");
                assert_eq!(env["CLAUDE_CODE_USE_GATEWAY"], "1");
                assert_eq!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "32768");
                assert_eq!(env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "4096");
                assert_eq!(env["API_TIMEOUT_MS"], "2147483647");
                assert_eq!(env["CLAUDE_ENABLE_BYTE_WATCHDOG"], "0");
                assert_eq!(env["CLAUDE_ENABLE_STREAM_WATCHDOG"], "0");
                assert_eq!(env["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "30");
                assert!(!env.contains_key("DISABLE_AUTO_COMPACT"));
                assert!(!env.contains_key("DISABLE_COMPACT"));
                let args = &launch.args;
                assert!(!args.iter().any(|argument| argument == "--effort"));
                assert_eq!(after(args, "--permission-mode"), "plan");
                assert!(args.iter().any(|argument| argument.contains("Keep the identity instructions.")));
                assert_eq!(args.iter().any(|argument| argument == "--resume"), previous.is_some());
                if let Some(previous) = previous {
                    assert_eq!(after(args, "--resume"), previous);
                }
                assert_eq!(args.iter().any(|argument| argument == "-p"), mode != "terminal");
                let settings: Value = serde_json::from_str(after(args, "--settings")).unwrap();
                assert_eq!(settings["fastMode"], false);
                if mode == "terminal" {
                    assert!(settings["hooks"]["UserPromptSubmit"].is_array());
                    assert!(settings["hooks"]["Stop"].is_array());
                    assert!(settings["statusLine"].is_object());
                } else {
                    assert_eq!(after(args, "--prompt-suggestions"), "false");
                }
            }
        }
    }

    #[test]
    fn 자격은_자식_환경에만_있고_부모와_다른_제공자는_그대로다() {
        let (before, original) = (base(), character());
        let launch = build(|_| {}).unwrap();
        assert_eq!(launch.env["ANTHROPIC_API_KEY"], SECRET);
        for key in ["CLAUDE_CODE_USE_BEDROCK", "OPENAI_API_KEY", "OFFICESTRA_LOCAL_OTHER_TOKEN", "OFFICESTRA_LOCAL_4090_TOKEN"] {
            assert!(!launch.env.contains_key(key), "{key}");
        }
        assert_eq!(base(), before);
        assert_eq!(character(), original);
        let visible = format!("{:?}{:?}{}", launch.profile, launch.args, launch.signature);
        assert!(!visible.contains(SECRET));
    }

    #[test]
    fn 프로필이_바뀌면_워커_서명이_바뀌고_비밀값은_섞이지_않는다() {
        let original = build(|_| {}).unwrap().signature;
        for patch in [
            json!({ "credentialVersion": "v2" }),
            json!({ "endpoint": "http://127.0.0.1:41236" }),
            json!({ "id": "local-second" }),
            json!({ "credentialEnv": "OFFICESTRA_LOCAL_OTHER_TOKEN" }),
        ] {
            assert_ne!(build(|input| input.profile = with(profile(), patch.clone())).unwrap().signature, original, "{patch}");
        }
        assert_ne!(build(|input| input.workdir = "/tmp/other".into()).unwrap().signature, original);
        assert_ne!(build(|input| input.executable = "/other/claude".into()).unwrap().signature, original);
        assert_ne!(build(|input| input.character.identity_prompt = "changed".into()).unwrap().signature, original);
        let rotated = build(|input| {
            input.base.insert("OFFICESTRA_LOCAL_4090_TOKEN".into(), "rotated".into());
        })
        .unwrap();
        assert_eq!(rotated.signature, original);
    }

    #[test]
    fn 자격이_없거나_직원_설정이_다르면_닫힌_채_실패한다() {
        assert!(build(|input| input.base = Environment::new()).is_err());
        assert!(build(|input| input.character.backend = "codex".into()).is_err());
        assert!(build(|input| input.character.model = Some("other".into())).is_err());
        assert!(build(|input| input.character.effort = "high".into()).is_err());
        assert!(build(|input| input.character.fast_mode = true).is_err());
    }

    #[test]
    fn 플래그처럼_보이는_프롬프트와_정체_문구는_그대로_남는다() {
        for prompt in ["--effort", "--prompt-suggestions", "--settings", "--model"] {
            let launch = build(|input| {
                input.prompt = prompt.into();
                input.character.identity_prompt = "--effort".into();
            })
            .unwrap();
            assert_eq!(launch.args[0], "-p");
            assert_eq!(launch.args[1], prompt);
            assert_eq!(after(&launch.args, "--permission-mode"), "plan");
            assert!(after(&launch.args, "--append-system-prompt").starts_with("--effort"));
        }
    }

    #[test]
    fn 로컬_100퍼센트는_claude와_codex의_gui와_터미널에_닿는다() {
        assert_eq!(normalize_auto_compact_percent(Some(100.0), 95.0), 95);
        assert_eq!(normalize_auto_compact_percent(Some(100.0), 100.0), 100);
        for backend in ["claude", "codex"] {
            for mode in ["gui", "terminal"] {
                let usage = if backend == "claude" { "anthropic-normalized-v1" } else { "openai-responses-v1" };
                let full = json!({
                    "id": "full-context", "providerKind": "local", "backend": backend, "model": "test-model",
                    "endpoint": "http://127.0.0.1:40001", "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1",
                    "contextWindow": 65536, "maxOutputTokens": 8192, "usageProtocol": usage
                });
                let launch_with = |percent: Option<f64>| {
                    build(|input| {
                        input.profile = full.clone();
                        input.mode = mode;
                        input.executable = format!("/test/{backend}");
                        input.workdir = "/tmp".into();
                        input.base = [("OFFICESTRA_LOCAL_TEST_TOKEN".to_string(), "test".to_string())].into_iter().collect();
                        input.character = RuntimeCharacter {
                            id: Some("test".into()),
                            backend: backend.into(),
                            model: Some("test-model".into()),
                            permission: if backend == "claude" { "plan".into() } else { "read-only".into() },
                            auto_compact_percent: percent,
                            ..character()
                        };
                    })
                    .unwrap()
                };
                let launch = launch_with(Some(100.0));
                let fallback = launch_with(None);
                for launch in [&launch, &fallback] {
                    if backend == "claude" {
                        assert_eq!(launch.env["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "100");
                    } else {
                        assert!(launch.args.iter().any(|argument| argument == "model_auto_compact_token_limit=65536"));
                    }
                }
                assert_ne!(launch.signature, launch_with(Some(95.0)).signature);
            }
        }
    }

    #[test]
    fn 메로메로는_두_실행기와_gui_터미널에서_선택한_모델과_추론으로_뜬다() {
        use crate::local_model_capabilities::MEROMERO_MODEL_ID;
        for context_window in [32768, 65536] {
            for backend in ["claude", "codex"] {
                for mode in ["gui", "terminal"] {
                    for reasoning in ["default", "off", "on"] {
                        let usage = if backend == "codex" { "openai-responses-v1" } else { "anthropic-normalized-v1" };
                        let launch = build(|input| {
                            input.profile = json!({
                                "id": "mero-test", "providerKind": "local", "backend": backend, "runtime": DIRECT_RUNTIME,
                                "model": MEROMERO_MODEL_ID, "endpoint": "http://127.0.0.1:41235", "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN",
                                "credentialVersion": "v1", "contextWindow": context_window, "maxOutputTokens": 8192, "usageProtocol": usage,
                                "kvCacheQuantization": "q8_0", "reasoning": reasoning
                            });
                            input.mode = mode;
                            input.workdir = "/tmp".into();
                            input.prompt = "hello".into();
                            input.executable = "/usr/bin/true".into();
                            input.base = [("OFFICESTRA_LOCAL_TEST_TOKEN".to_string(), "test-token".to_string())].into_iter().collect();
                            input.character = RuntimeCharacter {
                                backend: backend.into(),
                                model: Some(MEROMERO_MODEL_ID.into()),
                                permission: if backend == "codex" { "read-only".into() } else { "plan".into() },
                                auto_compact_percent: None,
                                ..character()
                            };
                        })
                        .unwrap();
                        let quoted_model = format!("model=\"{MEROMERO_MODEL_ID}\"");
                        assert!(launch.args.iter().any(|argument| argument == MEROMERO_MODEL_ID || *argument == quoted_model));
                        if backend == "codex" {
                            let effort = format!("model_reasoning_effort=\"{}\"", if reasoning == "on" { "medium" } else { "none" });
                            assert!(launch.args.contains(&effort));
                        } else {
                            assert_eq!(launch.env["ANTHROPIC_DEFAULT_SONNET_MODEL"], MEROMERO_MODEL_ID);
                            assert_eq!(launch.env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], context_window.to_string());
                        }
                    }
                }
            }
        }
    }
}
